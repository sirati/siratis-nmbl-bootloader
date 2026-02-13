//! Client connection handler
//!
//! This module handles individual client connections and command processing

mod handler_attach;
mod handler_find;
mod handler_trigger;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::Duration;
use tracing::{debug, trace, warn};

use crate::buffer::OutputBuffer;
use crate::protocol::{CommandRequest, CommandResponse, CommandType};

pub use handler_attach::handle_attach;
pub use handler_find::handle_find;
pub use handler_trigger::handle_trigger;

/// Handle a client connection
pub async fn handle_client(
    stream: UnixStream,
    serial_writer: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
    output_tx: broadcast::Sender<String>,
    buffer: Arc<Mutex<OutputBuffer>>,
    shutdown_tx: mpsc::Sender<()>,
) -> Result<()> {
    // Peek at the command type to decide how to handle the stream
    let mut peek_buf = vec![0u8; 1024];
    let mut temp_stream = stream;

    // Read first line to determine command type
    let (reader_half, mut writer_half) = temp_stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut line = String::new();

    debug!("New client connected");

    // Read command from client
    let n = reader
        .read_line(&mut line)
        .await
        .context("Failed to read command")?;

    trace!("Read {} bytes from client: {:?}", n, line.trim());

    let command_type: CommandType =
        serde_json::from_str(line.trim()).context("Failed to parse command")?;

    trace!("Parsed command type: {:?}", command_type);

    match command_type {
        CommandType::Stop => {
            debug!("Stop command received");
            let resp = CommandResponse::Stopped;
            let resp_bytes = resp.to_bytes();
            trace!("Sending response: {} bytes", resp_bytes.len());
            writer_half.write_all(&resp_bytes).await?;
            writer_half.flush().await?;
            debug!("Stop response sent successfully");
            // Signal shutdown
            let _ = shutdown_tx.send(()).await;
            Ok(())
        }

        CommandType::Command(cmd_req) => {
            handle_command(
                cmd_req,
                serial_writer,
                writer_half,
                output_tx,
                buffer,
                reader,
            )
            .await
        }

        CommandType::Find(find_req) => handle_find(find_req, writer_half, buffer).await,

        CommandType::Trigger(trigger_req) => {
            handle_trigger(trigger_req, writer_half, output_tx, buffer).await
        }

        CommandType::Attach(attach_req) => {
            // Attach needs the full stream - reunite the split halves
            let reunited_stream = reader
                .into_inner()
                .reunite(writer_half)
                .context("Failed to reunite stream")?;
            handle_attach(
                attach_req,
                reunited_stream,
                serial_writer,
                output_tx,
                buffer,
            )
            .await
        }

        CommandType::Lines(lines_req) => handle_lines(lines_req, writer_half, buffer).await,
    }
}

