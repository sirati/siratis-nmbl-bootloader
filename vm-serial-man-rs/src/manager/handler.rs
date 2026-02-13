//! Client connection handler
//!
//! This module handles individual client connections and command processing

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::Duration;
use tracing::{debug, trace, warn};

use crate::buffer::OutputBuffer;
use crate::protocol::{CommandRequest, CommandResponse, CommandType};

/// Handle a client connection
pub async fn handle_client(
    stream: UnixStream,
    serial_writer: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
    output_tx: broadcast::Sender<String>,
    buffer: Arc<Mutex<OutputBuffer>>,
    shutdown_tx: mpsc::Sender<()>,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = writer;
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
            writer.write_all(&resp_bytes).await?;
            writer.flush().await?;
            debug!("Stop response sent successfully");
            // Signal shutdown
            let _ = shutdown_tx.send(()).await;
            Ok(())
        }

        CommandType::Command(cmd_req) => {
            handle_command(cmd_req, serial_writer, writer, output_tx, buffer, reader).await
        }
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

    // Get buffered output
    let buffer_snapshot = buffer.lock().await.get_all();
    trace!("Buffer snapshot has {} lines", buffer_snapshot.len());

    // Send buffered output
    let resp = CommandResponse::BufferedOutput(buffer_snapshot);
    let resp_bytes = resp.to_bytes();
    trace!("Sending BufferedOutput response: {} bytes", resp_bytes.len());
    writer.write_all(&resp_bytes).await?;
    writer.flush().await?;
    trace!("BufferedOutput sent and flushed");

    // Send command injected marker
    let resp = CommandResponse::CommandInjected(cmd_req.command.clone());
    let resp_bytes = resp.to_bytes();
    trace!("Sending CommandInjected response: {} bytes", resp_bytes.len());
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
        serial.write_all(cmd_bytes.as_bytes())
            .await?;
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

    debug!("Command completed successfully with {} output lines", line_count);
    Ok(())
}
