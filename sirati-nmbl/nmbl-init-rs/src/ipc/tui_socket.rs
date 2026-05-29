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

use std::io::{self, IoSlice, IoSliceMut, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustix::fs::Mode;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tokio::net::{UnixListener, UnixStream};

/// Directory on the root tmpfs holding the TUI socket. Created 0700.
pub const TUI_SOCK_DIR: &str = "/nmbl-run";
/// Canonical path of the TUI control socket. The file is chmod 0600.
pub const TUI_SOCK_PATH: &str = "/nmbl-run/tui.sock";
/// Chrooted-rescue view of the same socket (Phase 4). Tried last by the
/// client when the canonical path is absent.
pub const TUI_SOCK_PATH_CHROOT: &str = "/nmbl-root/nmbl-run/tui.sock";
/// Env override for the client's socket path (highest precedence).
pub const TUI_SOCK_ENV: &str = "NMBL_TUI_SOCK";

/// Status byte the server sends to acknowledge a root peer.
const STATUS_OK: u8 = b'K';
/// Status byte the server sends to reject a non-root peer.
const STATUS_NO: u8 = b'N';
/// Reject message body written after [`STATUS_NO`].
const REJECT_MSG: &[u8] = b"you are not root\n";
/// Upper bound on how long we wait for an authenticated peer to deliver
/// its fd + handshake, so a stuck (or malicious) root peer can't wedge
/// PID 1 by connecting, passing peercred, then never sending anything.
const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Upper bound on the handshake TERM string, so a hostile/garbled peer
/// can't make us allocate unbounded ancillary/data buffers.
const MAX_TERM_LEN: usize = 256;

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
    const fn max_encoded_len() -> usize {
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

// ---------------------------------------------------------------------------
// Server side (async, tokio current-thread).
// ---------------------------------------------------------------------------

/// Create `/nmbl-run` (0700) if absent, unlink any stale socket, bind
/// [`TUI_SOCK_PATH`], and chmod the socket 0600. Phase 3 owns the
/// accept loop; this only hands back the bound listener.
pub fn bind_listener() -> io::Result<UnixListener> {
    let dir = Path::new(TUI_SOCK_DIR);
    match rustix::fs::mkdir(dir, Mode::from_raw_mode(0o700)) {
        // We just created it 0700/uid0; trust it.
        Ok(()) => {}
        // Reuse path: don't trust an attacker-influenced dir. Require it
        // to be a directory, owned by uid 0, mode exactly 0700, or fail
        // CLOSED rather than bind a socket inside it.
        Err(rustix::io::Errno::EXIST) => verify_run_dir(TUI_SOCK_DIR)?,
        Err(e) => return Err(io::Error::from(e)),
    }
    let path = Path::new(TUI_SOCK_PATH);
    // Unlink any stale socket from a previous run; ENOENT is fine.
    match rustix::fs::unlink(path) {
        Ok(()) => {}
        Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(io::Error::from(e)),
    }
    // Tighten the umask around the bind so the socket is born without any
    // group/other access — closing the window between `bind` and `chmod`
    // during which the socket would otherwise be world-connectable.
    let prev = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let bound = UnixListener::bind(path);
    nix::sys::stat::umask(prev); // restore regardless of bind outcome
    let listener = bound?;
    // Belt-and-suspenders: pin the mode explicitly too.
    rustix::fs::chmod(path, Mode::from_raw_mode(0o600))?;
    Ok(listener)
}

/// Fail-closed verification of a pre-existing [`TUI_SOCK_DIR`]: it must be
/// a directory, owned by uid 0, with mode exactly 0700. Any deviation is
/// an `io::Error` so we never bind inside an attacker-influenced dir.
fn verify_run_dir(path: &str) -> io::Result<()> {
    let st = nix::sys::stat::stat(path)?;
    let perm = st.st_mode & 0o7777;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path} exists but is not a directory"),
        ));
    }
    if st.st_uid != 0 || perm != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{path}: want root-owned 0700, got uid {} mode {perm:#o}",
                st.st_uid
            ),
        ));
    }
    Ok(())
}

