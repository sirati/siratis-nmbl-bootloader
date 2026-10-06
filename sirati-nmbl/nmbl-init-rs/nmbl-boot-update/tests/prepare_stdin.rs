//! `nmbl-boot-update prepare … - PUBLIC_KEY` reads the private key once from
//! stdin, signs the whole slot bundle, and the result checks out.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use nmbl_host_tools::{cli, domain, keyfile, keygen, verify};

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
    // `network` and `tools` (the rescue tools image) are optional.
    for name in [
        "bootloader",
        "kernel",
        "initrd",
        "rescue",
        "config",
        "network",
        "tools",
    ] {
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
    assert!(output.join("tools").is_file() && output.join("tools.sig").is_file());
    let manifest = fs::read_to_string(output.join("manifest.json")).unwrap();
    assert!(
        manifest.contains(r#""role":"tools","destination":"tools""#),
        "{manifest}"
    );
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

    // Each signature is installed as the slot sidecar its verifier reads, so
    // it carries that verifier's domain: NMBL checks config.sig under
    // boot-config and the rescue, network and tools images under their own.
    for (name, expected, foreign) in [
        ("config", "boot-config", "boot-set-artifact"),
        ("rescue", "rescue-sfs", "boot-set-artifact"),
        ("network", "network-stage", "boot-set-artifact"),
        ("tools", "rescue-tools", "boot-set-artifact"),
        ("kernel", "boot-set-artifact", "boot-config"),
    ] {
        let signature = fs::read(output.join(format!("{name}.sig"))).unwrap();
        let verify_under = |domain_name: &str| {
            let mut payload = fs::File::open(output.join(name)).unwrap();
            verify::verify_reader(
                &mut payload,
                &public,
                domain::domain_for(domain_name).unwrap(),
                &signature,
            )
        };
        verify_under(expected).unwrap_or_else(|e| panic!("{name} under {expected}: {e}"));
        assert!(verify_under(foreign).is_err(), "{name} also verifies under {foreign}");
    }
}
