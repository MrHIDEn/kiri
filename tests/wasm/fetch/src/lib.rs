//! A wasi:cli command that GETs the URL in argv[1] and writes the response
//! body to stdout. It imports `wasi:http/outgoing-handler` and does not
//! export `wasi:http/incoming-handler`, so Kiri runs it once and exits.
//!
//! The connection is made by the host. In Kiri that host is the daemon, so
//! `127.0.0.1` is the machine running Kiri.
//!
//! Rebuild (Rust 1.97+, the `wasm32-wasip2` target) and replace the blob the
//! daemon tests load:
//!
//! ```text
//! rustup target add wasm32-wasip2
//! cargo build --manifest-path tests/wasm/fetch/Cargo.toml --target wasm32-wasip2 --release
//! cp tests/wasm/fetch/target/wasm32-wasip2/release/kiri_wasm_fetch.wasm tests/wasm/fetch.wasm
//! ```

wasi::cli::command::export!(Fetch);

struct Fetch;

impl wasi::exports::cli::run::Guest for Fetch {
    fn run() -> Result<(), ()> {
        let args = wasi::cli::environment::get_arguments();
        let Some(url) = args.get(1) else {
            let _ = write_all(&wasi::cli::stderr::get_stderr(), b"usage: fetch <url>\n");
            return Err(());
        };
        match http_get(url) {
            Ok(got) => {
                let _ = write_all(&wasi::cli::stdout::get_stdout(), got.body.as_bytes());
                Ok(())
            }
            Err(e) => {
                let _ = write_all(&wasi::cli::stderr::get_stderr(), e.as_bytes());
                Err(())
            }
        }
    }
}

struct Got {
    body: String,
}

fn http_get(url: &str) -> Result<Got, String> {
    let (scheme, authority, path) = split_url(url)?;
    let request = wasi::http::types::OutgoingRequest::new(wasi::http::types::Headers::new());
    request
        .set_scheme(Some(&scheme))
        .map_err(|()| "bad scheme".to_owned())?;
    request
        .set_authority(Some(&authority))
        .map_err(|()| "bad authority".to_owned())?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| "bad path".to_owned())?;
    let future = wasi::http::outgoing_handler::handle(request, None).map_err(|e| e.to_string())?;
    future.subscribe().block();
    let response = match future.get() {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(code))) => return Err(code.to_string()),
        Some(Err(())) | None => return Err("no response".to_owned()),
    };
    let body = match response.consume() {
        Ok(body) => read_body(&body),
        Err(()) => String::new(),
    };
    Ok(Got { body })
}

fn split_url(url: &str) -> Result<(wasi::http::types::Scheme, String, String), String> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (wasi::http::types::Scheme::Https, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (wasi::http::types::Scheme::Http, rest)
    } else {
        return Err("url must be http or https".to_owned());
    };
    if rest.is_empty() {
        return Err("url has no host".to_owned());
    }
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => match rest.split_once('?') {
            Some((authority, query)) => (authority, format!("?{query}")),
            None => (rest, "/".to_owned()),
        },
    };
    if authority.is_empty() {
        return Err("url has no host".to_owned());
    }
    Ok((scheme, authority.to_owned(), path))
}

fn read_body(body: &wasi::http::types::IncomingBody) -> String {
    let Ok(stream) = body.stream() else {
        return String::new();
    };
    let mut out = Vec::new();
    loop {
        match stream.blocking_read(4096) {
            Ok(chunk) if chunk.is_empty() => break,
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(_) => break,
        }
        if out.len() > 64 * 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_all(stream: &wasi::io::streams::OutputStream, bytes: &[u8]) -> Result<(), ()> {
    stream.blocking_write_and_flush(bytes).map_err(|_| ())
}
