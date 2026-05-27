//! Hand-rolled HTTP/1.0 GET client used by the rescue network
//! fallback (Phase E.1). Streams the response body chunk-by-chunk
//! through a caller-supplied sink so the operator can SHA-256 the
//! payload and write it to a memfd in a single pass — no full-body
//! buffering happens in this module.
//!
//! Why HTTP/1.0 and not 1.1? HTTP/1.0 servers must respond without
//! chunked transfer encoding and may close the connection to signal
//! EOF, which lets us avoid implementing chunked framing or
//! persistent-connection accounting. The request still advertises a
//! `Host:` header so virtual-hosted origins work.
//!
//! Why no URL crate? The static-musl build budget is tight and the
//! URL grammar we accept is a strict subset (`http://host[:port][/path]`,
//! no auth, no query parsing beyond passing it through, no fragment,
//! no IPv6 literal). Hand parsing keeps the dep graph small.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{NmblError, Result};

/// Read/write timeout applied to the TCP stream. Defends against a
/// hung peer that opens the socket but never sends a byte. 30s is
/// long enough to ride out a brief hiccup mid-download but short
/// enough that the operator's rescue path doesn't wedge for minutes.
const TCP_TIMEOUT: Duration = Duration::from_secs(30);

/// User-Agent we advertise in the request. Operators occasionally
/// firewall on UA in front of rescue mirrors, so make it
/// recognizable rather than blank.
const USER_AGENT: &str = "nmbl-rescue/1";

/// Cap on header section size. 64 KiB is several orders of magnitude
/// larger than anything a sane origin will send and bounds the
/// memory a malicious peer can force us to allocate before we even
/// see the body.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Parsed HTTP URL. Only the bits the client cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUrl {
    /// Hostname or IPv4 literal (no IPv6 brackets — unsupported).
    pub host: String,
    /// TCP port. Defaults to 80 when the URL omits one.
    pub port: u16,
    /// Request-target path. Always normalized to start with `/`.
    pub path: String,
}

impl HttpUrl {
    /// Parse `http://host[:port][/path]`. Rejects `https://`,
    /// URLs with userinfo, empty hosts, or malformed ports.
    pub fn parse(input: &str) -> Result<Self> {
        const SCHEME: &str = "http://";
        let rest = input
            .strip_prefix(SCHEME)
            .ok_or_else(|| NmblError::Rescue {
                stage: "http-parse-url",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("URL {input:?} must start with http://"),
                    context: "parsing rescue URL".to_string(),
                }),
            })?;

        // Reject userinfo — we don't ship a Basic-Auth implementation.
        if rest.contains('@') {
            return Err(NmblError::Rescue {
                stage: "http-parse-url",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("URL {input:?} contains userinfo; not supported"),
                    context: "parsing rescue URL".to_string(),
                }),
            });
        }

        // Reject IPv6 literals (square brackets) — also unsupported.
        if rest.starts_with('[') {
            return Err(NmblError::Rescue {
                stage: "http-parse-url",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("URL {input:?} uses IPv6 literal; not supported"),
                    context: "parsing rescue URL".to_string(),
                }),
            });
        }

        // Split host[:port] from the path on the first '/'. Anything
        // following (including a query string) becomes the path
        // verbatim — origins parse it themselves.
        let (authority, path) = match rest.find('/') {
            Some(idx) => {
                // Safe: `idx` came from `find` on the same byte string.
                let (a, p) = rest.split_at(idx);
                (a, p.to_string())
            }
            None => (rest, "/".to_string()),
        };

        if authority.is_empty() {
            return Err(NmblError::Rescue {
                stage: "http-parse-url",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("URL {input:?} is missing a host"),
                    context: "parsing rescue URL".to_string(),
                }),
            });
        }

        let (host, port) = match authority.rfind(':') {
            Some(idx) => {
                let (h, p) = authority.split_at(idx);
                // Strip the leading ':' from the port slice.
                let p = p.get(1..).unwrap_or("");
                let port = p.parse::<u16>().map_err(|_| NmblError::Rescue {
                    stage: "http-parse-url",
                    source: Box::new(NmblError::ConfigInvalid {
                        reason: format!("URL {input:?} has invalid port {p:?}"),
                        context: "parsing rescue URL".to_string(),
                    }),
                })?;
                (h, port)
            }
            None => (authority, 80u16),
        };

        if host.is_empty() {
            return Err(NmblError::Rescue {
                stage: "http-parse-url",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("URL {input:?} has empty host"),
                    context: "parsing rescue URL".to_string(),
                }),
            });
        }

        Ok(Self {
            host: host.to_string(),
            port,
            path,
        })
    }
}

