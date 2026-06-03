use super::*;

#[test]
fn cmdline_empty_is_just_nul() {
    let kernel = Path::new("/boot/vmlinuz-test");
    let (buf, len) = build_cmdline_cstring("", kernel, None).expect("empty cmdline must succeed");
    assert_eq!(len, 1, "len must be 1 (the NUL)");
    assert_eq!(buf.as_slice(), b"\0");
}

/// LOW-A initrd fd-pin: when an initrd SOURCE fd is supplied, the initrd
/// bytes the cpio splice reads come from THAT fd, and the initrd PATH is
/// NEVER re-read. Proof: a BOGUS, nonexistent initrd path plus a pinned
/// source fd returns the fd's bytes (not ENOENT). The SAME bogus path
/// WITHOUT a fd fails with ENOENT — the path read happened. The contrast is
/// the assertion: the verified initrd fd is the splice source, so there is
/// no path re-read between verify and load.
#[test]
fn supplied_initrd_fd_is_the_splice_source_not_the_path() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsFd;

    let kernel = Path::new("/boot/vmlinuz-test");
    let bogus_initrd = Path::new("/nonexistent/nmbl/kexec/initrd/does/not/exist");

    // A real, open initrd fd standing in for the verifier's pinned one.
    // Seek it to EOF first to prove read_initrd_bytes seeks back to 0
    // (mirroring a post-verify fd left at EOF by the hash).
    let mut ifile = tempfile::NamedTempFile::new().expect("initrd tempfile");
    let body = b"pristine-verified-initrd-bytes";
    ifile.write_all(body).expect("write initrd");
    ifile.as_file().sync_all().expect("sync");
    ifile.seek(SeekFrom::End(0)).expect("seek to EOF");

    // With the pinned fd: the bytes come from the fd, NOT the bogus path.
    let bytes = read_initrd_bytes(kernel, bogus_initrd, Some(ifile.as_file().as_fd()))
        .expect("pinned-fd read must succeed even with a bogus path");
    assert_eq!(
        bytes.as_slice(),
        body,
        "the splice source must be the pinned fd's bytes, read from offset 0",
    );

    // Control: NO fd ⇒ the bogus path IS read ⇒ a KexecLoad error (ENOENT).
    let err = read_initrd_bytes(kernel, bogus_initrd, None)
        .expect_err("without a fd the bogus path must be read and fail");
    match err {
        NmblError::KexecLoad { source, .. } => {
            assert_eq!(
                source,
                nix::Error::ENOENT,
                "without a pinned fd the initrd path is read (ENOENT on a bogus path)",
            );
        }
        other => panic!("expected KexecLoad ENOENT, got {other:?}"),
    }
}

/// FIX-02 / MED-1 fd-pin: when a kernel fd IS supplied, the kernel PATH is
/// NEVER re-opened. Proof: a bogus, nonexistent kernel path with a pinned
/// kernel fd + a real initrd fd must NOT fail with ENOENT (it gets past the
/// open and fails only at the syscall with some OTHER errno). The same bogus
/// path WITHOUT a fd fails with ENOENT — the open happened. The contrast is
/// the assertion: the verified fd is consumed in place of a path re-open, so
/// there is no second path-open between verify and load.
#[test]
fn supplied_kernel_fd_is_not_reopened_by_path() {
    use std::io::Write;
    use std::os::fd::AsFd;

    let bogus = Path::new("/nonexistent/nmbl/kexec/kernel/does/not/exist");

    // A real, open kernel fd standing in for the verified one, plus a real
    // initrd fd, so the function reaches the syscall (which then fails — we
    // are not in a position to actually load — but with a NON-ENOENT errno).
    let mut kfile = tempfile::NamedTempFile::new().expect("kernel tempfile");
    kfile
        .write_all(b"\x7fELF fake kernel")
        .expect("write kernel");
    let mut ifile = tempfile::NamedTempFile::new().expect("initrd tempfile");
    ifile.write_all(b"fake initrd").expect("write initrd");
    let initrd_fd: OwnedFd = ifile.reopen().expect("reopen initrd").into();

    let with_fd = load_with_initrd_fd(
        bogus,
        Some(kfile.as_file().as_fd()),
        Some(bogus),
        Some(&initrd_fd),
        "init=/x",
        0,
    );
    // The syscall fails (we cannot really kexec under test), but the failure
    // must NOT be the ENOENT of opening `bogus` — the fd was used instead.
    match with_fd {
        Err(NmblError::KexecLoad { source, .. }) => {
            assert_ne!(
                source,
                nix::Error::ENOENT,
                "with a pinned fd the kernel path must NOT be opened (no ENOENT)",
            );
        }
        other => panic!("expected KexecLoad (syscall) error, got {other:?}"),
    }

    // Control: NO fd ⇒ the bogus path IS opened ⇒ ENOENT.
    let without_fd = load_with_initrd_fd(bogus, None, Some(bogus), Some(&initrd_fd), "init=/x", 0);
    match without_fd {
        Err(NmblError::KexecLoad { source, .. }) => {
            assert_eq!(
                source,
                nix::Error::ENOENT,
                "without a fd the kernel path must be opened (ENOENT on a bogus path)",
            );
        }
        other => panic!("expected KexecLoad ENOENT, got {other:?}"),
    }
}

#[test]
fn cmdline_typical_includes_nul() {
    let kernel = Path::new("/boot/vmlinuz-test");
    let s = "init=/sbin/init root=/dev/sda1";
    let (buf, len) = build_cmdline_cstring(s, kernel, None).expect("typical cmdline must succeed");
    assert_eq!(len, s.len() + 1, "len must be byte length + 1 for NUL");
    assert_eq!(buf.len(), len, "buffer length must equal reported len");
    let last = match buf.last() {
        Some(b) => *b,
        None => panic!("buffer must be non-empty"),
    };
    assert_eq!(last, 0, "buffer must be NUL-terminated");
}

#[test]
fn cmdline_embedded_nul_is_rejected() {
    let kernel = Path::new("/boot/vmlinuz-test");
    let res = build_cmdline_cstring("init=/sbin/init\0root=/dev/sda1", kernel, None);
    assert!(res.is_err(), "embedded NUL must produce an error");
    match res {
        Err(NmblError::KexecLoad {
            kernel: k, source, ..
        }) => {
            assert_eq!(k, kernel);
            assert_eq!(source, nix::Error::from(Errno::EINVAL));
        }
        _ => panic!("expected NmblError::KexecLoad for embedded-NUL cmdline"),
    }
}

#[test]
fn flag_constants_match_kernel_uapi() {
    // Spot-check the bit values against linux/kexec.h.
    assert_eq!(KEXEC_FILE_ON_CRASH, 0x1);
    assert_eq!(KEXEC_FILE_PRESERVE_CTX, 0x2);
    assert_eq!(KEXEC_FILE_NO_INITRAMFS, 0x4);
}
