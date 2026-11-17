//! A wasi:http incoming-handler for Kiri's tests. The response body is
//! `METHOD path` plus the `x-echo` header and the request body, so a client
//! can see that method, path, a header and the body arrived.
//!
//! Rebuild (Rust 1.97+, the `wasm32-wasip2` target) and replace the blob the
//! daemon tests load:
//!
//! ```text
//! rustup target add wasm32-wasip2
//! cargo build --manifest-path tests/wasm/http/Cargo.toml --target wasm32-wasip2 --release
//! cp tests/wasm/http/target/wasm32-wasip2/release/kiri_wasm_http.wasm tests/wasm/http.wasm
//! ```

// The `wasi` crate's proxy macro emits the incoming-handler export.
wasi::http::proxy::export!(Component);

struct Component;

impl wasi::exports::http::incoming_handler::Guest for Component {
    fn handle(request: wasi::http::types::IncomingRequest, response_out: wasi::http::types::ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_else(|| "/".to_owned());
        let method = method_name(&request.method());
        let echo = header(&request.headers(), "x-echo");
        let incoming = request.consume().ok();
        let body = read_body(incoming.as_ref());
        let text = format!("{method} {path}\n{echo}{body}");

        let headers = wasi::http::types::Headers::new();
        let _ = headers.append("content-type", b"text/plain");
        let response = wasi::http::types::OutgoingResponse::new(headers);
        let _ = response.set_status_code(200);
        let out = response.body().expect("response body");
        wasi::http::types::ResponseOutparam::set(response_out, Ok(response));
        if let Ok(stream) = out.write() {
            let _ = stream.blocking_write_and_flush(text.as_bytes());
            drop(stream);
        }
        let _ = wasi::http::types::OutgoingBody::finish(out, None);
    }
}

fn method_name(method: &wasi::http::types::Method) -> String {
    use wasi::http::types::Method;
    match method {
        Method::Get => "GET".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Head => "HEAD".into(),
        Method::Options => "OPTIONS".into(),
        Method::Patch => "PATCH".into(),
        Method::Connect => "CONNECT".into(),
        Method::Trace => "TRACE".into(),
        Method::Other(name) => name.clone(),
    }
}

fn header(headers: &wasi::http::types::Headers, name: &str) -> String {
    headers
        .get(name)
        .into_iter()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .collect()
}

fn read_body(body: Option<&wasi::http::types::IncomingBody>) -> String {
    let Some(body) = body else { return String::new() };
    let Ok(stream) = body.stream() else { return String::new() };
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
