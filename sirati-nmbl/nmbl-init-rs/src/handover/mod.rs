//! Shared decoders for everything NMBL hands to the kexec'd kernel.
//!
//! NMBL's boot handoff carries several structured payloads: the kernel
//! cmdline, an initrd cpio fragment (LUKS keyfiles + the NMBL log transcript),
//! the log byte-ring itself, and the on-disk generation-state records. This
//! module is the SINGLE source of truth for parsing those formats back into
//! typed, human-readable views. Two consumers share it:
//!
//! * `nmblctl status` — the running system inspects what NMBL handed it;
//! * the syscall-simulating container harness — inspects the simulated kexec.
//!
//! Keeping the decoders here, next to the encoders they invert
//! ([`crate::sys::cpio`], [`crate::boot`], [`crate::log`],
//! [`crate::generation_state`]), lets round-trip tests pin encoder⇔decoder
//! agreement so the two consumers can never drift. Unknown or extra blobs
//! fall back to a hex/strings view rather than failing.
//!
//! Nothing here performs I/O or holds secrets beyond what the caller passes
//! in: every function is a pure transform over borrowed bytes/strings, so it
//! is trivially testable and safe to run on the live system. Secret values
//! (LUKS key material) are surfaced MASKED by default; the caller opts into a
//! reveal explicitly (`nmblctl`/harness `--reveal`).

pub mod cmdline;
pub mod cpio;
pub mod keyfiles;
pub mod logbuf;

pub use cmdline::{CmdlineParam, ParsedCmdline, parse_cmdline};
pub use cpio::{CpioEntry, CpioEntryKind, decode_cpio_fragment};
pub use keyfiles::{KeyHandover, KeyMethod, describe_key_injection, mask_key};
pub use logbuf::{LogLevel, LogLine, decode_log_buffer};
