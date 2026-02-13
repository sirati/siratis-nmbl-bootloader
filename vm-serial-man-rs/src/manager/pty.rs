//! Serial I/O handling via Unix socket
//!
//! This module manages reading from and writing to the QEMU serial via Unix socket

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, trace, warn};

use crate::buffer::OutputBuffer;

/// Serial I/O handler using Unix socket
pub struct SerialHandler {
    pub reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    pub writer: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
}

impl SerialHandler {
    /// Connect to QEMU serial socket with retries
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        debug!("Connecting to QEMU serial socket: {}", socket_path.display());

        // Wait for socket to be created by QEMU
        let mut attempts = 0;
        let max_attempts = 20;

        let stream = loop {
            if attempts >= max_attempts {
                anyhow::bail!("Timeout waiting for QEMU serial socket");
            }

            match UnixStream::connect(socket_path).await {
                Ok(stream) => {
                    debug!("Connected to QEMU serial socket");
                    break stream;
                }
                Err(e) if attempts < max_attempts - 1 => {
                    trace!("Attempt {}/{}: {}", attempts + 1, max_attempts, e);
                    sleep(Duration::from_millis(100)).await;
                    attempts += 1;
                }
                Err(e) => {
                    return Err(e).context("Failed to connect to QEMU serial socket");
                }
            }
        };

        let (reader, writer) = tokio::io::split(stream);
        let reader = BufReader::new(reader);
        let writer = Arc::new(Mutex::new(writer));

        Ok(Self { reader, writer })
    }

    /// Spawn a task to read from serial and update buffer
    pub fn spawn_reader(
        mut self,
        buffer: Arc<Mutex<OutputBuffer>>,
        output_tx: broadcast::Sender<String>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                match self.reader.read_line(&mut line).await {
                    Ok(0) => {
                        warn!("Serial socket closed");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end().to_string();
                        if !trimmed.is_empty() {
                            trace!("Serial output: {}", trimmed);
                            // Print to stdout for console visibility
                            println!("{}", trimmed);
                            // Add to buffer
                            buffer.lock().await.push(trimmed.clone());
                            // Broadcast to listeners
                            let _ = output_tx.send(trimmed);
                        }
                        line.clear();
                    }
                    Err(e) => {
                        error!("Error reading from serial socket: {}", e);
                        break;
                    }
                }
            }
        })
    }

    /// Get a clone of the writer
    pub fn get_writer(&self) -> Arc<Mutex<tokio::io::WriteHalf<UnixStream>>> {
        self.writer.clone()
    }

    /// Write data to serial
    pub async fn write(&self, data: &str) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(data.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Write a line to serial (appends newline)
    pub async fn write_line(&self, line: &str) -> Result<()> {
        self.write(&format!("{}\n", line)).await
    }
}
