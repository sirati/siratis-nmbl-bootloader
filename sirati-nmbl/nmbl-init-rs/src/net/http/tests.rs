//! Integration and unit tests for the HTTP client.

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::error::{NmblError, Result};
use crate::net::http::url::HttpUrl;
use crate::net::http::{get, response};
use response::{Headers, parse_content_length, parse_status_line, read_headers};

#[test]
fn parse_url_default_port_and_path() {
    let u = HttpUrl::parse("http://example.com/").expect("parse");
    assert_eq!(u.host, "example.com");
    assert_eq!(u.port, 80);
    assert_eq!(u.path, "/");
}

#[test]
fn parse_url_explicit_port_and_path() {
    let u = HttpUrl::parse("http://1.2.3.4:8080/foo").expect("parse");
    assert_eq!(u.host, "1.2.3.4");
    assert_eq!(u.port, 8080);
    assert_eq!(u.path, "/foo");
}

#[test]
fn parse_url_missing_path_defaults_to_slash() {
    let u = HttpUrl::parse("http://example.com").expect("parse");
    assert_eq!(u.host, "example.com");
    assert_eq!(u.port, 80);
    assert_eq!(u.path, "/");
}

#[test]
fn parse_url_missing_path_with_port() {
    let u = HttpUrl::parse("http://example.com:1234").expect("parse");
    assert_eq!(u.host, "example.com");
    assert_eq!(u.port, 1234);
    assert_eq!(u.path, "/");
}

#[test]
fn parse_url_preserves_query_in_path() {
    let u = HttpUrl::parse("http://h/foo?bar=baz").expect("parse");
    assert_eq!(u.host, "h");
    assert_eq!(u.path, "/foo?bar=baz");
}

