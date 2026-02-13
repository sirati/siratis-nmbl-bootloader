//! Client module for communicating with VM Manager
//!
//! This module provides functionality to interact with running VM managers:
//! - Send commands to a VM and capture output
//! - Stop a running VM manager
//! - Show status of all running VM managers
//!
//! The module is organized into:
//! - `utils`: Helper functions for socket discovery and process checking
//! - `commands`: Command execution (send, stop)
//! - `status`: Status display functionality

mod utils;
mod commands;
mod status;

pub use commands::{send_command, stop_manager};
pub use status::show_status;
