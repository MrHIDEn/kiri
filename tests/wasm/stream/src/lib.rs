//! A wasi:http incoming-handler that streams.
//!
//! `POST /upload` reads the body in chunks, hashes it, and replies with the
//! sha256 hex and the byte count. `GET /download?n=` writes `n` bytes of a
//! fixed pattern (default 4 MiB, at most 16 MiB) without buffering them.
//! The byte at index `i` is the low 8 bits of `i * 131 + 17`.
//!
//! Rebuild (Rust 1.97+, the `wasm32-wasip2` target) and replace the blob the
//! daemon tests load:
//!
//! ```text
//! rustup target add wasm32-wasip2
//! cargo build --manifest-path tests/wasm/stream/Cargo.toml --target wasm32-wasip2 --release
//! cp tests/wasm/stream/target/wasm32-wasip2/release/kiri_wasm_stream.wasm tests/wasm/stream.wasm
//! ```

use sha2::{Digest, Sha256};

wasi::http::proxy::export!(Component);

struct Component;

impl wasi::exports::http::incoming_handler::Guest for Component {
    fn handle(
        request: wasi::http::types::IncomingRequest,
        response_out: wasi::http::types::ResponseOutparam,
    ) {
        let path = request.path_with_query().unwrap_or_else(|| "/".to_owned());
        let path_only = path.split('?').next().unwrap_or("/");
        if path_only == "/download" {
            let n = query_u64(&path, "n").unwrap_or(4 * 1024 * 1024);
            download(n, response_out);
            return;
        }
        if path_only == "/upload" {
            match hash_body(request) {
                Ok((hex, n)) => reply(response_out, 200, &format!("{hex}\n{n}\n")),
                Err(e) => reply(response_out, 400, &format!("{e}\n")),
            }
            return;
        }
        reply(response_out, 404, "not found\n");
    }
}

fn query_u64(path: &str, key: &str) -> Option<u64> {
    let query = path.split_once('?')?.1;
    let prefix = format!("{key}=");
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix(&prefix) {
            return v.parse().ok();
        }
    }
    None
}

/// 16 MiB is under the host's default body limit, so a test can ask for a
/// large download and still have the host stream it.
const MAX_DOWNLOAD: u64 = 16 * 1024 * 1024;

fn download(n: u64, response_out: wasi::http::types::ResponseOutparam) {
    if n > MAX_DOWNLOAD {
        reply(response_out, 400, "n is too large\n");
        return;
    }
    let headers = wasi::http::types::Headers::new();
    let _ = headers.append("content-type", b"application/octet-stream");
    let response = wasi::http::types::OutgoingResponse::new(headers);
    let _ = response.set_status_code(200);
    let out = response.body().expect("response body");
    wasi::http::types::ResponseOutparam::set(response_out, Ok(response));
    let Ok(stream) = out.write() else {
        let _ = wasi::http::types::OutgoingBody::finish(out, None);
        return;
    };
    // wasi:io blocking-write-and-flush rejects a buffer larger than 4096.
    let mut i = 0u64;
    let mut buf = vec![0u8; 4096];
    while i < n {
        let len = ((n - i) as usize).min(buf.len());
        for b in &mut buf[..len] {
            *b = pattern(i);
            i += 1;
        }
        if stream.blocking_write_and_flush(&buf[..len]).is_err() {
            break;
        }
    }
    drop(stream);
    let _ = wasi::http::types::OutgoingBody::finish(out, None);
}

fn pattern(i: u64) -> u8 {
    i.wrapping_mul(131).wrapping_add(17) as u8
}

fn hash_body(request: wasi::http::types::IncomingRequest) -> Result<(String, u64), String> {
    let body = request.consume().map_err(|()| "no body".to_owned())?;
    let stream = body.stream().map_err(|()| "no body stream".to_owned())?;
    let mut hasher = Sha256::new();
    let mut n = 0u64;
    loop {
        match stream.blocking_read(16 * 1024) {
            Ok(chunk) if chunk.is_empty() => break,
            Ok(chunk) => {
                n += chunk.len() as u64;
                hasher.update(&chunk);
            }
            Err(wasi::io::streams::StreamError::Closed) => break,
            Err(e) => return Err(format!("{e:?}")),
        }
    }
    drop(stream);
    let _ = wasi::http::types::IncomingBody::finish(body);
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((hex, n))
}

fn reply(response_out: wasi::http::types::ResponseOutparam, status: u16, text: &str) {
    let headers = wasi::http::types::Headers::new();
    let _ = headers.append("content-type", b"text/plain");
    let len = text.len().to_string();
    let _ = headers.append("content-length", len.as_bytes());
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
