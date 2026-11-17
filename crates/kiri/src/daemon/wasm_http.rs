//! `wasi:http/incoming-handler` containers, the shape `wasmtime serve` runs.
//!
//! The component stays up. One hyper server on `127.0.0.1` accepts the
//! connections; each request builds a fresh instance. Published ports and
//! the container-domain router dial that socket and copy bytes, so they do
//! not grow a second HTTP stack. TLS stops at the domain router: the
//! handler always sees plain HTTP.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use http_body_util::BodyExt;
use hyper::server::conn::http1;
use kiri_proto::Stream;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use wasmtime::Store;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use super::wasm::{self, Preopen};

/// Instantiations in flight. Past this, a connection is refused with 503
/// instead of queuing wasmtime compiles without bound.
const IN_FLIGHT: usize = 64;
/// A request body is read to the end before the guest runs, so the
/// connection task is not stuck waiting on a guest that is itself waiting
/// to read. Larger bodies are rejected.
const MAX_BODY: usize = 1024 * 1024;

struct State {
    ctx: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: wasmtime_wasi_http::default_hooks(),
        }
    }
}

struct Running {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
}

static RUNNING: LazyLock<Mutex<HashMap<String, Running>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn running() -> std::sync::MutexGuard<'static, HashMap<String, Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the domain router and published ports connect, once the handler is
/// listening.
pub(super) fn http_addr(id: &str) -> Option<SocketAddr> {
    running().get(id).map(|r| r.addr)
}

/// Wakes the accept loop. The in-flight request traps because `interrupt`
/// already moved this container's epoch.
pub(super) fn shutdown(id: &str) {
    if let Some(run) = running().get(id) {
        let _ = run.stop.send(true);
    }
}

pub(super) fn forget(id: &str) {
    running().remove(id);
}

/// A compiled incoming-handler and the socket it will accept on.
/// `None` means the bytes are not one: a core module, or a command
/// component. The caller runs those the usual way.
pub(super) struct Prep {
    engine: wasmtime::Engine,
    pre: ProxyPre<State>,
    listener: std::net::TcpListener,
    pub(super) addr: SocketAddr,
    stop: watch::Receiver<bool>,
}

pub(super) fn prepare(id: &str, bytes: &[u8]) -> io::Result<Option<Prep>> {
    if !wasm::is_component(bytes) {
        return Ok(None);
    }
    if running().contains_key(id) {
        return Err(io::Error::other(
            "wasm http is already serving this container",
        ));
    }
    let engine = http_engine()?;
    let component = Component::from_binary(&engine, bytes)
        .map_err(|e| io::Error::other(format!("wasm component: {e}")))?;
    let mut linker = Linker::<State>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| io::Error::other(e.to_string()))?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let Ok(pre) = linker.instantiate_pre(&component) else {
        return Ok(None);
    };
    let Ok(pre) = ProxyPre::new(pre) else {
        return Ok(None);
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let (tx, stop) = watch::channel(false);
    running().insert(id.to_owned(), Running { addr, stop: tx });
    Ok(Some(Prep {
        engine,
        pre,
        listener,
        addr,
        stop,
    }))
}

fn http_engine() -> io::Result<wasmtime::Engine> {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    // Async host calls are the default in this wasmtime. `async_support` no
    // longer changes anything.
    config.epoch_interruption(true);
    wasmtime::Engine::new(&config).map_err(|e| io::Error::other(e.to_string()))
}

/// Accepts until `shutdown`. The thread reports the exit itself.
pub(super) fn spawn(
    daemon: Arc<super::engine::Engine>,
    guest: wasm::Guest,
    prep: Prep,
    stdin_write: Option<File>,
    killed: Arc<AtomicU32>,
) -> io::Result<()> {
    let id = guest.id.clone();
    wasm::register(&id, prep.engine.clone(), killed.clone(), stdin_write);
    let started = std::thread::Builder::new()
        .name("kiri-wasm".into())
        .spawn(move || {
            let id = guest.id.clone();
            let code = {
                let _leave = wasm::Leave(id.clone());
                run(daemon.clone(), guest, prep, killed)
            };
            daemon.on_wasm_exit(&id, code);
        });
    if started.is_err() {
        wasm::unregister(&id);
    }
    started.map(|_| ())
}

