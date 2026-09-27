//! `nmbl-simbox` — run the unmodified production `nmbl-init` as PID 1 in a
//! rootless podman container with all capabilities dropped, while a
//! supervisor outside the container answers every kernel-facing syscall from
//! a scenario description. See `docs/nmbl-simbox.md` for the architecture.
//!
//! ```text
//! nmbl-simbox run SCENARIO.toml [--headless] [--reveal-keys] [--trace]
//!             [--json OUT] [--timeout SECS]
//! ```
//!
//! The binary also serves as the in-container `blkid` and `cryptsetup`
//! stand-ins (dispatch on `argv[0]`).

mod console;
mod drm;
mod report;
mod scenario;
mod seccomp;
mod sim;
mod task;
mod tools;
mod x11view;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let prog = argv
        .first()
        .and_then(|a| Path::new(a).file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
    match prog.as_str() {
        "blkid" => return exit(tools::blkid(&rest)),
        "cryptsetup" => return exit(tools::cryptsetup(&rest)),
        _ => {}
    }
    match parse(&rest).and_then(|opts| console::run(&opts)) {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("nmbl-simbox: {e}");
            ExitCode::from(2)
        }
    }
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

/// Command-line options for `run`.
pub struct Opts {
    pub scenario: PathBuf,
    pub headless: bool,
    pub reveal_keys: bool,
    pub trace: bool,
    pub json: Option<PathBuf>,
    pub timeout_secs: u64,
    /// The nmbl-init binary to run as PID 1 (default: the one inside the
    /// scenario's initramfs at /init).
    pub init: Option<PathBuf>,
    /// Simulate a DRM card and show it in an X11 window (splash builds).
    pub graphical: bool,
    /// Write the last simulated framebuffer to this PPM file at the end.
    pub frame_dump: Option<PathBuf>,
}

const USAGE: &str = "usage: nmbl-simbox run SCENARIO.toml [--headless] [--reveal-keys] \
                     [--trace] [--json OUT] [--timeout SECS] [--init NMBL-INIT] [--graphical] \
                     [--frame-dump FILE.ppm]";

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("run") => {}
        _ => return Err(USAGE.to_string()),
    }
    let mut o = Opts {
        scenario: PathBuf::new(),
        headless: false,
        reveal_keys: false,
        trace: false,
        json: None,
        timeout_secs: 120,
        init: None,
        graphical: false,
        frame_dump: None,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => o.headless = true,
            "--graphical" => o.graphical = true,
            "--frame-dump" => o.frame_dump = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--reveal-keys" => o.reveal_keys = true,
            "--trace" => o.trace = true,
            "--json" => o.json = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--init" => o.init = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--timeout" => {
                o.timeout_secs = it.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?;
            }
            s if !s.starts_with("--") && o.scenario.as_os_str().is_empty() => {
                o.scenario = PathBuf::from(s);
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    if o.scenario.as_os_str().is_empty() {
        return Err(USAGE.to_string());
    }
    Ok(o)
}
