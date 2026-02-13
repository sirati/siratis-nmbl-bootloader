//! Utility functions for VM Manager
//!
//! This module contains helper functions for process checking and socket cleanup

mod shutdown;

pub use shutdown::shutdown_qemu_gracefully;

use anyhow::Result;
use std::path::PathBuf;
use tokio::fs;
use tracing::{debug, trace};

/// Check if a process is running
pub fn is_process_running(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // Use signal 0 to check if process exists without sending a real signal
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false, // No such process
        Err(nix::errno::Errno::EPERM) => true,  // Permission denied, but exists
        Err(_) => false,
    }
}

/// Clean up stale sockets in the socket directory
pub async fn cleanup_stale_sockets(socket_path: &PathBuf) -> Result<()> {
    // If our socket exists, check if it's stale
    if socket_path.exists() {
        // Try to extract PID from socket name
        if let Some(name) = socket_path.file_name().and_then(|n| n.to_str()) {
            if let Some(pid_str) = name
                .strip_prefix("vm-serial-man-")
                .and_then(|s| s.strip_suffix(".sock"))
            {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    if !is_process_running(pid) {
                        debug!("Removing stale socket from PID {}", pid);
                        let _ = fs::remove_file(socket_path).await;
                    } else {
                        anyhow::bail!("Socket already exists and process {} is still running", pid);
                    }
                }
            }
        }
        // If we can't determine the PID, try to remove it anyway
        if socket_path.exists() {
            let _ = fs::remove_file(socket_path).await;
        }
    }
    Ok(())
}