fn run(
    daemon: Arc<super::engine::Engine>,
    guest: wasm::Guest,
    prep: Prep,
    killed: Arc<AtomicU32>,
) -> i32 {
    let wasm::Guest {
        id,
        args,
        env,
        cwd,
        preopens,
        stdin_read,
        bytes: _,
    } = guest;
    let _stdin = stdin_read;
    let (out_r, out_w) = match wasm::os_pipe() {
        Ok(p) => p,
        Err(e) => {
            note(&daemon, &id, Stream::Stderr, &format!("wasm http: {e}\n"));
            return exit_code(&killed, 1);
        }
    };
    let (err_r, err_w) = match wasm::os_pipe() {
        Ok(p) => p,
        Err(e) => {
            note(&daemon, &id, Stream::Stderr, &format!("wasm http: {e}\n"));
            return exit_code(&killed, 1);
        }
    };
    let (d_out, d_err) = (daemon.clone(), daemon.clone());
    let (id_out, id_err) = (id.clone(), id.clone());
    let pump_out = std::thread::Builder::new()
        .name("kiri-wasm-out".into())
        .spawn(move || wasm::pump(d_out, id_out, Stream::Stdout, out_r));
    let pump_err = std::thread::Builder::new()
        .name("kiri-wasm-err".into())
        .spawn(move || wasm::pump(d_err, id_err, Stream::Stderr, err_r));
    let (Ok(pump_out), Ok(pump_err)) = (pump_out, pump_err) else {
        note(
            &daemon,
            &id,
            Stream::Stderr,
            "wasm http: could not read output\n",
        );
        return exit_code(&killed, 1);
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_count())
        .enable_io()
        .enable_time()
        .thread_name("kiri-wasm-http")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            note(&daemon, &id, Stream::Stderr, &format!("wasm http: {e}\n"));
            drop(out_w);
            drop(err_w);
            let _ = pump_out.join();
            let _ = pump_err.join();
            return exit_code(&killed, 1);
        }
    };
    // from_std registers the socket with the runtime that is current.
    let listener = match rt.block_on(async { tokio::net::TcpListener::from_std(prep.listener) }) {
        Ok(listener) => listener,
        Err(e) => {
            note(&daemon, &id, Stream::Stderr, &format!("wasm http: {e}\n"));
            drop(out_w);
            drop(err_w);
            let _ = pump_out.join();
            let _ = pump_err.join();
            return exit_code(&killed, 1);
        }
    };
    let srv = Arc::new(Srv {
        daemon: daemon.clone(),
        id: id.clone(),
        engine: prep.engine,
        pre: prep.pre,
        killed: killed.clone(),
        args,
        env,
        cwd,
        preopens,
        stdout: Mutex::new(out_w),
        stderr: Mutex::new(err_w),
    });
    let code = rt.block_on(serve(listener, prep.stop, srv.clone(), &killed));
    // Drop the files only after in-flight requests are gone, or the pumps
    // would wait on a write end a request still holds.
    drop(srv);
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    let _ = pump_out.join();
    let _ = pump_err.join();
    code
}

fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(2)
}

fn exit_code(killed: &AtomicU32, otherwise: i32) -> i32 {
    let sig = killed.load(Ordering::SeqCst);
    if sig != 0 {
        wasm::signal_code(sig)
    } else {
        otherwise
    }
}

fn note(daemon: &super::engine::Engine, id: &str, stream: Stream, text: &str) {
    daemon.on_wasm_output(id, stream, text.as_bytes());
}

struct Srv {
    daemon: Arc<super::engine::Engine>,
    id: String,
    engine: wasmtime::Engine,
    pre: ProxyPre<State>,
    killed: Arc<AtomicU32>,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
    preopens: Vec<Preopen>,
    stdout: Mutex<File>,
    stderr: Mutex<File>,
}

async fn serve(
    listener: tokio::net::TcpListener,
    mut stop: watch::Receiver<bool>,
    srv: Arc<Srv>,
    killed: &AtomicU32,
) -> i32 {
    let limit = Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT));
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                let _ = changed;
                break;
            }
            accepted = listener.accept() => {
                let mut sock = match accepted {
                    Ok((sock, _)) => sock,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        note(&srv.daemon, &srv.id, Stream::Stderr, &format!("wasm http: {e}\n"));
                        break;
                    }
                };
                let _ = sock.set_nodelay(true);
                let Some(permit) = Arc::clone(&limit).try_acquire_owned().ok() else {
                    tokio::spawn(async move {
                        let _ = sock.write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 19\r\nConnection: close\r\n\r\nserver is busy\n",
                        ).await;
                    });
                    continue;
                };
                let srv = Arc::clone(&srv);
                tokio::spawn(async move {
                    let _permit = permit;
                    let srv2 = Arc::clone(&srv);
                    let svc = hyper::service::service_fn(move |req| {
                        let srv = Arc::clone(&srv2);
                        async move { Ok::<_, Infallible>(srv.respond(req).await) }
                    });
                    if let Err(e) = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(TokioIo::new(sock), svc)
                        .await
                    {
                        note(&srv.daemon, &srv.id, Stream::Stderr, &format!("wasm http: {e}\n"));
                    }
                });
            }
        }
    }
    exit_code(killed, 0)
}

