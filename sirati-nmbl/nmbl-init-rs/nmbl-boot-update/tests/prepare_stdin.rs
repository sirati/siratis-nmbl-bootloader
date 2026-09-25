//! `nmbl-boot-update prepare … - PUBLIC_KEY` reads the private key once from
//! stdin, signs the whole slot bundle, and the result checks out.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use nmbl_host_tools::{cli, keyfile, keygen};

const BIN: &str = env!("CARGO_BIN_EXE_nmbl-boot-update");

#[test]
fn prepare_reads_private_key_from_stdin() {
    let temp = tempfile::tempdir().unwrap();
    let mut private = Vec::new();
    let mut public = Vec::new();
    let alg = match cli::parse(&[
        "keygen".into(),
        "--alg".into(),
        "ml-dsa-65".into(),
        "--stdio".into(),
    ])
    .unwrap()
    {
        cli::Command::KeygenStdio { alg } => alg,
        other => panic!("unexpected {other:?}"),
    };
    keygen::run_to(alg, &mut private, &mut public).unwrap();
    keyfile::parse_private(&private).unwrap();
    let public_path = temp.path().join("pub");
    fs::write(&public_path, &public).unwrap();

    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    for name in ["bootloader", "kernel", "initrd", "rescue", "config"] {
        fs::write(source.join(name), format!("{name} payload")).unwrap();
    }
    let output = temp.path().join("bundle");

    let mut child = Command::new(BIN)
        .args(["prepare", "A"])
        .arg(&source)
        .arg(&output)
        .arg("-")
        .arg(&public_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&private).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "prepare failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(output.join("manifest.json.sig").is_file());
    // Nothing in the bundle carries the private container.
    for entry in fs::read_dir(&output).unwrap() {
        let bytes = fs::read(entry.unwrap().path()).unwrap();
        assert!(!bytes.windows(8).any(|w| w == b"NMBLSK01"));
    }

    let check = Command::new(BIN)
        .arg("check")
        .arg(&output)
        .arg(&public_path)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}
