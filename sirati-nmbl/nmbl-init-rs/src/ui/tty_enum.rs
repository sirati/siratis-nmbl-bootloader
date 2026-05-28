//! Enumerate operator-attached tty devices for the console picker.
//!
//! The kernel-elected console (from `/sys/class/tty/console/active`) is
//! only one of several plausible operator-visible ttys. On a typical
//! splash-on-VNC system with `console=ttyS0` on the cmdline the kernel
//! reports `ttyS0` as the active console, but the framebuffer-rendered
//! splash that the operator actually looks at lives on `/dev/tty1`. The
//! picker has to enumerate ALL plausible operator-attached ttys, not
//! just the kernel-elected one, otherwise the operator can never select
//! the splash tty as a shell target.
//!
//! Enumeration sources (in candidate-list order):
//! 1. The kernel-elected primary console (already resolved by the
//!    picker; labelled `(kernel console)`).
//! 2. `/dev/tty1` — almost always the framebuffer VT on splash-capable
//!    systems. Labelled `(framebuffer tty)`.
//! 3. `/dev/ttyS0..=/dev/ttyS3` when the devnode exists — the classic
//!    serial-port range. Labelled `(serial port)`.
//! 4. `/dev/ttyUSB0..=/dev/ttyUSB7` and `/dev/ttyACM0..=/dev/ttyACM3`
//!    when the devnodes exist — USB-attached serial. Labelled
//!    `(USB serial)`.
//!
//! Deduplication: every candidate added here is rejected if its path
//! matches the kernel-elected console — the kernel entry already
//! carries the operator-visible label and we do not want to render
//! the same fd under two different annotations.

use std::path::{Path, PathBuf};

use rustix::fs::{FileType, stat};

/// Auto-discovered tty candidate. Distinct from
/// [`crate::ui::console_picker::PickerCandidate`] so the enumeration
/// logic stays decoupled from the picker's state-machine representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumeratedTty {
    /// Absolute `/dev/<name>` path.
    pub path: PathBuf,
    /// Operator-facing kind annotation. Short enough to fit on the
    /// candidate-row label without wrapping.
    pub kind: TtyKind,
}

/// Why this tty showed up in the enumeration. Renderer maps each to a
/// short human label (e.g. `(serial port)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyKind {
    /// `/dev/tty1` (or similar) — the framebuffer VT.
    FramebufferTty,
    /// `/dev/ttyS<N>` — classic serial line.
    SerialPort,
    /// `/dev/ttyUSB<N>` or `/dev/ttyACM<N>` — USB-attached serial.
    UsbSerial,
}

impl TtyKind {
    /// Operator-facing short label rendered next to the path in the
    /// picker candidate list.
    pub fn short_label(self) -> &'static str {
        match self {
            TtyKind::FramebufferTty => "(framebuffer tty)",
            TtyKind::SerialPort => "(serial port)",
            TtyKind::UsbSerial => "(USB serial)",
        }
    }
}

/// Enumerate auto-discovered ttys against the live `/dev`. Returns
/// every candidate that exists as a character device, EXCLUDING any
/// entry whose path equals `exclude`. Use the kernel-elected console
/// path as `exclude` so the picker does not list the same fd twice
/// under different labels.
pub fn enumerate_ttys(exclude: &Path) -> Vec<EnumeratedTty> {
    enumerate_ttys_in(Path::new("/dev"), exclude)
}

/// Path-parameterised core for unit tests. Production callers go
/// through [`enumerate_ttys`].
pub fn enumerate_ttys_in(dev_root: &Path, exclude: &Path) -> Vec<EnumeratedTty> {
    let mut out: Vec<EnumeratedTty> = Vec::new();

    // 1. Framebuffer VT.
    consider(dev_root, "tty1", TtyKind::FramebufferTty, exclude, &mut out);

    // 2. Classic serial. ttyS0..ttyS3 covers the historical 16550 set;
    //    going higher is rare on real hardware and operators can use
    //    the custom-path input for anything more exotic.
    for n in 0..=3 {
        let name = format!("ttyS{n}");
        consider(dev_root, &name, TtyKind::SerialPort, exclude, &mut out);
    }

    // 3. USB-attached serial. The kernel allocates ttyUSB0..N for
    //    ftdi/cp210x/pl2303 style adapters and ttyACM0..N for CDC-ACM
    //    (Arduino, modems). Cap each range at a sensible upper bound;
    //    operators with more devices can use the custom input.
    for n in 0..=7 {
        let name = format!("ttyUSB{n}");
        consider(dev_root, &name, TtyKind::UsbSerial, exclude, &mut out);
    }
    for n in 0..=3 {
        let name = format!("ttyACM{n}");
        consider(dev_root, &name, TtyKind::UsbSerial, exclude, &mut out);
    }

    out
}

