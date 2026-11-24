//! WASI workloads (`--platform wasi/wasm`) run in this process, on wasmtime,
//! instead of in the guest.
//!
//! A module does not need a Linux kernel, and the image blobs are already on
//! the host, so a Mac does not have to wake the VM. The guest agent stays the
//! small PID 1 it is: wasmtime is far larger than that binary. Output and the
//! exit go through the same log and `Event::Exited` path as a Linux container,
//! which is what `docker logs`, `docker wait` and `--rm` already use.
//!
//! Preview 1 core modules and preview 2 command components both run. A
//! component that exports `wasi:http/incoming-handler` is served by
//! [`super::wasm_http`] instead of running once. A container port can name
//! a different module with the label `dev.kiri.wasm.port.<port>`: image
//! `config.Labels` are the base, and the same key on the container replaces
//! that port. A port with no label keeps the entrypoint. There is no cgroup.
//!
//! `wasi:http/outgoing-handler` dials from this process. On a Mac that is
//! the Mac's own network: `127.0.0.1` is the Mac's loopback, and a name is
//! resolved here, not inside the VM. Container addresses that exist only
//! in the VM are not reachable. HTTPS trusts the CA set compiled into the
//! daemon, and the module's `HTTP_PROXY` is not applied. Incoming request
//! and response bodies are streamed, each capped by
//! `KIRI_WASM_HTTP_BODY_LIMIT` (32 MiB unless set). A preview 1 core
//! module has no `wasi:http`.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use kiri_proto::Stream;

/// A directory the module sees, at a WASI preopen (fd 3 and up for preview 1).
#[derive(Debug, Clone)]
pub(super) struct Preopen {
    pub host: PathBuf,
    pub guest: String,
    pub read_only: bool,
}

/// `wasi/wasm`, `wasip1/wasm32` and the same with `wasip2` or `wasm64`.
/// A Linux image is not one of these, even when its architecture string
/// contains the letters.
pub(super) fn is_wasm_platform(platform: &str) -> bool {
    let Some((os, arch)) = platform.split_once('/') else {
        return false;
    };
    matches!(os, "wasi" | "wasip1" | "wasip2") && matches!(arch, "wasm" | "wasm32" | "wasm64")
}

struct Active {
    engine: wasmtime::Engine,
    killed: Arc<AtomicU32>,
    stdin: Option<File>,
}

/// One running module. `docker stop` interrupts it; stdin arrives here so
/// the engine never asks the VM for a pipe.
static ACTIVE: LazyLock<Mutex<HashMap<String, Active>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn active() -> std::sync::MutexGuard<'static, HashMap<String, Active>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// `signal` is what `docker stop` / `docker kill` asked for. The module's
/// exit becomes `128 + signal` unless it already called `proc_exit`.
pub(super) fn interrupt(id: &str, signal: u32) {
    {
        let guard = active();
        if let Some(slot) = guard.get(id) {
            slot.killed.store(signal, Ordering::SeqCst);
            slot.engine.increment_epoch();
        }
    }
    // A wasi:http container is a thread blocked in accept, not in wasm.
    super::wasm_http::shutdown(id);
}

pub(super) fn write_stdin(id: &str, data: &[u8]) -> io::Result<()> {
    let mut guard = active();
    let Some(slot) = guard.get_mut(id) else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "container has exited",
        ));
    };
    let Some(stdin) = slot.stdin.as_mut() else {
        return Ok(());
    };
    stdin.write_all(data)
}

pub(super) fn close_stdin(id: &str) {
    if let Some(slot) = active().get_mut(id) {
        slot.stdin.take();
    }
}

pub(super) fn unregister(id: &str) {
    active().remove(id);
    super::wasm_http::forget(id);
}

pub(super) fn register(
    id: &str,
    engine: wasmtime::Engine,
    killed: Arc<AtomicU32>,
    stdin: Option<File>,
) {
    active().insert(
        id.to_owned(),
        Active {
            engine,
            killed,
            stdin,
        },
    );
}

/// Drops the registration when the module's thread ends, and not before the
/// exit is reported: a restart can register the same id only after this.
pub(super) struct Leave(pub(super) String);

impl Drop for Leave {
    fn drop(&mut self) {
        unregister(&self.0);
    }
}

/// Applies one image layer onto `dest`, bottom to top as the caller walks them.
/// A gzip or plain tar is unpacked. A raw `\0asm` blob is written as the
/// entrypoint file. zstd layers are refused: the store can hold them, and
/// saying so is better than a garbled module.
pub(super) fn unpack_layer(
    bytes: &[u8],
    media_type: &str,
    dest: &Path,
    entrypoint: &str,
) -> io::Result<()> {
    if media_type.contains("zstd") || bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zstd wasm layers are not supported yet",
        ));
    }
    if bytes.starts_with(b"\0asm") {
        let name = entrypoint.trim_start_matches('/');
        let name = if name.is_empty() { "module.wasm" } else { name };
        let Some(path) = safe_join(dest, name) else {
            return Err(io::Error::other("wasm entrypoint escapes the rootfs"));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        return Ok(());
    }
    let reader: Box<dyn Read> = if bytes.starts_with(&[0x1f, 0x8b]) || media_type.contains("gzip") {
        Box::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)))
    } else {
        Box::new(std::io::Cursor::new(bytes))
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(false);
    archive.set_unpack_xattrs(false);
    archive.unpack(dest)?;
    Ok(())
}

/// The entrypoint file, or the only `.wasm` file when the entrypoint misses.
/// `dev.kiri.wasm.port.<container-port>`. The value is a module path, the
/// same shape as the image `Entrypoint`. OCI wasm images and the wasm
/// runtimes declare one entrypoint (and annotations that only choose the
/// runtime), so a port map lives in the label family Kiri already uses.
pub(super) const WASM_PORT_LABEL: &str = "dev.kiri.wasm.port.";

/// One module the container may serve. `ports` are the container ports
/// whose label names this path. Empty means the entrypoint, which serves
/// every port that names nothing else.
pub(super) struct WasmModule {
    pub path: String,
    pub bytes: Vec<u8>,
    pub ports: Vec<u16>,
    pub entry: bool,
}

/// `config.Labels` from an image config blob. Missing or not an object is
/// no labels, which is every image that does not name ports.
pub(super) fn config_labels(config: &[u8]) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(config)
        .ok()
        .and_then(|v| v.get("config")?.get("Labels").cloned())
        .filter(|v| v.is_object())
        .unwrap_or(serde_json::Value::Null)
}

/// Container ports that name their own module. Image labels are the base.
/// A container label with the same key replaces that port.
pub(super) fn port_modules(
    image_labels: &serde_json::Value,
    container_labels: &serde_json::Value,
) -> io::Result<BTreeMap<u16, String>> {
    let mut out = BTreeMap::new();
    apply_port_labels(&mut out, image_labels)?;
    apply_port_labels(&mut out, container_labels)?;
    Ok(out)
}

