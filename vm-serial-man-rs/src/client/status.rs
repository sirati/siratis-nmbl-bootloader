//! Status display for VM managers
//!
//! This module provides functionality to scan for and display information about running VM managers

use anyhow::Result;

use crate::stdout_safe::write_stdout_line;

use super::utils::is_process_running;

/// Show status of running VM managers
///
/// Scans /tmp for VM manager sockets and displays their status,
/// including whether the associated process is still running
pub async fn show_status() -> Result<()> {
    write_stdout_line("Scanning for VM managers...");

    let tmp_dir = std::path::Path::new("/tmp");
    let mut entries = tokio::fs::read_dir(tmp_dir).await?;
    let mut found = false;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if let Some(name) = path.file_name() {
            if let Some(name_str) = name.to_str() {
                if name_str.starts_with("vm-serial-man-") && name_str.ends_with(".sock") {
                    found = true;
                    write_stdout_line(&format!("Found VM manager socket: {}", path.display()));

                    // Try to extract PID from socket name
                    if let Some(pid_str) = name_str
                        .strip_prefix("vm-serial-man-")
                        .and_then(|s| s.strip_suffix(".sock"))
                    {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            // Check if process is running
                            if is_process_running(pid) {
                                write_stdout_line(&format!("  Status: Running (PID: {})", pid));
                            } else {
                                write_stdout_line(
                                    "  Status: Socket exists but process not running (stale socket)",
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if !found {
        write_stdout_line("No VM managers found");
    }

    Ok(())
}