/// HTTP GET. Streams the response body through `sink`, returning
/// the total bytes received. On non-200 status, returns
/// `NmblError::Rescue { stage: "http-bad-status", ... }` with the
/// status code in the message.
///
/// `sink` is called incrementally with chunks; the caller can use it
/// to feed a SHA-256 hasher and a `memfd_write` in one pass. When a
/// `Content-Length` header is present, `progress(total)` fires once
/// before the first body chunk so the UI can render a percentage.
pub fn get<W>(url: &HttpUrl, mut sink: W, mut progress: Option<&mut dyn FnMut(u64)>) -> Result<u64>
where
    W: FnMut(&[u8]) -> Result<()>,
{
    let addr = format!("{}:{}", url.host, url.port);
    let stream = TcpStream::connect(&addr).map_err(|source| NmblError::Rescue {
        stage: "http-connect",
        source: Box::new(NmblError::Io {
            source,
            context: format!("TCP connect to {addr}"),
        }),
    })?;
    stream
        .set_read_timeout(Some(TCP_TIMEOUT))
        .map_err(|source| NmblError::Rescue {
            stage: "http-connect",
            source: Box::new(NmblError::Io {
                source,
                context: format!("set_read_timeout on {addr}"),
            }),
        })?;
    stream
        .set_write_timeout(Some(TCP_TIMEOUT))
        .map_err(|source| NmblError::Rescue {
            stage: "http-connect",
            source: Box::new(NmblError::Io {
                source,
                context: format!("set_write_timeout on {addr}"),
            }),
        })?;

    send_request(&stream, url)?;

    let mut reader = BufReader::new(stream);
    let status = read_status_line(&mut reader)?;
    if status != 200 {
        return Err(NmblError::Rescue {
            stage: "http-bad-status",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!("HTTP status {status} from {}", url.host),
                context: format!("GET http://{}{}", url.host, url.path),
            }),
        });
    }

    let headers = read_headers(&mut reader)?;
    let content_length = parse_content_length(&headers)?;

    if let (Some(total), Some(cb)) = (content_length, progress.as_mut()) {
        cb(total);
    }

    stream_body(&mut reader, content_length, &mut sink)
}

/// Send the HTTP/1.0 request line + headers. A separate function so
/// the error stage is unambiguous.
fn send_request(mut stream: &TcpStream, url: &HttpUrl) -> Result<()> {
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: {ua}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path = url.path,
        host = url.host,
        ua = USER_AGENT,
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|source| NmblError::Rescue {
            stage: "http-send",
            source: Box::new(NmblError::Io {
                source,
                context: format!("writing request to {}:{}", url.host, url.port),
            }),
        })
}

/// Read and parse the status line, returning the numeric status
/// code. Accepts both `HTTP/1.0` and `HTTP/1.1` replies because some
/// origins always send 1.1 regardless of the request version.
fn read_status_line<R: BufRead>(reader: &mut R) -> Result<u16> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|source| NmblError::Rescue {
            stage: "http-recv-status",
            source: Box::new(NmblError::Io {
                source,
                context: "reading status line".to_string(),
            }),
        })?;
    if n == 0 {
        return Err(NmblError::Rescue {
            stage: "http-recv-status",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "peer closed before status line".to_string(),
                context: "reading status line".to_string(),
            }),
        });
    }
    parse_status_line(&line)
}

/// Pure parser separated for unit-testability. Returns the status
/// code on success.
fn parse_status_line(line: &str) -> Result<u16> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    // Reason phrase (parts.next()) is allowed to be absent — RFC
    // 7230 says SP can be followed by an empty reason.

    if !(version == "HTTP/1.0" || version == "HTTP/1.1") {
        return Err(NmblError::Rescue {
            stage: "http-recv-status",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!("unsupported HTTP version {version:?}"),
                context: format!("status line {trimmed:?}"),
            }),
        });
    }

    code.parse::<u16>().map_err(|_| NmblError::Rescue {
        stage: "http-recv-status",
        source: Box::new(NmblError::ConfigInvalid {
            reason: format!("malformed status code {code:?}"),
            context: format!("status line {trimmed:?}"),
        }),
    })
}

/// Collected header lines (case-preserved). Stored as raw strings so
/// the value can be re-parsed by individual extractors.
type Headers = Vec<(String, String)>;

