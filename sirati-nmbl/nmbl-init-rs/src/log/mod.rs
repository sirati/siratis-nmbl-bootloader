mod byte_ring;
mod kmsg;
mod kmsg_read;
mod ring;
mod tui_flag;
mod verbosity;

#[cfg(test)]
mod tests;

/// Tmpfs path the byte-ring is flushed to before every terminal action
/// and before kexec. The same path is recreated in the next kernel's
/// initramfs by the cpio fragment spliced into `kexec_file_load(2)`, so
/// the booted system's `nmbl-log-import` stage-1 helper can drain it.
/// Single source of truth for both the dispatcher (`main.rs`) and the
/// kexec staging path (`boot.rs`).
pub const NMBL_LOG_PATH: &str = "/nmbl-log/nmbl.log";

pub use byte_ring::{flush_to, snapshot_full};
pub use kmsg::emit_kmsg;
#[cfg(test)]
pub(crate) use kmsg_read::set_raw_reader_for_test;
pub use kmsg_read::snapshot_kernel;
pub use ring::{push_ring, snapshot};
pub use tui_flag::{clear_tui_active, set_tui_active, tui_active};
pub use verbosity::{Verbosity, current, init};

#[macro_export]
macro_rules! nmbl_warn {
    ($($arg:tt)*) => {{
        let __line = format!("{}", format_args!($($arg)*));
        if !$crate::log::tui_active() {
            eprintln!("[nmbl] {}", __line);
        }
        $crate::log::emit_kmsg(&__line);
    }};
}

#[macro_export]
macro_rules! nmbl_info {
    ($($arg:tt)*) => {{
        match $crate::log::current() {
            $crate::log::Verbosity::Info | $crate::log::Verbosity::Verbose => {
                let __line = format!("{}", format_args!($($arg)*));
                if !$crate::log::tui_active() {
                    eprintln!("[nmbl] {}", __line);
                }
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
            if !$crate::log::tui_active() {
                eprintln!("[nmbl] {}", __line);
            }
            $crate::log::emit_kmsg(&__line);
        }
    }};
}
