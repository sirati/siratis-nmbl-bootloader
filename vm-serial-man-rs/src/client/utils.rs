//! Utility functions for VM Manager client
//!
//! This module contains helper functions for socket discovery and process checking

use anyhow::Result;
use std::path::PathBuf;
use tracing::{debug, trace};

/// Find the first active VM manager socket in /tmp
///
/// This function scans /tmp for VM manager sockets and checks if the
/// associated process is still running. Stale sockets are skipped.
pub async fn find_socket() -> Result<PathBuf> {
    let tmp_dir = std::path::Path::new("/tmp");
    let mut entries = tokio::fs::read_dir(tmp_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if let Some(name) = path.file_name() {
            if let Some(name_str) = name.to_str() {
                if name_str.starts_with("vm-serial-man-") && name_str.ends_with(".sock") {
                    // Check if the socket is stale
                    if let Some(pid_str) = name_str
                        .strip_prefix("vm-serial-man-")
                        .and_then(|s| s.strip_suffix(".sock"))
                    {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            if !is_process_running(pid) {
                                trace!("Skipping stale socket from PID {}", pid);
                                continue;
                            }
                        }
                    }
                    debug!("Found active VM manager socket: {}", path.display());
                    return Ok(path);
                }
            }
        }
    }

    anyhow::bail!("No active VM manager socket found in /tmp")
}

/// Check if a process is running
///
/// Uses signal 0 to check if a process exists without actually sending a signal
pub fn is_process_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
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

    #[cfg(not(unix))]
    {
        false
    }
}
