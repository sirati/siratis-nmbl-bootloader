//! In-container stand-ins for the external tools NMBL `execve`s: `blkid` and
//! `cryptsetup`. The same `nmbl-simbox` binary is copied into the container as
//! `/bin/blkid` and `/bin/cryptsetup` and dispatches on `argv[0]`. They read
//! the scenario data the supervisor staged at `/.simbox/devices.json`.
//!
//! They are real processes (the kernel executes them), so NMBL's fork/exec,
//! pipe, stdin-passphrase and exit-code handling all run for real; only the
//! device knowledge is simulated.

use std::collections::BTreeMap;
use std::io::Read;

use serde::{Deserialize, Serialize};

pub const DEVICES_JSON: &str = "/.simbox/devices.json";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Devices {
    /// Block device basename -> blkid attributes.
    pub blkid: BTreeMap<String, BTreeMap<String, String>>,
    /// Device basename -> (mapper name, passphrase).
    pub luks: BTreeMap<String, (String, String)>,
}

fn load() -> Devices {
    std::fs::read_to_string(DEVICES_JSON)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Resolve `/dev/disk/by-*/x` symlinks to the node basename.
fn node_name(path: &str) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `blkid -p -o export DEV`: exit 2 (no superblock) for unknown devices.
pub fn blkid(args: &[String]) -> i32 {
    let Some(dev) = args.last() else { return 4 };
    let devices = load();
    let Some(attrs) = devices.blkid.get(&node_name(dev)) else {
        return 2;
    };
    println!("DEVNAME={dev}");
    for (k, v) in attrs {
        println!("{k}={v}");
    }
    0
}

/// `cryptsetup open DEV NAME --key-file=-` (password) or `--token-only`.
/// Checks the passphrase from stdin; on success creates `/dev/mapper/NAME`
/// (as a regular-file stand-in) and exits 0. Wrong passphrase: exit 2, which
/// is what real cryptsetup returns and what NMBL's retry modal keys on.
pub fn cryptsetup(args: &[String]) -> i32 {
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (Some(cmd), Some(dev), Some(name)) =
        (positional.first(), positional.get(1), positional.get(2))
    else {
        eprintln!("cryptsetup(simbox): unsupported invocation {args:?}");
        return 1;
    };
    if cmd.as_str() != "open" {
        eprintln!("cryptsetup(simbox): only `open` is simulated");
        return 1;
    }
    let devices = load();
    let Some((mapper, pass)) = devices.luks.get(&node_name(dev)) else {
        eprintln!("cryptsetup(simbox): {dev} is not a LUKS device");
        return 1;
    };
    if args.iter().any(|a| a == "--token-only") {
        eprintln!("cryptsetup(simbox): no TPM token in this scenario");
        return 1;
    }
    let mut given = String::new();
    let _ = std::io::stdin().read_to_string(&mut given);
    if given.trim_end_matches('\n') != pass {
        eprintln!("No key available with this passphrase.");
        return 2;
    }
    let _ = std::fs::create_dir_all("/dev/mapper");
    if std::fs::write(format!("/dev/mapper/{name}"), b"").is_err() {
        return 1;
    }
    let _ = mapper;
    0
}
