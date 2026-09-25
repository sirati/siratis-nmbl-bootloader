//! End-to-end tests of the pipe-only key flow through the real `nmbl-sign`
//! binary: `keygen --stdio` (private on stdout, public on fd 3), then
//! `sign --key-stdin`, then `verify` with the fd-3 public key. Also pins the
//! refusals: fd 3 closed, and stdin used for both key and input.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "integration tests assert and may panic on failure"
)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_nmbl-sign");

/// Run `nmbl-sign keygen --stdio` under `sh` so fd 3 can be redirected to
/// `public`; returns the private-key bytes captured from stdout.
fn keygen_stdio(alg: &str, public: &Path) -> Vec<u8> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(r#"exec "$0" keygen --alg "$1" --stdio 3>"$2""#)
        .arg(BIN)
        .arg(alg)
        .arg(public)
        .stdin(Stdio::null())
        .output()
        .expect("spawn keygen");
    assert!(out.status.success(), "keygen failed: {}", stderr(&out));
    out.stdout
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run `nmbl-sign <args>` with `stdin_bytes` piped on stdin.
fn run_with_stdin(args: &[&str], stdin_bytes: &[u8]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nmbl-sign");
    // A refused invocation may exit before reading; ignore EPIPE then.
    let _ = child.stdin.take().unwrap().write_all(stdin_bytes);
    child.wait_with_output().unwrap()
}

#[test]
fn stdio_keygen_round_trips_through_key_stdin() {
    for (alg, pk_len) in [("ml-dsa-65", 1952), ("ml-dsa-87", 2592)] {
        let dir = tempfile::tempdir().unwrap();
        let public = dir.path().join("public");
        let private = keygen_stdio(alg, &public);
        assert!(private.starts_with(b"NMBLSK01"), "{alg}: container magic");
        assert_eq!(
            fs::read(&public).unwrap().len(),
            pk_len,
            "{alg}: raw public key"
        );
        // keygen --stdio creates no files besides what the caller redirected.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        let input = dir.path().join("config.toml");
        fs::write(&input, b"[bootstrap]\n").unwrap();
        let sig = dir.path().join("config.toml.sig");
        let out = run_with_stdin(
            &[
                "sign",
                "--key-stdin",
                "--domain",
                "boot-config",
                input.to_str().unwrap(),
                "--out",
                sig.to_str().unwrap(),
            ],
            &private,
        );
        assert!(out.status.success(), "{alg}: sign failed: {}", stderr(&out));

        let verify = Command::new(BIN)
            .args([
                "verify",
                "--key",
                public.to_str().unwrap(),
                "--domain",
                "boot-config",
                "--sig",
                sig.to_str().unwrap(),
                input.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            verify.status.success(),
            "{alg}: verify failed: {}",
            stderr(&verify)
        );

        // The same signature must not verify under another role.
        let wrong = Command::new(BIN)
            .args([
                "verify",
                "--key",
                public.to_str().unwrap(),
                "--domain",
                "gen-kernel",
                "--sig",
                sig.to_str().unwrap(),
                input.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(!wrong.status.success(), "{alg}: wrong domain must fail");
    }
}

#[test]
fn stdio_keygen_refuses_when_fd3_is_closed() {
    let out = Command::new("sh")
        .arg("-c")
        .arg(r#"exec "$0" keygen --alg ml-dsa-65 --stdio 3>&-"#)
        .arg(BIN)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "no private key may be emitted without fd 3"
    );
    assert!(stderr(&out).contains("fd 3"), "{}", stderr(&out));
}

#[test]
fn key_stdin_refuses_stdin_as_input() {
    let dir = tempfile::tempdir().unwrap();
    let private = keygen_stdio("ml-dsa-65", &dir.path().join("public"));

    // Spelled as stdin: refused at parse time.
    for input in ["-", "/dev/stdin"] {
        let out = run_with_stdin(
            &["sign", "--key-stdin", "--domain", "gen-kernel", input],
            &private,
        );
        assert!(!out.status.success(), "{input} must be refused");
        assert!(stderr(&out).contains("not stdin"), "{}", stderr(&out));
    }

    // An alias of the stdin file (same device/inode): refused at run time.
    let key_file = dir.path().join("key-as-stdin");
    fs::write(&key_file, &private).unwrap();
    let out = Command::new(BIN)
        .args([
            "sign",
            "--key-stdin",
            "--domain",
            "gen-kernel",
            key_file.to_str().unwrap(),
            "--out",
            dir.path().join("x.sig").to_str().unwrap(),
        ])
        .stdin(fs::File::open(&key_file).unwrap())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "input aliasing stdin must be refused"
    );
    assert!(stderr(&out).contains("not stdin"), "{}", stderr(&out));
}

#[test]
fn key_stdin_rejects_garbage_and_oversize() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("blob");
    fs::write(&input, b"x").unwrap();
    let input = input.to_str().unwrap();
    let out = run_with_stdin(
        &["sign", "--key-stdin", "--domain", "gen-kernel", input],
        b"nope",
    );
    assert!(!out.status.success());
    let big = vec![0u8; 64 * 1024];
    let out = run_with_stdin(
        &["sign", "--key-stdin", "--domain", "gen-kernel", input],
        &big,
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("exceeds"), "{}", stderr(&out));
    assert!(
        !dir.path().join("blob.sig").exists(),
        "no sidecar on a refused key"
    );
}