/// Authenticate one accepted connection and, on success, receive the
/// client's pty fd + handshake.
///
/// Returns `Ok(None)` for a rejected (non-root) peer after writing the
/// rejection bytes; `Ok(Some(handle))` for an authenticated root peer
/// after writing the `b"K"` ack.
pub async fn authenticate_and_receive(stream: &mut UnixStream) -> io::Result<Option<RemoteHandle>> {
    // The peercred check is immediate, so it stays untimed.
    let cred = rustix::net::sockopt::get_socket_peercred(stream.as_fd())?;
    if !cred.uid.is_root() {
        write_rejection(stream).await?;
        return Ok(None);
    }
    // Bound the handshake wait so a root peer that connects but never
    // sends the fd can't park PID 1 forever. (Phase 3's accept loop
    // should also `spawn` each connection so one slow peer can't starve
    // `accept` while we wait out this timeout.)
    let handle = match recv_with_timeout(stream).await {
        Ok(handle) => handle,
        // Timed out waiting for the handshake: drop the connection.
        Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(None),
        Err(e) => return Err(e),
    };
    stream.writable().await?;
    write_all_async(stream, &[STATUS_OK]).await?;
    Ok(Some(handle))
}

/// Wait (up to [`RECV_TIMEOUT`]) for one readable event and recv the fd +
/// handshake. tokio's readiness is edge-ish, so loop on `WouldBlock`.
/// Returns an `io::ErrorKind::TimedOut` error if the deadline elapses.
async fn recv_with_timeout(stream: &mut UnixStream) -> io::Result<RemoteHandle> {
    let recv = async {
        loop {
            stream.readable().await?;
            match stream.try_io(tokio::io::Interest::READABLE, || {
                recv_fd_and_handshake(stream.as_fd())
            }) {
                Ok(handle) => return Ok(handle),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    };
    match tokio::time::timeout(RECV_TIMEOUT, recv).await {
        Ok(res) => res,
        Err(_elapsed) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for the client handshake",
        )),
    }
}

/// Write the rejection status byte + reason to a non-root peer.
async fn write_rejection(stream: &mut UnixStream) -> io::Result<()> {
    stream.writable().await?;
    let mut buf = Vec::with_capacity(1 + REJECT_MSG.len());
    buf.push(STATUS_NO);
    buf.extend_from_slice(REJECT_MSG);
    write_all_async(stream, &buf).await
}

/// Async `write_all` over a tokio `UnixStream` without pulling
/// `AsyncWriteExt` into scope at every call site.
async fn write_all_async(stream: &mut UnixStream, data: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    stream.write_all(data).await
}

/// Synchronous `recvmsg` of exactly one pty fd + the handshake payload.
/// Runs inside `try_io`, so a return of `WouldBlock` is retried by the
/// caller.
fn recv_fd_and_handshake(fd: BorrowedFd<'_>) -> io::Result<RemoteHandle> {
    let mut data = [0u8; Handshake::max_encoded_len()];
    let mut iov = [IoSliceMut::new(&mut data)];
    let mut space = [0u8; rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = RecvAncillaryBuffer::new(&mut space);
    let ret = recvmsg(fd, &mut iov, &mut cmsg, RecvFlags::empty())?;
    // Reject truncated ancillary data: the fd set may be incomplete and
    // must not be trusted. rustix 0.38's `RecvFlags` has no named CTRUNC
    // bit, but the raw bit is preserved in the returned flags.
    if ret.flags.bits() & (libc::MSG_CTRUNC as u32) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated ancillary data (MSG_CTRUNC)",
        ));
    }
    let mut pty: Option<OwnedFd> = None;
    for msg in cmsg.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            for received in fds {
                if pty.is_none() {
                    pty = Some(received);
                }
                // Any extra fds drop here, closing them.
            }
        }
    }
    let pty =
        pty.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "client sent no pty fd"))?;
    let hs = Handshake::decode(data.get(..ret.bytes).unwrap_or(&[]))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed handshake"))?;
    Ok(RemoteHandle {
        pty,
        term: hs.term,
        winsize: hs.winsize,
    })
}

