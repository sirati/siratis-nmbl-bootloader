//! VM Manager module
//!
//! This module is split into several submodules for better organization:
//! - `qemu`: QEMU process management
//! - `pty`: PTY I/O handling
//! - `handler`: Client connection handling
//! - `core`: Main VmManager orchestration
//! - `screenshot`: Framebuffer capture via QEMU monitor

mod core;
mod firmware;
mod handler;
mod pty;
mod qemu;
pub mod screenshot;
mod utils;

pub use core::run_manager;
pub use qemu::{BootMode, Display, SecureBoot, TpmConfig, TpmKind};