fn apply_port_labels(
    out: &mut BTreeMap<u16, String>,
    labels: &serde_json::Value,
) -> io::Result<()> {
    let Some(obj) = labels.as_object() else {
        return Ok(());
    };
    for (key, value) in obj {
        let Some(rest) = key.strip_prefix(WASM_PORT_LABEL) else {
            continue;
        };
        let port: u16 = rest.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad wasm port label {key}"),
            )
        })?;
        if port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad wasm port label {key}"),
            ));
        }
        let Some(path) = value
            .as_str()
            .map(str::trim)
            .filter(|p| !p.trim_start_matches('/').is_empty())
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{key} needs a module path"),
            ));
        };
        out.insert(port, path.to_owned());
    }
    Ok(())
}

fn module_key(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// The entrypoint, then each other module a port label names. Two ports
/// that name the same path share one entry.
pub(super) fn load_modules(
    root: &Path,
    entry: &str,
    routes: &BTreeMap<u16, String>,
) -> io::Result<Vec<WasmModule>> {
    let entry_key = module_key(entry);
    let mut entry_ports = Vec::new();
    let mut extras: BTreeMap<String, (String, Vec<u16>)> = BTreeMap::new();
    for (port, path) in routes {
        if module_key(path) == entry_key {
            entry_ports.push(*port);
        } else {
            extras
                .entry(module_key(path).to_owned())
                .or_insert_with(|| (path.clone(), Vec::new()))
                .1
                .push(*port);
        }
    }
    entry_ports.sort_unstable();
    let mut out = vec![WasmModule {
        path: entry.to_owned(),
        bytes: module_bytes(root, entry)?,
        ports: entry_ports,
        entry: true,
    }];
    for (_, (path, mut ports)) in extras {
        ports.sort_unstable();
        out.push(WasmModule {
            path: path.clone(),
            bytes: module_bytes(root, &path)?,
            ports,
            entry: false,
        });
    }
    Ok(out)
}

pub(super) fn module_bytes(root: &Path, argv0: &str) -> io::Result<Vec<u8>> {
    let rel = argv0.trim_start_matches('/');
    if !rel.is_empty()
        && let Some(path) = safe_join(root, rel)
        && path.is_file()
    {
        return fs::read(path);
    }
    let mut found = Vec::new();
    find_wasm(root, &mut found)?;
    match found.len() {
        1 => fs::read(&found[0]),
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("wasm module not found: {argv0}"),
        )),
        n => Err(io::Error::other(format!(
            "wasm image has {n} modules and entrypoint {argv0} is not one of them"
        ))),
    }
}

fn find_wasm(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        let ft = ent.file_type()?;
        if ft.is_dir() {
            find_wasm(&path, out)?;
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "wasm") {
            out.push(path);
        }
    }
    Ok(())
}

fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Bind mounts, named volumes and tmpfs from `mount_info`, then the rootfs
/// at `/` unless a mount already took it. Mounts come first so preview 1
/// fd 3 is the first `-v`, which is what a small module can rely on.
/// Named volumes live under `volume_root` on the host: the guest disk is
/// asleep, so they are not the bytes a Linux container would see.
pub(super) fn preopens(
    rootfs: &Path,
    mounts: &[serde_json::Value],
    readonly_root: bool,
    volume_root: &Path,
    tmp: &Path,
) -> io::Result<Vec<Preopen>> {
    let mut out: Vec<Preopen> = Vec::new();
    for (i, m) in mounts.iter().enumerate() {
        let Some(dest) = m["Destination"].as_str().filter(|d| !d.is_empty()) else {
            continue;
        };
        let guest = guest_path(dest);
        let read_only = m["RW"].as_bool() == Some(false);
        let host = match m["Type"].as_str().unwrap_or("") {
            "bind" => {
                let src = m["Source"].as_str().unwrap_or("");
                let path = PathBuf::from(src);
                if !path.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("wasm bind mounts must be directories: {src}"),
                    ));
                }
                path
            }
            "volume" => {
                let name = m["Name"].as_str().unwrap_or("");
                volume_dir(volume_root, name)?
            }
            "tmpfs" => {
                let dir = tmp.join(i.to_string());
                fs::create_dir_all(&dir)?;
                dir
            }
            _ => continue,
        };
        if let Some(existing) = out.iter_mut().find(|p| p.guest == guest) {
            *existing = Preopen {
                host,
                guest,
                read_only,
            };
        } else {
            out.push(Preopen {
                host,
                guest,
                read_only,
            });
        }
    }
    if !out.iter().any(|p| p.guest == "/") {
        out.push(Preopen {
            host: rootfs.to_path_buf(),
            guest: "/".into(),
            read_only: readonly_root,
        });
    }
    Ok(out)
}

fn guest_path(dest: &str) -> String {
    let dest = if dest.starts_with('/') {
        dest.to_owned()
    } else {
        format!("/{dest}")
    };
    if dest.len() > 1 {
        dest.trim_end_matches('/').to_owned()
    } else {
        dest
    }
}

fn volume_dir(root: &Path, name: &str) -> io::Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(io::Error::other(format!("bad wasm volume name {name}")));
    }
    let dir = root.join(name);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// What one run needs. The engine is this container's: `increment_epoch`
/// stops only this module, not every other wasm container.
struct Job<'a> {
    engine: &'a wasmtime::Engine,
    bytes: &'a [u8],
    args: &'a [String],
    env: &'a [(String, String)],
    cwd: &'a str,
    preopens: &'a [Preopen],
    stdin: Option<File>,
    stdout: File,
    stderr: File,
    killed: &'a AtomicU32,
    interruptible: bool,
}

pub(super) struct WasiIo {
    pub(super) stdin: Option<File>,
    pub(super) stdout: File,
    pub(super) stderr: File,
}

pub(super) fn os_pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two owned descriptors into `fds`, or it fails and
    // writes nothing we keep. pipe2 is Linux-only; macOS sets CLOEXEC below.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: F_SETFD on a descriptor pipe just opened and that we own.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            let err = io::Error::last_os_error();
            // SAFETY: both ends are still ours. Close them before returning.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err);
        }
    }
    // SAFETY: both descriptors are open, CLOEXEC, and not owned by anyone else.
    unsafe { Ok((File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))) }
}

pub(super) fn signal_code(signal: u32) -> i32 {
    128i32.saturating_add(i32::try_from(signal).unwrap_or(i32::MAX))
}