/// Append `name` to the output list when it exists as a character
/// device under `dev_root` and its absolute path is not the kernel
/// console (`exclude`). All filtering happens here so the caller stays
/// concise.
fn consider(
    dev_root: &Path,
    name: &str,
    kind: TtyKind,
    exclude: &Path,
    out: &mut Vec<EnumeratedTty>,
) {
    let full = dev_root.join(name);
    if full == exclude {
        return;
    }
    if !is_char_device(&full) {
        return;
    }
    out.push(EnumeratedTty { path: full, kind });
}

/// True iff `path` exists and resolves to a character device. Returns
/// false for missing paths, regular files, directories, sockets, etc.
/// Used both by the auto-enumeration (filter out absent devnodes) and
/// by the custom-input live validator (only accept chardevs).
pub fn is_char_device(path: &Path) -> bool {
    match stat(path) {
        Ok(st) => FileType::from_raw_mode(st.st_mode) == FileType::CharacterDevice,
        Err(_) => false,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    use std::os::unix::fs::OpenOptionsExt;

    /// Build a fake /dev tree under a temp directory and assert the
    /// enumeration picks the right set, in the right order, with the
    /// kernel-console exclusion respected.
    #[test]
    fn enumerate_picks_existing_chardevs_and_skips_exclude() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return,
        };
        // We can't `mknod` without CAP_MKNOD, so we approximate the
        // chardev test by symlinking to /dev/null (which IS a chardev).
        // `stat(2)` follows the symlink so the type check passes.
        let null = Path::new("/dev/null");
        if !is_char_device(null) {
            // CI container without /dev/null; skip.
            return;
        }
        let want = ["tty1", "ttyS0", "ttyS2", "ttyUSB0"];
        for name in &want {
            let p = dir.path().join(name);
            if std::os::unix::fs::symlink(null, &p).is_err() {
                return;
            }
        }
        // ttyACM2 deliberately absent so the enumeration must skip it.
        // A regular file at ttyS3 to make sure non-chardevs are rejected.
        let bogus = dir.path().join("ttyS3");
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&bogus);

        // Exclude must be expressed relative to the same dev_root the
        // enumerator joins against; that's the path the enumerator
        // actually constructs and compares.
        let exclude = dir.path().join("ttyS0");
        let got = enumerate_ttys_in(dir.path(), &exclude);
        let paths: Vec<&Path> = got.iter().map(|c| c.path.as_path()).collect();
        // ttyS0 excluded as kernel console; ttyS3 rejected because it's
        // not a chardev; tty1 framebuffer first, then ttyS2, then
        // ttyUSB0.
        assert_eq!(
            paths,
            vec![
                dir.path().join("tty1").as_path(),
                dir.path().join("ttyS2").as_path(),
                dir.path().join("ttyUSB0").as_path(),
            ]
        );
        assert_eq!(got[0].kind, TtyKind::FramebufferTty);
        assert_eq!(got[1].kind, TtyKind::SerialPort);
        assert_eq!(got[2].kind, TtyKind::UsbSerial);
    }

    #[test]
    fn is_char_device_accepts_dev_null() {
        // /dev/null is a chardev on every Linux distro NMBL targets. If
        // CI lacks it the test silently passes — production callers
        // tolerate the same condition.
        if std::fs::metadata("/dev/null").is_ok() {
            assert!(is_char_device(Path::new("/dev/null")));
        }
    }

    #[test]
    fn is_char_device_rejects_missing_path() {
        let p = Path::new("/tmp/this/does/not/exist/nmbl-tty-enum-test");
        assert!(!is_char_device(p));
    }

    #[test]
    fn is_char_device_rejects_regular_file() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return,
        };
        let p = dir.path().join("regular");
        if std::fs::write(&p, b"hi").is_err() {
            return;
        }
        assert!(!is_char_device(&p));
    }
}
