//! Serial I/O handling via Unix socket
//!
//! This module manages reading from and writing to the QEMU serial via Unix socket

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{sleep, timeout, Duration};
use tracing::{debug, error, trace, warn};

/// How long the serial stream may sit idle with un-terminated bytes pending
/// before we flush them as a line anyway.
///
/// This MUST stay comfortably larger than the gap between newline-terminated
/// lines of normal boot / interactive output: that output already flushes the
/// instant its `\n` arrives (see the newline branch in `spawn_reader`), so a
/// long idle window never affects it. The idle flush only ever fires for a
/// guest that emits a stream with NO trailing newline — e.g. NMBL's
/// full-screen ratatui LUKS modal, which repaints with cursor-positioning
/// escapes and never sends `\n`. Without this flush those frames accumulate
/// invisibly inside `read` and `tail`/`find`/`trigger` see an empty history.
const IDLE_FLUSH: Duration = Duration::from_millis(750);

/// Read chunk size for the incremental serial reader.
const READ_CHUNK: usize = 4096;

use crate::buffer::OutputBuffer;
use crate::stdout_safe::write_stdout_line;

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

    /// Spawn a task to read from serial and update buffer.
    ///
    /// The reader does NOT depend solely on newline framing. It accumulates raw
    /// bytes into `pending` (the current, not-yet-`\n`-terminated logical line)
    /// and tracks `flushed_len`, the count of leading bytes of `pending` that
    /// have already been emitted by an *idle flush*.
    ///
    /// Two paths emit a line:
    ///  * **Newline path** (unchanged behaviour): on every complete `\n`, we
    ///    emit the logical line — trimmed, echoed to stdout, pushed to the
    ///    buffer and broadcast — exactly as before. On the normal fast path
    ///    `flushed_len == 0`, so the whole line is emitted byte-for-byte
    ///    identically to the old `read_line` loop. Newline-framed output (all
    ///    boot / interactive guests) therefore still flushes the instant its
    ///    `\n` arrives and is completely unaffected.
    ///  * **Idle path**: if a `read` times out (`IDLE_FLUSH`) AND `pending` has
    ///    un-emitted bytes (`flushed_len < pending.len()`), we emit just the
    ///    un-emitted tail `pending[flushed_len..]` and advance `flushed_len` to
    ///    cover it. This makes stuck TUI frames searchable without ever
    ///    re-emitting bytes: each byte is emitted by exactly one path. When the
    ///    line finally completes, the newline path emits only the remainder
    ///    `pending[flushed_len..]` and resets the pending state.
    pub fn spawn_reader(
        mut self,
        buffer: Arc<Mutex<OutputBuffer>>,
        output_tx: broadcast::Sender<String>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Raw bytes of the current logical line (no trailing `\n` yet).
            let mut pending: Vec<u8> = Vec::new();
            // Bytes at the front of `pending` already emitted via an idle flush.
            let mut flushed_len: usize = 0;
            let mut chunk = [0u8; READ_CHUNK];

            // Emit one logical line: trim, echo to stdout (EPIPE-safe), push to
            // the searchable buffer and broadcast. Skips empty lines, matching
            // the old loop.
            async fn emit(
                bytes: &[u8],
                buffer: &Arc<Mutex<OutputBuffer>>,
                output_tx: &broadcast::Sender<String>,
            ) {
                let text = String::from_utf8_lossy(bytes);
                let trimmed = text.trim_end().to_string();
                if !trimmed.is_empty() {
                    trace!("Serial output: {}", trimmed);
                    // Mirror to stdout for console visibility, but never panic
                    // if the downstream pipe is gone (EPIPE).
                    write_stdout_line(&trimmed);
                    buffer.lock().await.push(trimmed.clone());
                    let _ = output_tx.send(trimmed);
                }
            }

            loop {
                // `AsyncReadExt::read` is cancellation-safe: if the timeout
                // elapses before any byte arrives, nothing is consumed, so no
                // serial data is ever dropped by the idle flush.
                match timeout(IDLE_FLUSH, self.reader.read(&mut chunk)).await {
                    // Read completed within the idle window.
                    Ok(Ok(0)) => {
                        warn!("Serial socket closed");
                        break;
                    }
                    Ok(Ok(n)) => {
                        // Split the freshly read bytes on `\n`. Everything up to
                        // and including a newline completes the current logical
                        // line; the trailing fragment (if any) becomes the new
                        // `pending`.
                        let mut rest = &chunk[..n];
                        while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
                            pending.extend_from_slice(&rest[..nl]);
                            // Emit only the bytes not already idle-flushed; on
                            // the common path flushed_len == 0 => whole line.
                            emit(&pending[flushed_len..], &buffer, &output_tx).await;
                            pending.clear();
                            flushed_len = 0;
                            rest = &rest[nl + 1..];
                        }
                        // Stash the un-terminated remainder for later.
                        pending.extend_from_slice(rest);
                    }
                    // Idle timeout: flush any un-emitted pending tail so stuck
                    // TUI frames / pre-modal output become searchable.
                    Err(_) => {
                        if flushed_len < pending.len() {
                            emit(&pending[flushed_len..], &buffer, &output_tx).await;
                            flushed_len = pending.len();
                        }
                    }
                    Ok(Err(e)) => {
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
