//! Handshake wire codec and the shared protocol constants.
//!
//! The handshake codec is little-endian and explicit:
//! `[term_len: u16][term: term_len bytes][rows: u16][cols: u16]`.

use std::os::fd::OwnedFd;

/// Status byte the server sends to acknowledge a root peer.
pub(super) const STATUS_OK: u8 = b'K';
/// Status byte the server sends to reject a non-root peer.
pub(super) const STATUS_NO: u8 = b'N';
/// Reject message body written after [`STATUS_NO`].
pub(super) const REJECT_MSG: &[u8] = b"you are not root\n";
/// Upper bound on the handshake TERM string, so a hostile/garbled peer
/// can't make us allocate unbounded ancillary/data buffers.
pub(super) const MAX_TERM_LEN: usize = 256;

/// Decoded handshake the client sends alongside its pty fd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// The client's `$TERM` (e.g. `xterm-256color`). May be empty.
    pub term: String,
    /// Terminal geometry: `(rows, cols)`.
    pub winsize: (u16, u16),
}

impl Handshake {
    /// Encode to the little-endian wire form.
    /// `[term_len: u16][term bytes][rows: u16][cols: u16]`.
    pub fn encode(&self) -> Vec<u8> {
        let term = self.term.as_bytes();
        let clamped = term.len().min(MAX_TERM_LEN);
        let term_len = u16::try_from(clamped).unwrap_or(u16::MAX);
        let term = term.get(..clamped).unwrap_or(term);
        let mut out = Vec::with_capacity(2 + term.len() + 4);
        out.extend_from_slice(&term_len.to_le_bytes());
        out.extend_from_slice(term);
        out.extend_from_slice(&self.winsize.0.to_le_bytes());
        out.extend_from_slice(&self.winsize.1.to_le_bytes());
        out
    }

    /// Decode from the little-endian wire form. Returns `None` if the
    /// buffer is truncated, the TERM length is implausible, or the TERM
    /// bytes are not valid UTF-8.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        let term_len = u16::from_le_bytes([*buf.first()?, *buf.get(1)?]) as usize;
        if term_len > MAX_TERM_LEN {
            return None;
        }
        let term_bytes = buf.get(2..2 + term_len)?;
        let term = std::str::from_utf8(term_bytes).ok()?.to_string();
        let rows_off = 2 + term_len;
        let rows = u16::from_le_bytes([*buf.get(rows_off)?, *buf.get(rows_off + 1)?]);
        let cols = u16::from_le_bytes([*buf.get(rows_off + 2)?, *buf.get(rows_off + 3)?]);
        Some(Self {
            term,
            winsize: (rows, cols),
        })
    }

    /// Maximum encoded length, used to size the recv data buffer.
    pub(super) const fn max_encoded_len() -> usize {
        2 + MAX_TERM_LEN + 4
    }
}

/// Everything PID 1 (Phase 3) needs to serve one remote TUI session:
/// the client's pty fd plus its declared terminal environment.
#[derive(Debug)]
pub struct RemoteHandle {
    /// The client's controlling-terminal fd, received via `SCM_RIGHTS`.
    pub pty: OwnedFd,
    /// The client's `$TERM`.
    pub term: String,
    /// The client's terminal geometry `(rows, cols)`.
    pub winsize: (u16, u16),
}
