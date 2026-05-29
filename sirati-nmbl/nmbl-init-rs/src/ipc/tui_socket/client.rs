//! Client side (synchronous std — the non-PID-1 invocation).
//!
//! Connects to PID 1's TUI socket, passes the controlling terminal
//! across, and goes quiescent while the server drives it.

use std::io::{self, IoSlice, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustix::fs::Mode;
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};

use super::codec::{Handshake, STATUS_NO, STATUS_OK};

/// Directory on the root tmpfs holding the TUI socket. Created 0700.
pub const TUI_SOCK_DIR: &str = "/nmbl-run";
/// Canonical path of the TUI control socket. The file is chmod 0600.
pub const TUI_SOCK_PATH: &str = "/nmbl-run/tui.sock";
/// Chrooted-rescue view of the same socket (Phase 4). Tried last by the
/// client when the canonical path is absent.
pub const TUI_SOCK_PATH_CHROOT: &str = "/nmbl-root/nmbl-run/tui.sock";
/// Env override for the client's socket path (highest precedence).
pub const TUI_SOCK_ENV: &str = "NMBL_TUI_SOCK";

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
pub(super) fn send_fd_and_handshake(
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
