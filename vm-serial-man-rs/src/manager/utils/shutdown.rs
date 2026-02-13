//! Graceful shutdown utilities for QEMU processes
//!
//! This module handles the graceful shutdown of QEMU processes with fallback to force kill

use anyhow::Result;
use tokio::process::Child;
use tokio::time::Duration;
use tracing::{debug, warn};

/// Attempt to gracefully shut down a QEMU process
///
/// This function:
/// 1. Sends SIGTERM to the QEMU process
/// 2. Waits up to `timeout` seconds for the process to exit
/// 3. If the process doesn't exit in time, force kills it
pub async fn shutdown_qemu_gracefully(mut qemu_process: Child, timeout: Duration) -> Result<()> {
    debug!("Attempting graceful QEMU shutdown...");

    if let Some(id) = qemu_process.id() {
        // Send SIGTERM to QEMU
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        if let Err(e) = kill(Pid::from_raw(id as i32), Signal::SIGTERM) {
            warn!("Failed to send SIGTERM to QEMU: {}", e);
            // Try to kill directly
            let _ = qemu_process.kill().await;
            let _ = qemu_process.wait().await;
            return Ok(());
        }

        // Wait up to timeout for QEMU to exit
        match tokio::time::timeout(timeout, qemu_process.wait()).await {
            Ok(Ok(status)) => {
                debug!("QEMU exited gracefully with status: {:?}", status);
            }
            Ok(Err(e)) => {
                warn!("Error waiting for QEMU: {}", e);
                let _ = qemu_process.kill().await;
            }
            Err(_) => {
                warn!("QEMU did not exit within timeout, force killing");
                let _ = qemu_process.kill().await;
                let _ = qemu_process.wait().await;
            }
        }
    } else {
        // Process already exited
        let _ = qemu_process.wait().await;
    }

    Ok(())
}
