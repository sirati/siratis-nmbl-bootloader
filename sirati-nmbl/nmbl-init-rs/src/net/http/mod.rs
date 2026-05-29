//! Hand-rolled HTTP/1.0 GET client used by the rescue network
//! fallback (Phase D.3). Streams the response body chunk-by-chunk
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

pub(crate) mod response;
mod url;

pub use url::HttpUrl;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests;

use std::io::BufReader;
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{NmblError, Result};
use response::{parse_content_length, read_headers, read_status_line, stream_body};

/// Read/write timeout applied to the TCP stream. Defends against a
/// hung peer that opens the socket but never sends a byte. 30s is
/// long enough to ride out a brief hiccup mid-download but short
/// enough that the operator's rescue path doesn't wedge for minutes.
const TCP_TIMEOUT: Duration = Duration::from_secs(30);

/// User-Agent we advertise in the request. Operators occasionally
/// firewall on UA in front of rescue mirrors, so make it
/// recognizable rather than blank.
const USER_AGENT: &str = "nmbl-rescue/1";

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
    use std::io::Write;
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