// ---------------------------------------------------------------------------
// Client side (synchronous std — the non-PID-1 invocation).
// ---------------------------------------------------------------------------

/// Connect to PID 1's TUI socket, pass our controlling terminal across,
/// and go quiescent while the server drives it. Returns the process
/// exit code.
pub fn connect_and_serve() -> ExitCode {
    match run_client() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[nmbl] remote-tui client: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Fallible body of [`connect_and_serve`].
fn run_client() -> io::Result<ExitCode> {
    let path = resolve_socket_path()?;
    let mut stream = StdUnixStream::connect(&path)?;
    let tty = open_controlling_tty()?;
    let handshake = build_handshake(tty.as_fd());
    send_fd_and_handshake(stream.as_fd(), tty.as_fd(), &handshake)?;
    drop(tty); // the server holds its own copy now.

    let mut status = [0u8; 1];
    stream.read_exact(&mut status)?;
    match status[0] {
        STATUS_OK => {
            // Server now drives the operator's terminal. Stay quiescent:
            // do NOT touch the tty. Block until the server hangs up.
            let mut sink = [0u8; 256];
            loop {
                match stream.read(&mut sink) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        STATUS_NO => {
            let mut reason = String::new();
            stream.read_to_string(&mut reason)?;
            eprint!("[nmbl] remote-tui rejected: {reason}");
            Ok(ExitCode::FAILURE)
        }
        other => {
            eprintln!("[nmbl] remote-tui: unexpected status byte {other:#x}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Resolve the socket path: `$NMBL_TUI_SOCK`, else the canonical path,
/// else the chrooted-rescue view.
fn resolve_socket_path() -> io::Result<PathBuf> {
    if let Some(env) = std::env::var_os(TUI_SOCK_ENV) {
        return Ok(PathBuf::from(env));
    }
    let canonical = Path::new(TUI_SOCK_PATH);
    if canonical.exists() {
        return Ok(canonical.to_path_buf());
    }
    let chroot = Path::new(TUI_SOCK_PATH_CHROOT);
    if chroot.exists() {
        return Ok(chroot.to_path_buf());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no TUI socket at {TUI_SOCK_PATH} or {TUI_SOCK_PATH_CHROOT}"),
    ))
}

/// Open the client's controlling terminal: prefer `/dev/tty`, else fall
/// back to stdin if it is a tty.
fn open_controlling_tty() -> io::Result<OwnedFd> {
    match rustix::fs::open("/dev/tty", rustix::fs::OFlags::RDWR, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(_) => {
            let stdin = std::io::stdin();
            let stdin = stdin.as_fd();
            if rustix::termios::isatty(stdin) {
                // Duplicate so the returned OwnedFd owns a distinct fd
                // and dropping it never closes the real stdin.
                Ok(rustix::io::dup(stdin)?)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no controlling terminal (/dev/tty failed, stdin not a tty)",
                ))
            }
        }
    }
}

/// Build the handshake from `$TERM` and the tty's current winsize.
fn build_handshake(tty: BorrowedFd<'_>) -> Handshake {
    let term = std::env::var("TERM").unwrap_or_default();
    let winsize = rustix::termios::tcgetwinsize(tty)
        .map(|ws| (ws.ws_row, ws.ws_col))
        .unwrap_or((0, 0));
    Handshake { term, winsize }
}

/// `sendmsg` the handshake payload plus the tty fd via `SCM_RIGHTS`.
fn send_fd_and_handshake(
    sock: BorrowedFd<'_>,
    tty: BorrowedFd<'_>,
    handshake: &Handshake,
) -> io::Result<()> {
    let data = handshake.encode();
    let iov = [IoSlice::new(&data)];
    let mut space = [0u8; rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = SendAncillaryBuffer::new(&mut space);
    let fds = [tty];
    if !cmsg.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("failed to stage SCM_RIGHTS fd"));
    }
    let sent = sendmsg(sock, &iov, &mut cmsg, SendFlags::empty())?;
    if sent != data.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short handshake write",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "tui_socket_tests.rs"]
mod tests;