#[test]
fn parse_url_rejects_https() {
    let e = HttpUrl::parse("https://example.com/").expect_err("https must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_missing_host() {
    let e = HttpUrl::parse("http:///foo").expect_err("empty host must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_bad_port() {
    let e = HttpUrl::parse("http://h:abc/").expect_err("bad port must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_userinfo() {
    let e = HttpUrl::parse("http://user@host/").expect_err("userinfo must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_ipv6_literal() {
    let e = HttpUrl::parse("http://[::1]/").expect_err("ipv6 must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_crlf_injection() {
    // An attacker-pasted URL that smuggles CR/LF into the Host
    // header would otherwise let them inject a second HTTP
    // request after the legitimate one (request smuggling).
    let e = HttpUrl::parse("http://evil.example.com\r\nX-Injected: 1/path")
        .expect_err("crlf must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_control_byte_in_path() {
    let e = HttpUrl::parse("http://example.com/\x00bad").expect_err("nul byte must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_url_rejects_del_byte() {
    let e = HttpUrl::parse("http://example.com/\x7f").expect_err("DEL byte must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-parse-url"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn status_line_parses_http10_200() {
    assert_eq!(parse_status_line("HTTP/1.0 200 OK\r\n").unwrap(), 200);
}

#[test]
fn status_line_parses_http11_404() {
    assert_eq!(
        parse_status_line("HTTP/1.1 404 Not Found\r\n").unwrap(),
        404
    );
}

#[test]
fn status_line_rejects_unsupported_version() {
    let e = parse_status_line("HTTP/2.0 200 OK\r\n").expect_err("h2 must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-recv-status"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn status_line_rejects_garbage() {
    let e = parse_status_line("ssh-blob\r\n").expect_err("garbage must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-recv-status"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn status_line_rejects_non_numeric_code() {
    let e = parse_status_line("HTTP/1.0 OK OK\r\n").expect_err("non-numeric must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-recv-status"),
        other => panic!("wrong variant: {other:?}"),
    }
}

fn read_headers_from(bytes: &[u8]) -> Result<Headers> {
    let mut r = std::io::BufReader::new(bytes);
    read_headers(&mut r)
}

#[test]
fn headers_parse_case_insensitive_content_length() {
    let raw = b"content-LENGTH: 42\r\nServer: x\r\n\r\n";
    let h = read_headers_from(raw).expect("parse headers");
    assert_eq!(parse_content_length(&h).unwrap(), Some(42));
}

#[test]
fn headers_missing_content_length_returns_none() {
    let raw = b"Server: x\r\n\r\n";
    let h = read_headers_from(raw).expect("parse headers");
    assert_eq!(parse_content_length(&h).unwrap(), None);
}

#[test]
fn headers_reject_chunked_transfer_encoding() {
    let raw = b"Transfer-Encoding: chunked\r\n\r\n";
    let h = read_headers_from(raw).expect("parse headers");
    let e = parse_content_length(&h).expect_err("chunked must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-recv-headers"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn headers_reject_chunked_in_list() {
    let raw = b"Transfer-Encoding: gzip, chunked\r\n\r\n";
    let h = read_headers_from(raw).expect("parse headers");
    assert!(parse_content_length(&h).is_err());
}

#[test]
fn headers_reject_malformed_content_length() {
    let raw = b"Content-Length: abc\r\n\r\n";
    let h = read_headers_from(raw).expect("parse headers");
    let e = parse_content_length(&h).expect_err("bad cl must fail");
    match e {
        NmblError::Rescue { stage, .. } => assert_eq!(stage, "http-recv-headers"),
        other => panic!("wrong variant: {other:?}"),
    }
}

/// Helper: spawn a one-shot HTTP/1.0 origin on 127.0.0.1:0 that
/// serves `payload` with the supplied status line + headers and
/// closes the connection. Returns the bound port and the join
/// handle so the test can `.join()` cleanly.
fn spawn_origin(
    status_line: &'static str,
    extra_headers: &'static str,
    payload: Vec<u8>,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        // Drain the request — we don't validate it here, just
        // need to consume bytes so the client doesn't block on
        // its write half.
        let mut buf = [0u8; 1024];
        // Best-effort single read; the request fits in one MTU.
        let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = sock.read(&mut buf);
        let header = format!("{status_line}\r\n{extra_headers}\r\n");
        use std::io::Write;
        sock.write_all(header.as_bytes()).expect("write headers");
        sock.write_all(&payload).expect("write body");
        // Drop the socket → FIN → client sees EOF.
    });
    (port, handle)
}

#[test]
fn get_streams_body_with_content_length() {
    let payload = b"hello, rescue world".to_vec();
    let cl = payload.len();
    let (port, handle) = spawn_origin(
        "HTTP/1.0 200 OK",
        // We must write CL as a runtime value, so encode the
        // exact length manually below by concatenating.
        "Content-Length: 19\r\nServer: test\r\n",
        payload.clone(),
    );
    assert_eq!(cl, 19, "test payload length drift");
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/x")).expect("parse url");

    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    let progress_seen = Arc::new(Mutex::new(Vec::<u64>::new()));

    let recv_clone = Arc::clone(&received);
    let prog_clone = Arc::clone(&progress_seen);
    let mut progress_cb = move |total: u64| {
        prog_clone.lock().expect("mutex").push(total);
    };
    let n = get(
        &url,
        |chunk: &[u8]| {
            recv_clone.lock().expect("mutex").extend_from_slice(chunk);
            Ok(())
        },
        Some(&mut progress_cb),
    )
    .expect("get");

    handle.join().expect("origin thread");
    assert_eq!(n as usize, payload.len());
    assert_eq!(*received.lock().expect("mutex"), payload);
    assert_eq!(*progress_seen.lock().expect("mutex"), vec![19u64]);
}

#[test]
fn get_drains_to_eof_when_no_content_length() {
    let payload = b"abcdefghij".to_vec();
    let (port, handle) = spawn_origin("HTTP/1.0 200 OK", "Server: test\r\n", payload.clone());
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/eof")).expect("parse url");

    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    let recv_clone = Arc::clone(&received);
    let n = get(
        &url,
        |chunk: &[u8]| {
            recv_clone.lock().expect("mutex").extend_from_slice(chunk);
            Ok(())
        },
        None,
    )
    .expect("get");
    handle.join().expect("origin thread");
    assert_eq!(n as usize, payload.len());
    assert_eq!(*received.lock().expect("mutex"), payload);
}

#[test]
fn get_rejects_non_200_status() {
    let (port, handle) = spawn_origin(
        "HTTP/1.0 404 Not Found",
        "Content-Length: 0\r\n",
        Vec::new(),
    );
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/missing")).expect("parse url");

    let res = get(&url, |_chunk: &[u8]| Ok(()), None);
    handle.join().expect("origin thread");
    match res {
        Err(NmblError::Rescue { stage, .. }) => assert_eq!(stage, "http-bad-status"),
        other => panic!("expected http-bad-status, got {other:?}"),
    }
}

#[test]
fn get_rejects_chunked_transfer_encoding() {
    let (port, handle) = spawn_origin(
        "HTTP/1.1 200 OK",
        "Transfer-Encoding: chunked\r\n",
        // Body would never be read because chunked is rejected
        // at the header stage, but keep something here.
        b"0\r\n\r\n".to_vec(),
    );
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/chunked")).expect("parse url");

    let res = get(&url, |_chunk: &[u8]| Ok(()), None);
    handle.join().expect("origin thread");
    match res {
        Err(NmblError::Rescue { stage, .. }) => assert_eq!(stage, "http-recv-headers"),
        other => panic!("expected http-recv-headers, got {other:?}"),
    }
}

#[test]
fn get_reports_short_body_as_error() {
    // Declare 1000 bytes but only send 10 — the client should
    // detect EOF before reaching the declared count.
    let (port, handle) = spawn_origin(
        "HTTP/1.0 200 OK",
        "Content-Length: 1000\r\n",
        b"too-short!".to_vec(),
    );
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/short")).expect("parse url");

    let res = get(&url, |_chunk: &[u8]| Ok(()), None);
    handle.join().expect("origin thread");
    match res {
        Err(NmblError::Rescue { stage, .. }) => assert_eq!(stage, "http-recv-body"),
        other => panic!("expected http-recv-body, got {other:?}"),
    }
}

#[test]
fn get_truncates_when_origin_overshoots_content_length() {
    // Declare 5 bytes but send 20 — sink should see exactly 5.
    let payload = b"AAAAABBBBBCCCCCDDDDD".to_vec();
    let (port, handle) = spawn_origin("HTTP/1.0 200 OK", "Content-Length: 5\r\n", payload);
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/over")).expect("parse url");

    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    let recv_clone = Arc::clone(&received);
    let n = get(
        &url,
        |chunk: &[u8]| {
            recv_clone.lock().expect("mutex").extend_from_slice(chunk);
            Ok(())
        },
        None,
    )
    .expect("get");
    handle.join().expect("origin thread");
    assert_eq!(n, 5);
    assert_eq!(received.lock().expect("mutex").as_slice(), b"AAAAA");
}