/// Runs the module on this thread. Stdout and stderr are the write ends of
/// pipes the caller is already reading. Returns the exit code: the module's
/// `proc_exit`, `128 + signal` when stopped, 127 when `_start` is missing,
/// 1 on a trap.
fn execute(mut job: Job<'_>) -> i32 {
    let mut stderr_note = match job.stderr.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(io::stderr(), "wasm: {e}");
            return 1;
        }
    };
    let sig = job.killed.load(Ordering::SeqCst);
    if sig != 0 {
        return signal_code(sig);
    }
    let code = if is_component(job.bytes) {
        run_component(&mut job, &mut stderr_note)
    } else {
        run_core(&mut job, &mut stderr_note)
    };
    drop(stderr_note);
    code
}

/// A component's version word is layer 1. Toolchains emit `0x0001000d`;
/// older components, including the `.wat` fixtures, are `0x0001000a`.
pub(super) fn is_component(bytes: &[u8]) -> bool {
    bytes.len() >= 8
        && bytes[0..4] == *b"\0asm"
        && bytes[5..8] == [0x00, 0x01, 0x00]
        && matches!(bytes[4], 0x0a | 0x0d)
}

pub(super) fn wasi_builder_for(
    args: &[String],
    env: &[(String, String)],
    cwd: &str,
    preopens: &[Preopen],
    io: WasiIo,
) -> Result<wasmtime_wasi::WasiCtxBuilder, String> {
    let mut b = wasmtime_wasi::WasiCtx::builder();
    // Preview 1 file ops block this thread. Each container has its own thread,
    // and a wasi:http request runs on its own worker.
    b.allow_blocking_current_thread(true);
    b.args(args);
    for (k, v) in env {
        b.env(k, v);
    }
    if !cwd.is_empty() {
        b.initial_cwd(cwd);
    }
    for p in preopens {
        let perms = if p.read_only {
            wasmtime_wasi::FsPerms::ReadOnly
        } else {
            wasmtime_wasi::FsPerms::ReadWrite
        };
        b.preopened_dir(&p.host, &p.guest, perms)
            .map_err(|e| format!("preopen {}: {e}", p.host.display()))?;
    }
    b.stdout(wasmtime_wasi::cli::OutputFile::new(io.stdout));
    b.stderr(wasmtime_wasi::cli::OutputFile::new(io.stderr));
    if let Some(stdin) = io.stdin {
        b.stdin(wasmtime_wasi::cli::InputFile::new(stdin));
    }
    Ok(b)
}

fn wasi_builder(job: &Job<'_>, io: WasiIo) -> Result<wasmtime_wasi::WasiCtxBuilder, String> {
    wasi_builder_for(job.args, job.env, job.cwd, job.preopens, io)
}

fn arm_deadline(store: &mut wasmtime::Store<impl Sized>, job: &Job<'_>) -> Option<i32> {
    if !job.interruptible {
        return None;
    }
    store.set_epoch_deadline(1);
    let sig = job.killed.load(Ordering::SeqCst);
    (sig != 0).then(|| signal_code(sig))
}

fn finish(err: wasmtime::Error, job: &Job<'_>, stderr: &mut File) -> i32 {
    if let Some(exit) = err.downcast_ref::<wasmtime_wasi::I32Exit>() {
        return exit.0;
    }
    let sig = job.killed.load(Ordering::SeqCst);
    if sig != 0 {
        return signal_code(sig);
    }
    let _ = writeln!(stderr, "wasm: {err}");
    1
}

fn take_io(job: &mut Job<'_>) -> WasiIo {
    WasiIo {
        stdin: job.stdin.take(),
        stdout: std::mem::replace(&mut job.stdout, empty_file()),
        stderr: std::mem::replace(&mut job.stderr, empty_file()),
    }
}

/// A stand-in so `Job` can give its pipes away. Never written: the real
/// write ends moved into the WASI context, and dropping that context is
/// what closes them.
fn empty_file() -> File {
    fs::File::open("/dev/null").expect("wasm runner needs /dev/null")
}

fn run_core(job: &mut Job<'_>, stderr_note: &mut File) -> i32 {
    let module = match wasmtime::Module::from_binary(job.engine, job.bytes) {
        Ok(m) => m,
        Err(e) => return finish(e, job, stderr_note),
    };
    let io = take_io(job);
    let mut builder = match wasi_builder(job, io) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(stderr_note, "wasm: {e}");
            return 1;
        }
    };
    let mut linker = wasmtime::Linker::new(job.engine);
    if let Err(e) = wasmtime_wasi::p1::add_to_linker_sync(
        &mut linker,
        |ctx: &mut wasmtime_wasi::p1::WasiP1Ctx| ctx,
    ) {
        let _ = writeln!(stderr_note, "wasm: {e}");
        return 1;
    }
    let mut store = wasmtime::Store::new(job.engine, builder.build_p1());
    if let Some(code) = arm_deadline(&mut store, job) {
        return code;
    }
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => return finish(e, job, stderr_note),
    };
    let Some(start) = instance.get_func(&mut store, "_start") else {
        let _ = writeln!(stderr_note, "wasm: module does not export _start");
        return 127;
    };
    let typed = match start.typed::<(), ()>(&store) {
        Ok(f) => f,
        Err(e) => return finish(e, job, stderr_note),
    };
    match typed.call(&mut store, ()) {
        Ok(()) => 0,
        Err(e) => finish(e, job, stderr_note),
    }
}

struct ComponentState {
    ctx: wasmtime_wasi::WasiCtx,
    http: wasmtime_wasi_http::WasiHttpCtx,
    table: wasmtime::component::ResourceTable,
}

impl wasmtime_wasi::WasiView for ComponentState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_http::WasiHttpView for ComponentState {
    fn http(&mut self) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: wasmtime_wasi_http::default_hooks(),
        }
    }
}

fn run_component(job: &mut Job<'_>, stderr_note: &mut File) -> i32 {
    let component = match wasmtime::component::Component::from_binary(job.engine, job.bytes) {
        Ok(c) => c,
        Err(e) => return finish(e, job, stderr_note),
    };
    let io = take_io(job);
    let mut builder = match wasi_builder(job, io) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(stderr_note, "wasm: {e}");
            return 1;
        }
    };
    let mut linker = wasmtime::component::Linker::new(job.engine);
    if let Err(e) = wasmtime_wasi::p2::add_to_linker_sync(&mut linker) {
        let _ = writeln!(stderr_note, "wasm: {e}");
        return 1;
    }
    // Command components may import wasi:http/outgoing-handler. The sync
    // bindings run the request on wasmtime's tokio runtime, from this process.
    if let Err(e) = wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker) {
        let _ = writeln!(stderr_note, "wasm: {e}");
        return 1;
    }
    let state = ComponentState {
        ctx: builder.build(),
        http: wasmtime_wasi_http::WasiHttpCtx::new(),
        table: wasmtime::component::ResourceTable::new(),
    };
    let mut store = wasmtime::Store::new(job.engine, state);
    if let Some(code) = arm_deadline(&mut store, job) {
        return code;
    }
    let command = match wasmtime_wasi::p2::bindings::sync::Command::instantiate(
        &mut store, &component, &linker,
    ) {
        Ok(c) => c,
        Err(e) => return finish(e, job, stderr_note),
    };
    match command.wasi_cli_run().call_run(&mut store) {
        Ok(Ok(())) => 0,
        Ok(Err(())) => {
            let sig = job.killed.load(Ordering::SeqCst);
            if sig != 0 { signal_code(sig) } else { 1 }
        }
        Err(e) => finish(e, job, stderr_note),
    }
}