/// Handle a command request
async fn handle_command(
    cmd_req: CommandRequest,
    serial_writer: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    output_tx: broadcast::Sender<String>,
    buffer: Arc<Mutex<OutputBuffer>>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<()> {
    debug!("Processing command: {}", cmd_req.command);

    // Get recent buffered output with metadata using custom parameters
    let buffer_guard = buffer.lock().await;
    let recent_lines = buffer_guard.get_recent_custom(
        cmd_req.min_prev_lines,
        cmd_req.prev_lines_within,
        Some(cmd_req.max_prev_lines),
    );
    let total_lines = buffer_guard.len();
    let start_line = if total_lines > 0 {
        total_lines.saturating_sub(recent_lines.len()) + 1
    } else {
        0
    };
    let last_output_age_secs = buffer_guard.last_output_timestamp().map(|ts| {
        let now = chrono::Utc::now();
        (now - ts).num_milliseconds() as f64 / 1000.0
    });
    drop(buffer_guard);

    trace!("Buffer snapshot has {} lines", recent_lines.len());

    // Send buffered output with metadata
    let output_info = crate::protocol::BufferedOutputInfo {
        lines: recent_lines,
        total_lines,
        start_line,
        last_output_age_secs,
    };
    let resp = CommandResponse::BufferedOutput(output_info);
    let resp_bytes = resp.to_bytes();
    trace!(
        "Sending BufferedOutput response: {} bytes",
        resp_bytes.len()
    );
    writer.write_all(&resp_bytes).await?;
    writer.flush().await?;
    trace!("BufferedOutput sent and flushed");

    // Send command injected marker
    let resp = CommandResponse::CommandInjected(cmd_req.command.clone());
    let resp_bytes = resp.to_bytes();
    trace!(
        "Sending CommandInjected response: {} bytes",
        resp_bytes.len()
    );
    writer.write_all(&resp_bytes).await?;
    writer.flush().await?;
    trace!("CommandInjected sent and flushed");

    // Subscribe to output
    let mut output_rx = output_tx.subscribe();
    trace!("Subscribed to output channel");

    // Send command to serial
    {
        let mut serial = serial_writer.lock().await;
        let cmd_bytes = format!("{}\n", cmd_req.command);
        trace!("Writing command to serial: {:?}", cmd_bytes.trim());
        serial.write_all(cmd_bytes.as_bytes()).await?;
        serial.flush().await?;
        trace!("Command written to serial and flushed");
    }

    // Capture output for duration
    let start = tokio::time::Instant::now();
    let mut additional_line = String::new();
    let mut line_count = 0;

    trace!("Starting output capture for {:?}", cmd_req.duration);

    loop {
        let elapsed = start.elapsed();
        if elapsed >= cmd_req.duration {
            break;
        }

        let remaining = cmd_req.duration - elapsed;

        tokio::select! {
            // Receive output from PTY
            result = tokio::time::timeout(remaining, output_rx.recv()) => {
                match result {
                    Ok(Ok(line)) => {
                        line_count += 1;
                        trace!("Received output line #{}: {:?}", line_count, line);
                        let resp = CommandResponse::OutputLine(line);
                        let resp_bytes = resp.to_bytes();
                        trace!("Sending OutputLine response: {} bytes", resp_bytes.len());
                        writer.write_all(&resp_bytes).await?;
                        writer.flush().await?;
                        trace!("OutputLine sent and flushed");
                    }
                    Ok(Err(e)) => {
                        warn!("Output channel closed: {}", e);
                        break;
                    }
                    Err(_) => {
                        trace!("Timeout waiting for output");
                        break;
                    }
                }
            }

            // Read additional input from client (for multi-line commands)
            result = tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut additional_line)) => {
                if let Ok(Ok(n)) = result {
                    if n > 0 {
                        trace!("Received additional input from client: {:?}", additional_line.trim());
                        // Send additional line to serial
                        let mut serial = serial_writer.lock().await;
                        serial.write_all(additional_line.as_bytes()).await?;
                        serial.flush().await?;
                        trace!("Additional input sent to serial");
                        additional_line.clear();
                    }
                }
            }
        }
    }

    trace!("Capture complete, received {} output lines", line_count);

    // Send completion
    let resp = CommandResponse::Complete;
    let resp_bytes = resp.to_bytes();
    trace!("Sending Complete response: {} bytes", resp_bytes.len());
    writer.write_all(&resp_bytes).await?;
    writer.flush().await?;
    trace!("Complete response sent and flushed");

    debug!(
        "Command completed successfully with {} output lines",
        line_count
    );
    Ok(())
}

/// Handle a lines request - get specific line range
async fn handle_lines(
    lines_req: crate::protocol::LinesRequest,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    buffer: Arc<Mutex<OutputBuffer>>,
) -> Result<()> {
    debug!(
        "Processing lines request: {}-{}",
        lines_req.start, lines_req.end
    );

    let buffer_guard = buffer.lock().await;
    let total_lines = buffer_guard.len();

    // Validate range
    if lines_req.start == 0 {
        let resp = CommandResponse::Error("Line numbers are 1-indexed".to_string());
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;
        return Ok(());
    }

    if lines_req.start > lines_req.end {
        let resp = CommandResponse::Error(format!(
            "Invalid range: start ({}) > end ({})",
            lines_req.start, lines_req.end
        ));
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;
        return Ok(());
    }

    if lines_req.end > total_lines {
        let resp = CommandResponse::Error(format!(
            "End line ({}) exceeds buffer size ({})",
            lines_req.end, total_lines
        ));
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;
        return Ok(());
    }

    let lines = buffer_guard.get_lines_range(lines_req.start, lines_req.end);
    drop(buffer_guard);

    let resp = CommandResponse::Lines(lines);
    writer.write_all(&resp.to_bytes()).await?;
    writer.flush().await?;

    debug!("Lines request completed successfully");
    Ok(())
}