/// Read headers up to (but not including) the blank `CRLF CRLF`
/// terminator. Caps total bytes at `MAX_HEADER_BYTES` to avoid an
/// allocation-DoS.
fn read_headers<R: BufRead>(reader: &mut R) -> Result<Headers> {
    let mut headers: Headers = Vec::new();
    let mut bytes_seen = 0usize;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|source| NmblError::Rescue {
                stage: "http-recv-headers",
                source: Box::new(NmblError::Io {
                    source,
                    context: "reading header line".to_string(),
                }),
            })?;
        if n == 0 {
            return Err(NmblError::Rescue {
                stage: "http-recv-headers",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: "peer closed mid-headers".to_string(),
                    context: "reading header section".to_string(),
                }),
            });
        }
        bytes_seen = bytes_seen.saturating_add(n);
        if bytes_seen > MAX_HEADER_BYTES {
            return Err(NmblError::Rescue {
                stage: "http-recv-headers",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("header section exceeded {MAX_HEADER_BYTES} bytes"),
                    context: "reading header section".to_string(),
                }),
            });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(idx) = trimmed.find(':') {
            let (name, value) = trimmed.split_at(idx);
            // value starts with ':'; drop it and any leading SP/HT.
            let value = value.get(1..).unwrap_or("").trim_start_matches([' ', '\t']);
            headers.push((name.to_string(), value.to_string()));
        }
        // Lines without ':' are tolerated and ignored — strict
        // parsing would buy us nothing here.
    }
    Ok(headers)
}

/// Pull `Content-Length` (case-insensitive). Returns:
///   - `Ok(Some(n))` when the header is present and parses,
///   - `Ok(None)` when the header is absent (HTTP/1.0 servers
///     often omit it and signal EOF by closing the socket),
///   - `Err(...)` when `Transfer-Encoding: chunked` is present
///     (we explicitly do not implement chunked framing) or
///     `Content-Length` is malformed.
fn parse_content_length(headers: &Headers) -> Result<Option<u64>> {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("transfer-encoding") {
            let v = value.to_ascii_lowercase();
            if v.split(',').any(|tok| tok.trim() == "chunked") {
                return Err(NmblError::Rescue {
                    stage: "http-recv-headers",
                    source: Box::new(NmblError::ConfigInvalid {
                        reason: "Transfer-Encoding: chunked is unsupported".to_string(),
                        context: "parsing response headers".to_string(),
                    }),
                });
            }
        }
    }
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            let v = value.trim();
            let n = v.parse::<u64>().map_err(|_| NmblError::Rescue {
                stage: "http-recv-headers",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("malformed Content-Length {v:?}"),
                    context: "parsing response headers".to_string(),
                }),
            })?;
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// Stream the response body through `sink`. When `expected` is
/// `Some`, stop after that many bytes and treat a short EOF as an
/// error. When `expected` is `None`, drain to EOF (HTTP/1.0
/// connection-close semantics).
fn stream_body<R, W>(reader: &mut R, expected: Option<u64>, sink: &mut W) -> Result<u64>
where
    R: Read,
    W: FnMut(&[u8]) -> Result<()>,
{
    // 16 KiB chunks: balances syscall overhead against memory floor
    // for a static-musl binary. memfd_write batches well at this
    // size and so does SHA-256.
    let mut buf = [0u8; 16 * 1024];
    let mut total: u64 = 0;
    loop {
        if let Some(want) = expected
            && total >= want
        {
            break;
        }
        let n = reader.read(&mut buf).map_err(|source| NmblError::Rescue {
            stage: "http-recv-body",
            source: Box::new(NmblError::Io {
                source,
                context: format!("reading body (so far {total} bytes)"),
            }),
        })?;
        if n == 0 {
            if let Some(want) = expected
                && total < want
            {
                return Err(NmblError::Rescue {
                    stage: "http-recv-body",
                    source: Box::new(NmblError::ConfigInvalid {
                        reason: format!("short body: got {total} of {want} declared bytes"),
                        context: "streaming response body".to_string(),
                    }),
                });
            }
            break;
        }
        // When the kernel hands us more than the declared length
        // (broken origin), trim to the announced size so the caller
        // sees a consistent byte count.
        let take = match expected {
            Some(want) => {
                let remaining = want.saturating_sub(total);
                let n_u64 = n as u64;
                if n_u64 > remaining {
                    remaining as usize
                } else {
                    n
                }
            }
            None => n,
        };
        if take == 0 {
            break;
        }
        let slice = buf.get(..take).unwrap_or(&[]);
        sink(slice)?;
        total = total.saturating_add(take as u64);
    }
    Ok(total)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

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
}
