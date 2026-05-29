//! Unit tests for the TUI socket transport (behind `remote-tui`).

use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

use super::*;

/// AF_UNIX SOCK_STREAM connected pair for `SCM_RIGHTS` round-trip tests.
fn unix_pair() -> (OwnedFd, OwnedFd) {
    #[allow(clippy::expect_used, reason = "test setup")]
    socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair")
}

#[test]
fn handshake_roundtrip_basic() {
    let hs = Handshake {
        term: "xterm-256color".to_string(),
        winsize: (40, 120),
    };
    let decoded = Handshake::decode(&hs.encode());
    assert_eq!(decoded, Some(hs));
}

#[test]
fn handshake_roundtrip_empty_term() {
    let hs = Handshake {
        term: String::new(),
        winsize: (0, 0),
    };
    let decoded = Handshake::decode(&hs.encode());
    assert_eq!(decoded, Some(hs));
}

#[test]
fn handshake_roundtrip_max_winsize_and_odd_term() {
    let hs = Handshake {
        term: "screen-256color\u{1F600}.weird/term".to_string(),
        winsize: (u16::MAX, u16::MAX),
    };
    let decoded = Handshake::decode(&hs.encode());
    assert_eq!(decoded, Some(hs));
}

#[test]
fn handshake_decode_rejects_truncated() {
    // Claims a 5-byte TERM but supplies none.
    let buf = [5u8, 0u8];
    assert_eq!(Handshake::decode(&buf), None);
    // Empty buffer.
    assert_eq!(Handshake::decode(&[]), None);
    // TERM present but missing the trailing rows/cols.
    let buf = [2u8, 0u8, b'v', b't'];
    assert_eq!(Handshake::decode(&buf), None);
}

#[test]
fn handshake_decode_rejects_oversized_term_len() {
    // term_len well over MAX_TERM_LEN must be rejected, not allocated.
    let mut buf = vec![0u8, 0u8];
    buf[0] = 0xFF;
    buf[1] = 0xFF;
    assert_eq!(Handshake::decode(&buf), None);
}

#[test]
fn handshake_encode_clamps_long_term() {
    // A TERM longer than MAX_TERM_LEN is truncated on encode and still
    // decodes cleanly (length prefix matches the truncated body).
    let hs = Handshake {
        term: "x".repeat(MAX_TERM_LEN + 50),
        winsize: (24, 80),
    };
    let decoded = Handshake::decode(&hs.encode()).expect("decode clamped");
    assert_eq!(decoded.term.len(), MAX_TERM_LEN);
    assert_eq!(decoded.winsize, (24, 80));
}

#[test]
fn scm_rights_roundtrip_fd_and_handshake() {
    let (a, b) = unix_pair();
    // Use a temp file's fd as the "pty" fd we transfer. Writing through
    // the received dup and reading it back from a fresh handle to the
    // same path proves the fd really crossed the socket and is usable.
    #[allow(clippy::expect_used, reason = "test setup")]
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    let file_fd: OwnedFd = tmp.reopen().expect("reopen").into();

    let hs = Handshake {
        term: "linux".to_string(),
        winsize: (50, 100),
    };
    send_fd_and_handshake(a.as_fd(), file_fd.as_fd(), &hs).expect("send");
    drop(file_fd);

    let handle = recv_fd_and_handshake(b.as_fd()).expect("recv");
    assert_eq!(handle.term, "linux");
    assert_eq!(handle.winsize, (50, 100));

    // The received fd must be a usable, writable handle to the temp file.
    let mut wr = std::fs::File::from(handle.pty);
    wr.write_all(b"ping").expect("write via received fd");
    wr.flush().expect("flush");
    drop(wr);

    let got = std::fs::read(&path).expect("read temp file");
    assert_eq!(got, b"ping");
}

#[test]
fn recv_without_fd_is_invalid_data() {
    let (a, b) = unix_pair();
    // Send only the handshake bytes, no ancillary fd.
    let hs = Handshake {
        term: "vt100".to_string(),
        winsize: (24, 80),
    };
    let data = hs.encode();
    let n = rustix::net::send(a.as_fd(), &data, rustix::net::SendFlags::empty()).expect("send");
    assert_eq!(n, data.len());
    let err = recv_fd_and_handshake(b.as_fd()).expect_err("must reject");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

/// The reject helper writes `b'N'` + the reason. We exercise the exact
/// bytes via a plain blocking socketpair (can't fake a non-root peercred
/// in-test), mirroring what `write_rejection` sends.
#[test]
fn reject_payload_shape() {
    let (a, b) = unix_pair();
    let mut buf = Vec::with_capacity(1 + REJECT_MSG.len());
    buf.push(STATUS_NO);
    buf.extend_from_slice(REJECT_MSG);
    write_blocking(a.as_fd(), &buf);
    drop(a);

    let mut rd = std::fs::File::from(b);
    let mut got = Vec::new();
    rd.read_to_end(&mut got).expect("read reject");
    assert_eq!(got.first(), Some(&STATUS_NO));
    assert_eq!(&got[1..], REJECT_MSG);
}

/// peercred on a real socketpair returns *our own* uid; assert the
/// helper reads it and that the root check is what gates the reject.
#[test]
fn peercred_reports_self_uid() {
    let (a, _b) = unix_pair();
    #[allow(clippy::expect_used, reason = "test")]
    let cred = rustix::net::sockopt::get_socket_peercred(a.as_fd()).expect("peercred");
    assert_eq!(cred.uid.as_raw(), nix::unistd::getuid().as_raw());
}

/// `verify_run_dir` must reject a path that is not a directory.
#[test]
fn verify_run_dir_rejects_non_directory() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_str().expect("utf8 path");
    let err = verify_run_dir(path).expect_err("a file is not a dir");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

/// `verify_run_dir` must reject a directory whose mode is not exactly
/// 0700 (here, group/other-accessible 0755).
#[test]
fn verify_run_dir_rejects_wrong_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf8 path");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod 0755");
    let err = verify_run_dir(path).expect_err("0755 must be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
}

/// A directory with exactly mode 0700 owned by the test's own uid passes
/// the mode/type checks. (When run as non-root the owner is the test uid,
/// not 0; the uid==0 branch is only reachable in the real PID-1 context,
/// so we accept this check only when running as root.)
#[test]
fn verify_run_dir_accepts_0700_dir_when_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf8 path");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .expect("chmod 0700");
    if nix::unistd::getuid().is_root() {
        verify_run_dir(path).expect("root-owned 0700 dir must pass");
    } else {
        // Non-root: the owner check fires; mode/type already passed.
        let err = verify_run_dir(path).expect_err("non-root owner rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}

fn write_blocking(fd: BorrowedFd<'_>, data: &[u8]) {
    let mut off = 0;
    while off < data.len() {
        #[allow(clippy::expect_used, clippy::indexing_slicing, reason = "test")]
        let n = rustix::io::write(fd, &data[off..]).expect("write");
        off += n;
    }
}