impl Srv {
    async fn respond(
        &self,
        req: hyper::Request<hyper::body::Incoming>,
    ) -> hyper::Response<HyperOutgoingBody> {
        let method = req.method().clone();
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| "/".to_owned());
        note(
            &self.daemon,
            &self.id,
            Stream::Stdout,
            &format!("{method} {}\n", one_line(&path)),
        );
        self.dispatch(req).await
    }

    async fn dispatch(
        &self,
        req: hyper::Request<hyper::body::Incoming>,
    ) -> hyper::Response<HyperOutgoingBody> {
        if self.killed.load(Ordering::SeqCst) != 0 {
            return text(503, "stopped\n");
        }
        let (parts, incoming) = req.into_parts();
        let collected = match incoming.collect().await {
            Ok(c) => c,
            Err(e) => return text(400, &format!("{e}\n")),
        };
        let body = collected.to_bytes();
        if body.len() > MAX_BODY {
            return text(413, "request body is too large\n");
        }
        let req = hyper::Request::from_parts(parts, full_body(body));
        let (stdout, stderr) = match (dup(&self.stdout), dup(&self.stderr)) {
            (Ok(stdout), Ok(stderr)) => (stdout, stderr),
            (Err(e), _) | (_, Err(e)) => return text(500, &format!("{e}\n")),
        };
        let wasi = match wasm::wasi_builder_for(
            &self.args,
            &self.env,
            &self.cwd,
            &self.preopens,
            wasm::WasiIo {
                stdin: None,
                stdout,
                stderr,
            },
        ) {
            Ok(mut builder) => builder.build(),
            Err(e) => return text(500, &format!("{e}\n")),
        };
        let mut store = Store::new(
            &self.engine,
            State {
                ctx: wasi,
                http: WasiHttpCtx::new(),
                table: ResourceTable::new(),
            },
        );
        store.set_epoch_deadline(1);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let req = match store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)
        {
            Ok(req) => req,
            Err(e) => {
                let msg = e.to_string();
                let status = if msg.contains("host") || msg.contains("authority") {
                    400
                } else {
                    500
                };
                return text(status, &format!("{msg}\n"));
            }
        };
        let out = match store.data_mut().http().new_response_outparam(sender) {
            Ok(out) => out,
            Err(e) => return text(500, &format!("{e}\n")),
        };
        let pre = self.pre.clone();
        // The guest keeps running after it sends the headers, so the body
        // can be written while hyper reads it. Dropping the handle detaches
        // that task; it is not aborted.
        let task = tokio::spawn(async move {
            let proxy = pre.instantiate_async(&mut store).await?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, req, out)
                .await?;
            Ok::<(), wasmtime::Error>(())
        });
        match receiver.await {
            Ok(Ok(resp)) => resp,
            Ok(Err(code)) => text(500, &format!("{code}\n")),
            Err(_) => {
                let msg = match task.await {
                    Ok(Ok(())) => "guest sent no response".to_owned(),
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                };
                text(500, &format!("{msg}\n"))
            }
        }
    }
}

fn dup(file: &Mutex<File>) -> io::Result<File> {
    file.lock().unwrap_or_else(|e| e.into_inner()).try_clone()
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

fn full_body(bytes: hyper::body::Bytes) -> HyperOutgoingBody {
    http_body_util::Full::new(bytes)
        .map_err(|never: Infallible| -> wasmtime_wasi_http::Error { match never {} })
        .boxed_unsync()
}

fn text(status: u16, msg: &str) -> hyper::Response<HyperOutgoingBody> {
    let body = full_body(hyper::body::Bytes::from(msg.to_owned()));
    hyper::Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body)
        .unwrap_or_else(|_| hyper::Response::new(full_body(hyper::body::Bytes::new())))
}
