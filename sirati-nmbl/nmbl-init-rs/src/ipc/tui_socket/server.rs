//! Server side (async, tokio current-thread).
//!
//! Binds the root-only listener, authenticates each peer via
//! `SO_PEERCRED`, and receives the client's pty fd + handshake.

use std::io::{self, IoSliceMut};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::Mode;
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
use tokio::net::{UnixListener, UnixStream};

use super::client::{TUI_SOCK_DIR, TUI_SOCK_PATH};
use super::codec::{Handshake, REJECT_MSG, RemoteHandle, STATUS_NO, STATUS_OK};

/// Upper bound on how long we wait for an authenticated peer to deliver
/// its fd + handshake, so a stuck (or malicious) root peer can't wedge
/// PID 1 by connecting, passing peercred, then never sending anything.
const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Create `/nmbl-run` (0700) if absent, unlink any stale socket, bind
/// [`TUI_SOCK_PATH`], and chmod the socket 0600. Phase 3 owns the
/// accept loop; this only hands back the bound listener.
pub fn bind_listener() -> io::Result<UnixListener> {
    let dir = std::path::Path::new(TUI_SOCK_DIR);
    match rustix::fs::mkdir(dir, Mode::from_raw_mode(0o700)) {
        // We just created it 0700/uid0; trust it.
        Ok(()) => {}
        // Reuse path: don't trust an attacker-influenced dir. Require it
        // to be a directory, owned by uid 0, mode exactly 0700, or fail
        // CLOSED rather than bind a socket inside it.
        Err(rustix::io::Errno::EXIST) => verify_run_dir(TUI_SOCK_DIR)?,
        Err(e) => return Err(io::Error::from(e)),
    }
    let path = std::path::Path::new(TUI_SOCK_PATH);
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
pub(super) fn verify_run_dir(path: &str) -> io::Result<()> {
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
pub(super) fn recv_fd_and_handshake(fd: BorrowedFd<'_>) -> io::Result<RemoteHandle> {
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
