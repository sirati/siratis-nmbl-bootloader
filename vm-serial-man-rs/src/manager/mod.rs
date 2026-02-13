//! VM Manager module
//!
//! This module is split into several submodules for better organization:
//! - `qemu`: QEMU process management
//! - `pty`: PTY I/O handling
//! - `handler`: Client connection handling
//! - `core`: Main VmManager orchestration

mod qemu;
mod pty;
mod handler;
mod core;
mod utils;

pub use core::run_manager;
pub use qemu::BootMode;
