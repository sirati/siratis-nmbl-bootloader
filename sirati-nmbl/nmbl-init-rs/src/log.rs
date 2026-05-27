use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Deserialize;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    Quiet,
    #[default]
    Info,
    Verbose,
}

impl Verbosity {
    const fn as_u8(self) -> u8 {
        match self {
            Verbosity::Quiet => 0,
            Verbosity::Info => 1,
            Verbosity::Verbose => 2,
        }
    }

    const fn from_u8(v: u8) -> Verbosity {
        match v {
            0 => Verbosity::Quiet,
            2 => Verbosity::Verbose,
            // Any unexpected value collapses to Info — strictly safer than
            // silently dropping warnings, and we never write anything else.
            _ => Verbosity::Info,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Verbosity::Info.as_u8());

pub fn init(v: Verbosity) {
    CURRENT.store(v.as_u8(), Ordering::SeqCst);
}

pub fn current() -> Verbosity {
    Verbosity::from_u8(CURRENT.load(Ordering::SeqCst))
}

/// `/dev/kmsg` accepts writes from userspace and routes the resulting
/// printk message to every registered kernel console (regardless of
/// the `console=` ordering, which only picks the `/dev/console` target
/// for stdin/stdout/stderr). Teeing every NMBL log line here means
/// kernel messages, NMBL phase info, and the emergency shell all land
/// on the serial log AND on the framebuffer.
///
/// The fd is opened lazily on first write and cached for the lifetime
/// of the process. Failures (missing kmsg, permission denied) are
/// swallowed silently — the eprintln! path still produces output.
static KMSG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Try to write a line to /dev/kmsg. Must not be called with the KMSG
/// mutex held by the caller.
pub fn emit_kmsg(line: &str) {
    let Ok(mut guard) = KMSG.lock() else {
        return;
    };
    if guard.is_none() {
        let opened = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open("/dev/kmsg");
        if let Ok(f) = opened {
            *guard = Some(f);
        }
    }
    if let Some(file) = guard.as_mut() {
        // Use printk level 6 (KERN_INFO). The kernel parses "<6>" at
        // the start of each line and routes the rest as the message.
        let _ = writeln!(file, "<6>[nmbl] {line}");
    }
}

#[macro_export]
macro_rules! nmbl_warn {
    ($($arg:tt)*) => {{
        let __line = format!("{}", format_args!($($arg)*));
        eprintln!("[nmbl] {}", __line);
        $crate::log::emit_kmsg(&__line);
    }};
}

#[macro_export]
macro_rules! nmbl_info {
    ($($arg:tt)*) => {{
        match $crate::log::current() {
            $crate::log::Verbosity::Info | $crate::log::Verbosity::Verbose => {
                let __line = format!("{}", format_args!($($arg)*));
                eprintln!("[nmbl] {}", __line);
                $crate::log::emit_kmsg(&__line);
            }
            $crate::log::Verbosity::Quiet => {}
        }
    }};
}

#[macro_export]
macro_rules! nmbl_verbose {
    ($($arg:tt)*) => {{
        if $crate::log::current() == $crate::log::Verbosity::Verbose {
            let __line = format!("{}", format_args!($($arg)*));
            eprintln!("[nmbl] {}", __line);
            $crate::log::emit_kmsg(&__line);
        }
    }};
}