pub(super) fn pump(engine: Arc<super::engine::Engine>, id: String, stream: Stream, mut file: File) {
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => engine.on_wasm_output(&id, stream, &buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// One wasmtime engine per container, with epoch interruption on, so
/// `docker stop` of one module does not trap the others.
pub(super) fn new_engine() -> io::Result<wasmtime::Engine> {
    let mut config = wasmtime::Config::new();
    config.epoch_interruption(true);
    config.wasm_component_model(true);
    wasmtime::Engine::new(&config).map_err(|e| io::Error::other(e.to_string()))
}

/// `KEY=VALUE` entries. A value may itself contain `=`.
pub(super) fn split_env(env: &[String]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect()
}

/// One module to run. `stdin_read` is the module's fd 0; the write end stays
/// in [`Active`] so `input` never asks the VM for a pipe.
pub(super) struct Guest {
    pub id: String,
    pub bytes: Vec<u8>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub preopens: Vec<Preopen>,
    pub stdin_read: Option<File>,
}

/// Registers the module so `stop` can interrupt it, then runs it on `kiri-wasm`.
pub(super) fn launch(
    daemon: Arc<super::engine::Engine>,
    guest: Guest,
    stdin_write: Option<File>,
    engine: wasmtime::Engine,
    killed: Arc<AtomicU32>,
) -> io::Result<()> {
    let id = guest.id.clone();
    register(&id, engine.clone(), killed.clone(), stdin_write);
    let started = std::thread::Builder::new()
        .name("kiri-wasm".into())
        .spawn(move || {
            // Unregister before the exit is reported: a restart policy may start
            // the same id as soon as the exit is recorded.
            let id = guest.id.clone();
            let code = {
                let _leave = Leave(id.clone());
                run_logged(daemon.clone(), guest, &engine, &killed)
            };
            daemon.on_wasm_exit(&id, code);
        });
    if started.is_err() {
        unregister(&id);
    }
    started.map(|_| ())
}

fn run_logged(
    daemon: Arc<super::engine::Engine>,
    guest: Guest,
    engine: &wasmtime::Engine,
    killed: &AtomicU32,
) -> i32 {
    let (out_r, out_w) = match os_pipe() {
        Ok(p) => p,
        Err(e) => {
            daemon.on_wasm_output(&guest.id, Stream::Stderr, format!("wasm: {e}\n").as_bytes());
            return 1;
        }
    };
    let (err_r, err_w) = match os_pipe() {
        Ok(p) => p,
        Err(e) => {
            daemon.on_wasm_output(&guest.id, Stream::Stderr, format!("wasm: {e}\n").as_bytes());
            return 1;
        }
    };
    let (d_out, d_err) = (daemon.clone(), daemon.clone());
    let (id_out, id_err) = (guest.id.clone(), guest.id.clone());
    let pump_out = std::thread::Builder::new()
        .name("kiri-wasm-out".into())
        .spawn(move || pump(d_out, id_out, Stream::Stdout, out_r));
    let pump_err = std::thread::Builder::new()
        .name("kiri-wasm-err".into())
        .spawn(move || pump(d_err, id_err, Stream::Stderr, err_r));
    let (Ok(pump_out), Ok(pump_err)) = (pump_out, pump_err) else {
        daemon.on_wasm_output(
            &guest.id,
            Stream::Stderr,
            b"wasm: could not read the module's output\n",
        );
        return 1;
    };
    let code = execute(Job {
        engine,
        bytes: &guest.bytes,
        args: &guest.args,
        env: &guest.env,
        cwd: &guest.cwd,
        preopens: &guest.preopens,
        stdin: guest.stdin_read,
        stdout: out_w,
        stderr: err_w,
        killed,
        interruptible: true,
    });
    let _ = pump_out.join();
    let _ = pump_err.join();
    code
}

/// A pipe whose write end the engine keeps and whose read end the module has.
pub(super) fn stdin_pipe() -> io::Result<(File, File)> {
    os_pipe()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use super::*;

    fn shared_engine() -> &'static wasmtime::Engine {
        static ENGINE: OnceLock<wasmtime::Engine> = OnceLock::new();
        ENGINE.get_or_init(|| {
            let mut config = wasmtime::Config::new();
            config.wasm_component_model(true);
            wasmtime::Engine::new(&config).expect("wasmtime")
        })
    }

    fn run_wat(
        src: &str,
        args: &[&str],
        env: &[(&str, &str)],
        preopens: &[Preopen],
    ) -> (i32, String, String) {
        let bytes = wat::parse_str(src).expect("wat");
        let (mut out_r, out_w) = os_pipe().unwrap();
        let (mut err_r, err_w) = os_pipe().unwrap();
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let killed = AtomicU32::new(0);
        let code = execute(Job {
            engine: shared_engine(),
            bytes: &bytes,
            args: &args,
            env: &env,
            cwd: "",
            preopens,
            stdin: None,
            stdout: out_w,
            stderr: err_w,
            killed: &killed,
            interruptible: false,
        });
        let mut out = String::new();
        let mut err = String::new();
        out_r.read_to_string(&mut out).unwrap();
        err_r.read_to_string(&mut err).unwrap();
        (code, out, err)
    }

    fn scratch(label: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("kiri-wasm-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn wasi_platforms_are_wasm_and_linux_is_not() {
        assert!(is_wasm_platform("wasi/wasm"));
        assert!(is_wasm_platform("wasip1/wasm32"));
        assert!(is_wasm_platform("wasip2/wasm64"));
        assert!(!is_wasm_platform("linux/amd64"));
        assert!(!is_wasm_platform("linux/arm64"));
        assert!(!is_wasm_platform("wasi/amd64"));
        assert!(!is_wasm_platform("linux/wasm"));
        assert!(!is_wasm_platform("wasm32-wasip1"));
    }

    #[test]
    fn hello_world_writes_stdout() {
        let (code, out, err) = run_wat(
            include_str!("../../../../tests/wasm/hello.wat"),
            &["/hello.wasm"],
            &[],
            &[],
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, "hello-wasi\n");
    }

    #[test]
    fn args_and_env_reach_the_module() {
        let (code, out, err) = run_wat(
            include_str!("../../../../tests/wasm/args.wat"),
            &["/args.wasm", "alpha", "beta"],
            &[("WASM_MODE", "wasi")],
            &[],
        );
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("beta"), "{out}");
        assert!(out.contains("WASM_MODE=wasi"), "{out}");
    }

    #[test]
    fn a_nonzero_exit_is_the_modules_code() {
        let (code, _, err) = run_wat(
            include_str!("../../../../tests/wasm/exit.wat"),
            &["/exit.wasm"],
            &[],
            &[],
        );
        assert_eq!(code, 42, "{err}");
    }

    #[test]
    fn a_read_only_preopen_can_be_read_and_not_written() {
        let dir = scratch("ro");
        fs::write(dir.join("note.txt"), b"wasm-note").unwrap();
        let pre = [Preopen {
            host: dir.clone(),
            guest: "/data".into(),
            read_only: true,
        }];
        let (code, out, err) = run_wat(
            include_str!("../../../../tests/wasm/read.wat"),
            &["/read.wasm"],
            &[],
            &pre,
        );
        assert_eq!(code, 0, "{err} / {out}");
        assert!(out.contains("wasm-note"), "{out}");
        assert!(out.contains("readonly"), "{out}");
        assert!(
            !dir.join("nope.txt").exists(),
            "a read-only preopen created nope.txt"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stop_interrupts_a_looping_module() {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let bytes = wat::parse_str(include_str!("../../../../tests/wasm/sleep.wat")).unwrap();
        let killed = Arc::new(AtomicU32::new(0));
        let flag = killed.clone();
        let eng = engine.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (out_r, out_w) = os_pipe().unwrap();
            let (err_r, err_w) = os_pipe().unwrap();
            drop((out_r, err_r));
            let args = vec!["/sleep.wasm".to_owned()];
            let code = execute(Job {
                engine: &eng,
                bytes: &bytes,
                args: &args,
                env: &[],
                cwd: "",
                preopens: &[],
                stdin: None,
                stdout: out_w,
                stderr: err_w,
                killed: &flag,
                interruptible: true,
            });
            let _ = tx.send(code);
        });
        std::thread::sleep(Duration::from_millis(200));
        killed.store(libc::SIGTERM as u32, Ordering::SeqCst);
        engine.increment_epoch();
        let code = rx
            .recv_timeout(Duration::from_secs(15))
            .expect("the module did not stop");
        assert_eq!(code, 128 + libc::SIGTERM);
    }

    fn put(store: &kiri_image::Store, bytes: &[u8]) -> (String, u64) {
        let mut w = store.ingest().unwrap();
        w.write_all(bytes).unwrap();
        w.finish().unwrap()
    }

    /// The image `tests/wasm.sh` loads: every module, entrypoint `/hello.wasm`.
    fn install_wasi_image(store: &kiri_image::Store) -> kiri_image::Image {
        install_named(store, "kiri-wasm-test:latest", serde_json::Value::Null)
    }

    fn install_named(
        store: &kiri_image::Store,
        name: &str,
        labels: serde_json::Value,
    ) -> kiri_image::Image {
        let files = [
            (
                "hello.wasm",
                include_str!("../../../../tests/wasm/hello.wat"),
            ),
            ("exit.wasm", include_str!("../../../../tests/wasm/exit.wat")),
            ("args.wasm", include_str!("../../../../tests/wasm/args.wat")),
            ("read.wasm", include_str!("../../../../tests/wasm/read.wat")),
            (
                "sleep.wasm",
                include_str!("../../../../tests/wasm/sleep.wat"),
            ),
        ];
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for (name, src) in files {
                let bytes = wat::parse_str(src).expect(name);
                let mut header = tar::Header::new_gnu();
                header.set_path(name).unwrap();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, bytes.as_slice()).unwrap();
            }
            let http = include_bytes!("../../../../tests/wasm/http.wasm");
            let mut header = tar::Header::new_gnu();
            header.set_path("http.wasm").unwrap();
            header.set_size(http.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, http.as_slice()).unwrap();
            let fetch = include_bytes!("../../../../tests/wasm/fetch.wasm");
            let mut header = tar::Header::new_gnu();
            header.set_path("fetch.wasm").unwrap();
            header.set_size(fetch.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, fetch.as_slice()).unwrap();
            let stream = include_bytes!("../../../../tests/wasm/stream.wasm");
            let mut header = tar::Header::new_gnu();
            header.set_path("stream.wasm").unwrap();
            header.set_size(stream.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, stream.as_slice()).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
            enc.write_all(&raw).unwrap();
            enc.finish().unwrap();
        }
        let mut config = serde_json::json!({
            "os": "wasi",
            "architecture": "wasm",
            "created": "2026-01-01T00:00:00Z",
            "config": {
                "Entrypoint": ["/hello.wasm"],
                "Cmd": [],
                "Env": ["WASM_IMAGE=1"],
                "WorkingDir": "/"
            }
        });
        if labels.as_object().is_some_and(|o| !o.is_empty()) {
            config["config"]["Labels"] = labels;
        }
        let (cfg, _) = put(store, &serde_json::to_vec(&config).unwrap());
        let (layer, size) = put(store, &gz);
        let image = kiri_image::archive::from_config(
            name.into(),
            &cfg,
            &config,
            vec![kiri_image::Layer {
                digest: layer,
                size,
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            }],
        );
        store.put_image(image.clone()).unwrap();
        image
    }

    #[test]
    fn a_loadable_wasi_image_can_be_written() {
        let dir = scratch("image");
        let store = kiri_image::Store::open(&dir).unwrap();
        let image = install_wasi_image(&store);
        let mut tar = Vec::new();
        let images = [image];
        kiri_image::archive::save(&store, &images, &mut tar).unwrap();
        let other = kiri_image::Store::open(dir.join("loaded")).unwrap();
        let loaded = kiri_image::archive::load(&other, tar.as_slice()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "kiri-wasm-test:latest");
        assert_eq!(loaded[0].platform, "wasi/wasm");
        assert_eq!(loaded[0].config.entrypoint, ["/hello.wasm"]);
        assert_eq!(loaded[0].config.env, ["WASM_IMAGE=1"]);
        if let Ok(path) = std::env::var("KIRI_WASM_IMAGE") {
            fs::write(path, &tar).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wasi_image_runs_on_the_host_without_waking_a_vm() {
        let dir = scratch("engine");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        // A window Stop left under HOME must not fail this run.
        engine.test_clear_stopped();
        let linux = kiri_image::Image {
            name: "alpine:latest".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            index_digest: None,
            config_digest: format!("sha256:{}", "b".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/amd64".into(),
            created: String::new(),
            pulled_at: 0,
            config: kiri_image::RunConfig::default(),
            mirror: None,
        };
        let linux_spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": linux.name, "image_id": linux.digest, "entrypoint": ["/bin/true"], "cmd": [],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {}
        }))
        .unwrap();
        let linux_id = engine
            .create(Some("linux-not-wasm".into()), linux_spec, &linux)
            .unwrap();
        assert!(!engine.with(&linux_id, |c| c.spec.wasm).unwrap());
        assert!(!engine.is_awake());

        let image = install_wasi_image(&engine.store);
        let spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": image.digest, "entrypoint": ["/hello.wasm"], "cmd": [],
            "env": ["WASM_IMAGE=1"], "cwd": "/", "user": "", "auto_remove": false,
            "network_mode": "none", "labels": {}
        }))
        .unwrap();
        let id = engine
            .create(Some("wasi-hello".into()), spec, &image)
            .unwrap();
        assert!(engine.with(&id, |c| c.spec.wasm).unwrap());
        assert!(
            !engine.is_awake(),
            "creating a wasm container booted the VM"
        );
        engine.start(&id).unwrap();
        assert_eq!(engine.wait(&id, "not-running").unwrap(), 0);
        assert!(!engine.is_awake(), "running a wasm container booted the VM");
        let logs = engine.logs(&id, false).unwrap().unwrap();
        let mut text = Vec::new();
        for rec in logs.past.tail(None) {
            if rec.stream == Stream::Stdout {
                text.extend_from_slice(&rec.data);
            }
        }
        assert!(
            String::from_utf8_lossy(&text).contains("hello-wasi"),
            "{}",
            String::from_utf8_lossy(&text)
        );
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_component_versions_are_recognised() {
        for (name, bytes) in [
            (
                "http.wasm",
                include_bytes!("../../../../tests/wasm/http.wasm").as_slice(),
            ),
            (
                "fetch.wasm",
                include_bytes!("../../../../tests/wasm/fetch.wasm").as_slice(),
            ),
            (
                "stream.wasm",
                include_bytes!("../../../../tests/wasm/stream.wasm").as_slice(),
            ),
        ] {
            assert!(is_component(bytes), "{name} version {:02x?}", &bytes[4..8]);
        }
    }

    #[test]
    fn a_command_module_still_cannot_publish_ports() {
        let dir = scratch("ports");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        engine.test_clear_stopped();
        let image = install_wasi_image(&engine.store);
        let spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": image.digest, "entrypoint": ["/hello.wasm"], "cmd": [],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "ports": [{"container_port": 80, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0}]
        }))
        .unwrap();
        let id = engine
            .create(Some("wasi-cmd".into()), spec, &image)
            .unwrap();
        let err = engine.start(&id).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("wasi:http"), "{msg}");
        assert!(!engine.is_awake());
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_labels_let_the_container_override_the_image() {
        let image = serde_json::json!({
            "dev.kiri.wasm.port.80": "/api.wasm",
            "dev.kiri.wasm.port.81": "/admin.wasm",
            "dev.kiri.http.port": "80",
        });
        let container = serde_json::json!({
            "dev.kiri.wasm.port.81": "/other.wasm",
            "dev.kiri.domain": "api",
        });
        let map = super::port_modules(&image, &container).unwrap();
        assert_eq!(map.get(&80).map(String::as_str), Some("/api.wasm"));
        assert_eq!(map.get(&81).map(String::as_str), Some("/other.wasm"));
        assert_eq!(map.len(), 2);
        let blob = br#"{"config":{"Labels":{"dev.kiri.wasm.port.80":"/api.wasm"}}}"#;
        assert_eq!(
            super::config_labels(blob)["dev.kiri.wasm.port.80"],
            "/api.wasm"
        );
        assert!(super::config_labels(b"{}").is_null());
        let bad = super::port_modules(
            &serde_json::json!({"dev.kiri.wasm.port.nope": "/a.wasm"}),
            &serde_json::json!({}),
        );
        assert!(bad.unwrap_err().to_string().contains("bad wasm port"));
        let empty = super::port_modules(
            &serde_json::json!({"dev.kiri.wasm.port.80": ""}),
            &serde_json::json!({}),
        );
        assert!(
            empty
                .unwrap_err()
                .to_string()
                .contains("needs a module path")
        );
    }

    #[test]
    fn a_port_label_must_name_an_incoming_handler() {
        let dir = scratch("port-bad");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        engine.test_clear_stopped();
        let image = install_wasi_image(&engine.store);
        let spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": image.digest, "entrypoint": ["/http.wasm"], "cmd": [],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none",
            "labels": {"dev.kiri.wasm.port.81": "/hello.wasm"},
            "ports": [{"container_port": 81, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0}]
        }))
        .unwrap();
        let id = engine
            .create(Some("wasi-bad-port".into()), spec, &image)
            .unwrap();
        let err = engine.start(&id).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("hello.wasm"), "{msg}");
        assert!(msg.contains("wasi:http"), "{msg}");
        assert!(!engine.is_awake());
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wasi_http_component_serves_until_stopped() {
        let dir = scratch("http");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        engine.test_clear_stopped();
        let image = install_wasi_image(&engine.store);
        let spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": image.digest, "entrypoint": ["/http.wasm"], "cmd": [],
            "env": ["WASM_IMAGE=1"], "cwd": "/", "user": "", "auto_remove": false,
            "network_mode": "none", "labels": {},
            "ports": [{"container_port": 80, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0}]
        }))
        .unwrap();
        let id = engine
            .create(Some("wasi-http".into()), spec, &image)
            .unwrap();
        engine.start(&id).unwrap();
        assert!(!engine.is_awake(), "serving http booted the VM");
        let port = engine
            .with(&id, |c| c.forwards.as_ref().unwrap().bound[0].2)
            .unwrap();
        assert_ne!(port, 0);
        let routed = engine.route("wasi-http.kiri.local").expect("name route");
        assert_eq!(routed.ip, [127, 0, 0, 1]);

        let get = http_exchange(
            port,
            "GET /users/1 HTTP/1.1\r\nHost: wasi-http.kiri.local\r\nx-echo: hi\r\nConnection: close\r\n\r\n",
        );
        assert!(get.contains("HTTP/1.1 200"), "{get}");
        assert!(get.contains("GET /users/1\nhi"), "{get}");
        let post = http_exchange(
            port,
            "POST /v1 HTTP/1.1\r\nHost: wasi-http.kiri.local\r\nContent-Length: 4\r\nConnection: close\r\n\r\nping",
        );
        assert!(post.contains("POST /v1\nping"), "{post}");

        let logs = engine.logs(&id, false).unwrap().unwrap();
        let mut text = Vec::new();
        for rec in logs.past.tail(None) {
            if rec.stream == Stream::Stdout {
                text.extend_from_slice(&rec.data);
            }
        }
        let text = String::from_utf8_lossy(&text);
        assert!(text.contains("GET /users/1"), "{text}");
        assert!(text.contains("POST /v1"), "{text}");

        engine.stop(&id, Duration::from_secs(5)).unwrap();
        let code = engine.wait(&id, "not-running").unwrap();
        assert!(code == 143 || code == 137, "exit {code}");
        assert!(engine.route("wasi-http.kiri.local").is_none());
        assert!(!engine.is_awake());
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn published_ports_can_serve_different_modules() {
        let dir = scratch("ports-http");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        engine.test_clear_stopped();
        let labels = serde_json::json!({
            "dev.kiri.wasm.port.80": "/http.wasm",
            "dev.kiri.wasm.port.81": "/stream.wasm",
        });
        let image = install_named(&engine.store, "kiri-wasm-ports:latest", labels);
        let ports = serde_json::json!([
            {"container_port": 80, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0},
            {"container_port": 81, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0},
        ]);
        let spec = |name_labels: serde_json::Value, entry: &str| {
            serde_json::from_value(serde_json::json!({
                "image": image.name, "image_id": image.digest, "entrypoint": [entry], "cmd": [],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none",
                "labels": name_labels, "ports": ports,
            }))
            .unwrap()
        };
        let id = engine
            .create(
                Some("wasi-ports".into()),
                spec(
                    serde_json::json!({"dev.kiri.http.port": "81"}),
                    "/hello.wasm",
                ),
                &image,
            )
            .unwrap();
        engine.start(&id).unwrap();
        assert!(!engine.is_awake(), "two handlers booted the VM");
        let (p80, p81) = engine
            .with(&id, |c| {
                let bound = &c.forwards.as_ref().unwrap().bound;
                (bound[0].2, bound[1].2)
            })
            .unwrap();
        let echo = http_exchange(
            p80,
            "GET /users/1 HTTP/1.1\r\nHost: wasi-ports.kiri.local\r\nx-echo: hi\r\nConnection: close\r\n\r\n",
        );
        assert!(echo.contains("GET /users/1\nhi"), "{echo}");
        let hashed = sha256_hex(b"abc");
        let upload = http_exchange(
            p81,
            "POST /upload HTTP/1.1\r\nHost: wasi-ports.kiri.local\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        );
        assert!(upload.contains(&hashed), "{upload}");
        assert!(
            upload.contains("\n3\n") || upload.contains("\n3\r"),
            "{upload}"
        );
        let routed = super::super::domains::test_exchange(
            &engine,
            b"POST /upload HTTP/1.1\r\nHost: wasi-ports.kiri.local\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        )
        .unwrap()
        .expect("domain router");
        let routed = String::from_utf8_lossy(&routed);
        // dev.kiri.http.port=81, so the name reaches stream.wasm, not the entry port.
        assert!(routed.contains(&hashed), "{routed}");
        assert!(!routed.contains("POST /upload"), "{routed}");
        assert_ne!(
            super::super::wasm_http::http_addr_for(&id, 80),
            super::super::wasm_http::http_addr_for(&id, 81)
        );

        let over = engine
            .create(
                Some("wasi-override".into()),
                spec(
                    serde_json::json!({"dev.kiri.wasm.port.81": "/http.wasm", "dev.kiri.http.port": "81"}),
                    "/hello.wasm",
                ),
                &image,
            )
            .unwrap();
        engine.start(&over).unwrap();
        let target = engine.route("wasi-override.kiri.local").expect("name");
        assert_eq!(target.label, Some(81));
        let (o80, o81) = engine
            .with(&over, |c| {
                let bound = &c.forwards.as_ref().unwrap().bound;
                (bound[0].2, bound[1].2)
            })
            .unwrap();
        let left = http_exchange(
            o80,
            "GET /left HTTP/1.1\r\nHost: wasi-override.kiri.local\r\nConnection: close\r\n\r\n",
        );
        let right = http_exchange(
            o81,
            "GET /right HTTP/1.1\r\nHost: wasi-override.kiri.local\r\nConnection: close\r\n\r\n",
        );
        assert!(left.contains("GET /left"), "{left}");
        assert!(right.contains("GET /right"), "{right}");
        assert_eq!(
            super::super::wasm_http::http_addr_for(&over, 80),
            super::super::wasm_http::http_addr_for(&over, 81)
        );
        let via_name = super::super::domains::test_exchange(
            &engine,
            b"GET /admin HTTP/1.1\r\nHost: wasi-override.kiri.local\r\nConnection: close\r\n\r\n",
        )
        .unwrap()
        .expect("domain router for port 81");
        let via_name = String::from_utf8_lossy(&via_name);
        assert!(via_name.contains("GET /admin"), "{via_name}");

        let plain = install_wasi_image(&engine.store);
        let one: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": plain.name, "image_id": plain.digest, "entrypoint": ["/http.wasm"], "cmd": [],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none",
            "labels": {},
            "ports": [
                {"container_port": 80, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0},
                {"container_port": 81, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0}
            ]
        }))
        .unwrap();
        let shared = engine
            .create(Some("wasi-shared".into()), one, &plain)
            .unwrap();
        engine.start(&shared).unwrap();
        assert_eq!(
            super::super::wasm_http::http_addr_for(&shared, 80),
            super::super::wasm_http::http_addr_for(&shared, 81)
        );
        let (s80, s81) = engine
            .with(&shared, |c| {
                let bound = &c.forwards.as_ref().unwrap().bound;
                (bound[0].2, bound[1].2)
            })
            .unwrap();
        let a = http_exchange(
            s80,
            "GET /a HTTP/1.1\r\nHost: wasi-shared.kiri.local\r\nConnection: close\r\n\r\n",
        );
        let b = http_exchange(
            s81,
            "GET /b HTTP/1.1\r\nHost: wasi-shared.kiri.local\r\nConnection: close\r\n\r\n",
        );
        assert!(a.contains("GET /a"), "{a}");
        assert!(b.contains("GET /b"), "{b}");

        engine.stop(&id, Duration::from_secs(5)).unwrap();
        engine.stop(&over, Duration::from_secs(5)).unwrap();
        engine.stop(&shared, Duration::from_secs(5)).unwrap();
        assert!(!engine.is_awake());
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_component_fetches_on_the_host() {
        let (port, server) = serve_text(b"kiri-fetched-ok\n");
        let url = format!("http://127.0.0.1:{port}/hello");
        let (code, out, err) = run_bytes(
            include_bytes!("../../../../tests/wasm/fetch.wasm"),
            &["/fetch.wasm", &url],
        );
        let _ = server.join();
        assert_eq!(code, 0, "{err} / {out}");
        assert!(out.contains("kiri-fetched-ok"), "{out} / {err}");
    }

    #[test]
    fn a_wasi_http_handler_fetches_on_the_host() {
        let (upstream, server) = serve_text(b"kiri-fetched-ok\n");
        let dir = scratch("fetch-http");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        engine.test_clear_stopped();
        let image = install_wasi_image(&engine.store);
        let spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": image.digest, "entrypoint": ["/http.wasm"], "cmd": [],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "ports": [{"container_port": 80, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0}]
        }))
        .unwrap();
        let id = engine
            .create(Some("wasi-fetch".into()), spec, &image)
            .unwrap();
        engine.start(&id).unwrap();
        assert!(!engine.is_awake(), "fetching over http booted the VM");
        let published = engine
            .with(&id, |c| c.forwards.as_ref().unwrap().bound[0].2)
            .unwrap();
        let req = format!(
            "GET /out HTTP/1.1\r\nHost: wasi-fetch.kiri.local\r\nx-fetch: http://127.0.0.1:{upstream}/hello\r\nConnection: close\r\n\r\n"
        );
        let got = http_exchange(published, &req);
        assert!(got.contains("HTTP/1.1 200"), "{got}");
        assert!(got.contains("kiri-fetched-ok"), "{got}");
        engine.stop(&id, Duration::from_secs(5)).unwrap();
        let _ = engine.wait(&id, "not-running");
        let _ = server.join();
        assert!(!engine.is_awake());
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Four megabytes in, four megabytes out, past the old 1 MiB buffer.
    /// The upload is chunked, so the length is not known up front. The
    /// download is chunked because the guest sets no content-length.
    #[test]
    fn a_wasi_http_handler_streams_several_megabytes() {
        const N: usize = 4 * 1024 * 1024;
        let dir = scratch("stream");
        let store = kiri_image::Store::open(&dir).unwrap();
        let engine = super::super::engine::Engine::boot(store).unwrap();
        engine.test_clear_stopped();
        let image = install_wasi_image(&engine.store);
        let spec: super::super::engine::Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": image.digest, "entrypoint": ["/stream.wasm"], "cmd": [],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "ports": [{"container_port": 80, "proto": "tcp", "host_ip": "127.0.0.1", "host_port": 0}]
        }))
        .unwrap();
        let id = engine
            .create(Some("wasi-stream".into()), spec, &image)
            .unwrap();
        engine.start(&id).unwrap();
        assert!(!engine.is_awake(), "streaming http booted the VM");
        let port = engine
            .with(&id, |c| c.forwards.as_ref().unwrap().bound[0].2)
            .unwrap();

        let small = http_raw(
            port,
            "POST /upload HTTP/1.1\r\nHost: wasi-stream.kiri.local\r\nContent-Length: 3\r\nConnection: close\r\n\r\n",
            b"abc",
        );
        let (_, small_body) = split_http(&small);
        assert_eq!(
            String::from_utf8_lossy(&small_body),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n3\n",
            "{}",
            String::from_utf8_lossy(&small)
        );

        let payload = pattern(N);
        let expect = sha256_hex(&payload);
        let raw = http_raw(
            port,
            "POST /upload HTTP/1.1\r\nHost: wasi-stream.kiri.local\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            &chunked(&payload),
        );
        let (_, body) = split_http(&raw);
        let text = String::from_utf8_lossy(&body);
        assert_eq!(text, format!("{expect}\n{N}\n"), "{text}");

        let down = http_raw(
            port,
            &format!(
                "GET /download?n={N} HTTP/1.1\r\nHost: wasi-stream.kiri.local\r\nConnection: close\r\n\r\n"
            ),
            b"",
        );
        let (head, got) = split_http(&down);
        assert!(
            head.to_ascii_lowercase().contains("chunked"),
            "response should stream: {head}"
        );
        assert_eq!(got.len(), N, "downloaded {} bytes\n{head}", got.len());
        assert_eq!(sha256_hex(&got), expect);

        let too_big = 32 * 1024 * 1024 + 1;
        let refused = http_raw(
            port,
            &format!(
                "POST /upload HTTP/1.1\r\nHost: wasi-stream.kiri.local\r\nContent-Length: {too_big}\r\nConnection: close\r\n\r\n"
            ),
            b"",
        );
        let refused = String::from_utf8_lossy(&refused);
        assert!(refused.contains("413"), "{refused}");

        engine.stop(&id, Duration::from_secs(5)).unwrap();
        let _ = engine.wait(&id, "not-running");
        assert!(!engine.is_awake());
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    /// One HTTP response, from this process. The wasm module dials it itself.
    fn serve_text(body: &'static [u8]) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body);
        });
        (port, server)
    }

    fn run_bytes(bytes: &[u8], args: &[&str]) -> (i32, String, String) {
        let (mut out_r, out_w) = os_pipe().unwrap();
        let (mut err_r, err_w) = os_pipe().unwrap();
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let killed = AtomicU32::new(0);
        let code = execute(Job {
            engine: shared_engine(),
            bytes,
            args: &args,
            env: &[],
            cwd: "",
            preopens: &[],
            stdin: None,
            stdout: out_w,
            stderr: err_w,
            killed: &killed,
            interruptible: false,
        });
        let mut out = String::new();
        let mut err = String::new();
        out_r.read_to_string(&mut out).unwrap();
        err_r.read_to_string(&mut err).unwrap();
        (code, out, err)
    }

    fn http_exchange(port: u16, req: &str) -> String {
        let mut stream = None;
        let mut last = String::new();
        for _ in 0..20 {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut stream = stream.unwrap_or_else(|| panic!("no connection to {port}: {last}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn http_raw(port: u16, head: &str, body: &[u8]) -> Vec<u8> {
        let mut stream = None;
        let mut last = String::new();
        for _ in 0..40 {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut stream = stream.unwrap_or_else(|| panic!("no connection to {port}: {last}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 64 * 1024];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        buf
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u64).wrapping_mul(131).wrapping_add(17) as u8)
            .collect()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn chunked(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 64);
        for chunk in data.chunks(64 * 1024) {
            out.extend_from_slice(format!("{:X}\r\n", chunk.len()).as_bytes());
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    /// Headers and the decoded body. Chunked responses are unframed.
    fn split_http(raw: &[u8]) -> (String, Vec<u8>) {
        let sep = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(raw)));
        let head = String::from_utf8_lossy(&raw[..sep]).into_owned();
        let mut rest = &raw[sep + 4..];
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            let mut body = Vec::new();
            loop {
                let line_end = rest
                    .windows(2)
                    .position(|w| w == b"\r\n")
                    .unwrap_or_else(|| panic!("chunk size in {head}"));
                let line = std::str::from_utf8(&rest[..line_end]).unwrap();
                let size =
                    usize::from_str_radix(line.split(';').next().unwrap().trim(), 16).unwrap();
                rest = &rest[line_end + 2..];
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&rest[..size]);
                rest = &rest[size + 2..];
            }
            (head, body)
        } else {
            (head, rest.to_vec())
        }
    }
}
