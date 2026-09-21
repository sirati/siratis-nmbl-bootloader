#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use nmbl_host_tools::{cli, domain, sign};
use sha2::{Digest, Sha512};

struct Fixture {
    _temp: tempfile::TempDir,
    key: PathBuf,
    public: PathBuf,
    spool: PathBuf,
    bundle: PathBuf,
    boot: PathBuf,
    boot_mirror: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = temp.path().join("key");
        let public = temp.path().join("pub");
        let args = vec![
            "keygen".into(),
            "--alg".into(),
            "ml-dsa-65".into(),
            "--out-priv".into(),
            key.display().to_string(),
            "--out-pub".into(),
            public.display().to_string(),
        ];
        nmbl_host_tools::run(cli::parse(&args).expect("args")).expect("keygen");
        let spool = temp.path().join("spool");
        let bundle = spool.join("bundle");
        let boot = temp.path().join("boot");
        let boot_mirror = temp.path().join("boot-mirror");
        fs::create_dir_all(&bundle).expect("bundle");
        fs::create_dir(&boot).expect("boot");
        fs::create_dir(&boot_mirror).expect("boot mirror");
        make_bundle(&bundle, &key);
        let socket = temp.path().join("update.sock");
        Self {
            _temp: temp,
            key,
            public,
            spool,
            bundle,
            boot,
            boot_mirror,
            socket,
        }
    }

    fn server(&self) -> Child {
        let uid = rustix::process::getuid().as_raw().to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_nmbl-boot-update"))
            .args([
                "serve",
                self.socket.to_str().expect("socket"),
                self.spool.to_str().expect("spool"),
                self.public.to_str().expect("public"),
                &uid,
                self.boot.to_str().expect("boot"),
                self.boot_mirror.to_str().expect("boot mirror"),
            ])
            .spawn()
            .expect("start service");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.socket.exists() {
            if Instant::now() >= deadline {
                child.kill().expect("kill");
                child.wait().expect("reap timed-out service");
                assert!(self.socket.exists(), "socket timeout");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }
}

fn make_bundle(bundle: &Path, key: &Path) {
    let artifact_domain = domain::domain_for("boot-set-artifact").expect("domain");
    let entries = ["bootloader", "kernel", "initrd", "rescue", "config"]
        .into_iter().map(|name| {
            let bytes = format!("payload-{name}");
            let path = bundle.join(name);
            fs::write(&path, &bytes).expect("payload");
            sign::run(&path, key, artifact_domain, Some(&bundle.join(format!("{name}.sig"))))
                .expect("signature");
            let digest = format!("{:x}", Sha512::digest(bytes.as_bytes()));
            format!(r#"{{"role":"{name}","destination":"{name}","payload":"{name}","signature":"{name}.sig","domain":"boot-set-artifact","sha512":"{digest}"}}"#)
        }).collect::<Vec<_>>().join(",");
    let set_id = format!("{:x}", Sha512::digest(b"protocol"));
    fs::write(
        bundle.join("manifest.json"),
        format!(r#"{{"version":1,"set_id":"{set_id}","target_slot":"A","files":[{entries}]}}"#),
    )
    .expect("manifest");
    sign::run(
        &bundle.join("manifest.json"),
        key,
        domain::domain_for("boot-set-manifest").expect("domain"),
        Some(&bundle.join("manifest.json.sig")),
    )
    .expect("manifest signature");
}

#[test]
fn same_binary_peer_is_required_and_both_sides_validate() {
    let fixture = Fixture::new();
    let mut server = fixture.server();
    let mut foreign = UnixStream::connect(&fixture.socket).expect("foreign connect");
    writeln!(foreign, "{{\"bundle\":\"{}\"}}", fixture.bundle.display()).expect("request");
    let mut response = String::new();
    BufReader::new(foreign)
        .read_line(&mut response)
        .expect("response");
    assert!(response.contains("same update binary"));

    let output = Command::new(env!("CARGO_BIN_EXE_nmbl-boot-update"))
        .args([
            "request",
            fixture.socket.to_str().expect("socket"),
            fixture.bundle.to_str().expect("bundle"),
            fixture.public.to_str().expect("public"),
        ])
        .output()
        .expect("client");
    server.kill().expect("stop service");
    server.wait().expect("reap service");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.boot.join("nmbl-boot-sets/active")).expect("active"),
        "set nmbl_slot=A\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.boot_mirror.join("nmbl-boot-sets/active"))
            .expect("mirror active"),
        "set nmbl_slot=A\n"
    );
    assert!(!fixture.key.starts_with("/nix/store"));
}
