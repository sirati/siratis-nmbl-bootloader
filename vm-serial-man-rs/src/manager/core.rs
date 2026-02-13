//! Core VM Manager implementation
//!
//! This module contains the main VmManager struct and orchestration logic

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::Duration;
use tracing::{debug, error, trace, warn};

use crate::buffer::OutputBuffer;

use super::handler::handle_client;
use super::pty::SerialHandler;
use super::qemu::{BootMode, QemuConfig};
use super::utils::{cleanup_stale_sockets, shutdown_qemu_gracefully};

/// VM Manager state
pub struct VmManager {
    name: String,
    disk: PathBuf,
    boot_mode: BootMode,
    memory: u32,
    cores: u32,
    socket_path: PathBuf,
    buffer_lines: usize,
    buffer_seconds: u64,
}

impl VmManager {
    pub fn new(
        name: String,
        disk: PathBuf,
        boot_mode: BootMode,
        memory: u32,
        cores: u32,
        socket: Option<PathBuf>,
        buffer_lines: usize,
        buffer_seconds: u64,
    ) -> Self {
        let socket_path = socket.unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/vm-serial-man-{}.sock", std::process::id()))
        });

        Self {
            name,
            disk,
            boot_mode,
            memory,
            cores,
            socket_path,
            buffer_lines,
            buffer_seconds,
        }
    }

    /// Main manager loop
    pub async fn run(&self) -> Result<()> {
        // Clean up stale sockets
        cleanup_stale_sockets(&self.socket_path).await?;

        // Create Unix socket for control
        let listener = UnixListener::bind(&self.socket_path)
            .context("Failed to bind Unix socket")?;
        debug!("Listening on socket: {}", self.socket_path.display());

        // Get socket directory for QEMU serial socket
        let socket_dir = self.socket_path.parent().unwrap().to_path_buf();

        // Start QEMU
        let qemu_config = QemuConfig {
            name: self.name.clone(),
            disk: self.disk.clone(),
            boot_mode: self.boot_mode.clone(),
            memory: self.memory,
            cores: self.cores,
            socket_dir,
        };

        let mut qemu_process = qemu_config.start().await?;

        // Connect to QEMU serial socket
        let serial_handler = SerialHandler::connect(&qemu_process.serial_socket).await?;
        let serial_writer = serial_handler.get_writer();

        // Create output buffer with Arc<Mutex> for sharing
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(
            self.buffer_lines,
            Duration::from_secs(self.buffer_seconds),
        )));

        // Channel for broadcasting output lines to all listeners
        let (output_tx, _output_rx) = broadcast::channel::<String>(1000);

        // Channel for shutdown signal
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Spawn serial reader task
        let _serial_reader_handle = serial_handler.spawn_reader(buffer.clone(), output_tx.clone());

        // Set up signal handlers for graceful shutdown
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("Failed to create SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("Failed to create SIGINT handler")?;

        // Main accept loop
        loop {
            tokio::select! {
                // Accept new client connection
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let serial_writer_clone = serial_writer.clone();
                            let output_tx_clone = output_tx.clone();
                            let buffer_clone = buffer.clone();
                            let shutdown_tx_clone = shutdown_tx.clone();

                            tokio::spawn(async move {
                                if let Err(e) = handle_client(
                                    stream,
                                    serial_writer_clone,
                                    output_tx_clone,
                                    buffer_clone,
                                    shutdown_tx_clone,
                                )
                                .await
                                {
                                    error!("Client handler error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept connection: {}", e);
                        }
                    }
                }

                // Check for shutdown signal
                _ = shutdown_rx.recv() => {
                    debug!("Shutdown signal received via command");
                    break;
                }

                // Handle SIGTERM
                _ = sigterm.recv() => {
                    debug!("SIGTERM received, shutting down gracefully");
                    break;
                }

                // Handle SIGINT (Ctrl+C)
                _ = sigint.recv() => {
                    debug!("SIGINT received, shutting down gracefully");
                    break;
                }

                // Check if QEMU exited
                _ = qemu_process.child.wait() => {
                    warn!("QEMU process exited");
                    break;
                }
            }
        }

        // Cleanup
        debug!("Shutting down VM manager gracefully");
        drop(output_tx);

        // Try graceful QEMU shutdown
        shutdown_qemu_gracefully(qemu_process.child, Duration::from_secs(5)).await?;

        // Clean up socket
        let _ = fs::remove_file(&self.socket_path).await;
        debug!("VM manager shutdown complete");

        Ok(())
    }
}

/// Main entry point for manager
pub async fn run_manager(
    name: String,
    disk: PathBuf,
    boot_mode: BootMode,
    memory: u32,
    cores: u32,
    socket: Option<PathBuf>,
    buffer_lines: usize,
    buffer_seconds: u64,
) -> Result<()> {
    let manager = VmManager::new(
        name,
        disk,
        boot_mode,
        memory,
        cores,
        socket,
        buffer_lines,
        buffer_seconds,
    );

    manager.run().await
}
