//! Root-only Unix-socket transport for the remote TUI.
//!
//! Wire protocol (one round-trip per connection):
//!
//! 1. Client connects, then `sendmsg`s a single datagram-style message:
//!    the handshake payload [`Handshake`] in the data portion plus its
//!    controlling-terminal `OwnedFd` in an `SCM_RIGHTS` control message.
//! 2. Server checks `SO_PEERCRED`. If the peer is not uid 0 it writes
//!    `b"N"` + a human-readable reason and drops the connection.
//! 3. Otherwise the server `recvmsg`s the fd + handshake, writes `b"K"`
//!    to acknowledge, and returns a [`RemoteHandle`] to the caller
//!    (Phase 3), which then serves a TUI on the received pty.
//! 4. The client reads the 1-byte status. `b'K'` → go quiescent and
//!    block until the server closes the socket (EOF); `b'N'` → print the
//!    reason and exit non-zero.
//!
//! The handshake codec is little-endian and explicit:
//! `[term_len: u16][term: term_len bytes][rows: u16][cols: u16]`.

mod client;
mod codec;
mod server;

pub use client::{
    TUI_SOCK_DIR, TUI_SOCK_ENV, TUI_SOCK_PATH, TUI_SOCK_PATH_CHROOT, connect_and_serve,
};
pub use codec::{Handshake, RemoteHandle};
pub use server::{authenticate_and_receive, bind_listener};

// Re-exports for the unit tests' `use super::*` (internal items they
// assert on: constants, the codec helpers, and the recv/send/verify fns).
#[cfg(test)]
use client::send_fd_and_handshake;
#[cfg(test)]
use codec::{MAX_TERM_LEN, REJECT_MSG, STATUS_NO};
#[cfg(test)]
use server::{recv_fd_and_handshake, verify_run_dir};

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "tests.rs"]
mod tests;
