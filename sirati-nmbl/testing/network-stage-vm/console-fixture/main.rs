//! Test-only inherited VT state stimulus, followed by the production launcher.
use std::{fs::OpenOptions, os::{fd::AsRawFd, unix::{fs::OpenOptionsExt, process::CommandExt}}, process::{Command, Stdio}};
unsafe extern "C" { fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int; }
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(args.len() == 3 && args[1..].iter().all(|p| p.starts_with('/')));
    let tty = OpenOptions::new().read(true).write(true).custom_flags(0x20100).open("/dev/tty1").unwrap();
    for (request, argument) in [(0x5606, 2), (0x5607, 2), (0x4b45, 0)] {
        // SAFETY: pinned VT descriptor and Linux integer-valued ioctl requests.
        assert_eq!(unsafe { ioctl(tty.as_raw_fd(), request, argument) }, 0);
    }
    assert!(Command::new(env!("FIXTURE_STTY")).args(["-icanon", "-echo", "-icrnl"])
        .stdin(Stdio::from(tty)).status().unwrap().success());
    std::fs::write("/run/nmbl-console-inherited-state-fixture", "VT2 K_RAW noncanonical-noecho\n").expect("record inherited console stimulus");
    let error = Command::new(env!("PRODUCTION_CONSOLE")).args(&args[1..]).exec();
    panic!("production console exec failed: {error}");
}
