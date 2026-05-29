//! HTTP response parsing: status line, headers, and body streaming.

use std::io::{BufRead, Read};

use crate::error::{NmblError, Result};

/// Cap on header section size. 64 KiB is several orders of magnitude
/// larger than anything a sane origin will send and bounds the
/// memory a malicious peer can force us to allocate before we even
/// see the body.
pub(crate) const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Collected header lines (case-preserved). Stored as raw strings so
/// the value can be re-parsed by individual extractors.
pub(crate) type Headers = Vec<(String, String)>;

/// Read and parse the status line, returning the numeric status
/// code. Accepts both `HTTP/1.0` and `HTTP/1.1` replies because some
/// origins always send 1.1 regardless of the request version.
///
/// The read is capped at [`MAX_HEADER_BYTES`] so a malicious peer
/// that holds the socket open feeding a single unterminated line
/// can't drain the TCP read timeout indefinitely.
pub(crate) fn read_status_line<R: BufRead>(reader: &mut R) -> Result<u16> {
    let mut buf: Vec<u8> = Vec::new();
    let n = reader
        .take(MAX_HEADER_BYTES as u64)
        .read_until(b'\n', &mut buf)
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
    if !buf.ends_with(b"\n") {
        return Err(NmblError::Rescue {
            stage: "http-recv-status",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!("status line exceeded {MAX_HEADER_BYTES} bytes"),
                context: "reading status line".to_string(),
            }),
        });
    }
    let line = std::str::from_utf8(&buf).map_err(|_| NmblError::Rescue {
        stage: "http-recv-status",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "status line contains non-UTF-8 bytes".to_string(),
            context: "reading status line".to_string(),
        }),
    })?;
    parse_status_line(line)
}

/// Pure parser separated for unit-testability. Returns the status
/// code on success.
pub(crate) fn parse_status_line(line: &str) -> Result<u16> {
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

/// Read headers up to (but not including) the blank `CRLF CRLF`
/// terminator. Caps total bytes at `MAX_HEADER_BYTES` to avoid an
/// allocation-DoS.
pub(crate) fn read_headers<R: BufRead>(reader: &mut R) -> Result<Headers> {
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
pub(crate) fn parse_content_length(headers: &Headers) -> Result<Option<u64>> {
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
pub(crate) fn stream_body<R, W>(reader: &mut R, expected: Option<u64>, sink: &mut W) -> Result<u64>
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
