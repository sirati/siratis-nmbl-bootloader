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

mod attach;
mod commands;
mod find;
mod status;
mod trigger;
mod utils;

pub use attach::attach_console;
pub use commands::{get_lines, get_tail, send_command, stop_manager};
pub use find::find_in_history;
pub use status::show_status;
pub use trigger::trigger_on_pattern;
