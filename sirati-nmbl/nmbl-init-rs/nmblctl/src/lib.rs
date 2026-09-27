//! `nmblctl` — system-side control and inspection for the NMBL bootloader.
//!
//! Root-only. Subcommands:
//! * `chain`  — the whole boot chain as configured (coloured, paged).
//! * `status` — everything about the current boot, setup and health.
//! * `reboot-rescue` — set the rescue sentinel durably, then reboot.
//! * `reboot-into`   — one-shot "boot this generation next", then reboot.
//! * `default`       — set/show the persistent default generation.
//!
//! The library half holds the pieces that are worth testing without a live
//! system: colour rendering, the pager fallback decision, argument parsing,
//! the one-shot / default / rescue flag-file formats, and the state readers.
//! `main.rs` wires them to the real filesystem, systemd and `reboot`.

pub mod args;
pub mod color;
pub mod flags;
pub mod pager;

pub use args::{Cli, ColorChoice, Command, parse_args};
pub use color::{Palette, Style};
pub use flags::{DefaultSelection, OneShotSelection, RescueRequest};
pub use pager::{PagerChoice, decide_pager};
