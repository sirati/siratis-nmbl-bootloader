//! URL parsing for the rescue HTTP client.
//!
//! Accepts only `http://host[:port][/path]`; see [`HttpUrl::parse`] for
//! the full grammar and security constraints.

use crate::error::{NmblError, Result};

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
    /// URLs with userinfo, empty hosts, or malformed ports. Also
    /// rejects any ASCII control byte (`< 0x20` or `== 0x7f`) in the
    /// URL — those would otherwise be interpolated verbatim into the
    /// request line and Host header, letting a malicious URL forge
    /// HTTP headers (request smuggling).
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

        if let Some(bad) = rest.bytes().find(|b| *b < 0x20 || *b == 0x7f) {
            return Err(NmblError::Rescue {
                stage: "http-parse-url",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: format!("URL {input:?} contains control byte {bad:#04x}"),
                    context: "parsing rescue URL".to_string(),
                }),
            });
        }

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
