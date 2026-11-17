//! A wasi:http incoming-handler for Kiri's tests. The response body is
//! `METHOD path` plus the `x-echo` header and the request body, so a client
//! can see that method, path, a header and the body arrived.
//!
//! An `x-fetch` header that is an `http` or `https` URL is fetched with
//! `wasi:http/outgoing-handler` and that response (status and body) is
//! returned instead. The connection is the host's: in Kiri, the daemon's.
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
        let fetch_url = header(&request.headers(), "x-fetch");
        if !fetch_url.is_empty() {
            match http_get(&fetch_url) {
                Ok(got) => reply(response_out, got.status, &got.body),
                Err(e) => reply(response_out, 502, &e),
            }
            return;
        }
        let path = request.path_with_query().unwrap_or_else(|| "/".to_owned());
        let method = method_name(&request.method());
        let echo = header(&request.headers(), "x-echo");
        let incoming = request.consume().ok();
        let body = read_body(incoming.as_ref());
        reply(response_out, 200, &format!("{method} {path}\n{echo}{body}"));
    }
}

struct Got {
    status: u16,
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
    let status = response.status();
    let body = match response.consume() {
        Ok(body) => read_body(Some(&body)),
        Err(()) => String::new(),
    };
    Ok(Got { status, body })
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

fn reply(response_out: wasi::http::types::ResponseOutparam, status: u16, text: &str) {
    let headers = wasi::http::types::Headers::new();
    let _ = headers.append("content-type", b"text/plain");
    let response = wasi::http::types::OutgoingResponse::new(headers);
    let _ = response.set_status_code(status);
    let out = response.body().expect("response body");
    wasi::http::types::ResponseOutparam::set(response_out, Ok(response));
    if let Ok(stream) = out.write() {
        let _ = stream.blocking_write_and_flush(text.as_bytes());
        drop(stream);
    }
    let _ = wasi::http::types::OutgoingBody::finish(out, None);
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
