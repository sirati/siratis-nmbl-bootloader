//! Attach command handler - interactive console attachment

use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, warn};

use crate::buffer::OutputBuffer;
use crate::protocol::{AttachRequest, CommandResponse};

/// Handle an attach request - interactive console
pub async fn handle_attach(
    attach_req: AttachRequest,
    stream: tokio::net::UnixStream,
    serial_writer: Arc<Mutex<tokio::io::WriteHalf<tokio::net::UnixStream>>>,
    output_tx: broadcast::Sender<String>,
    buffer: Arc<Mutex<OutputBuffer>>,
) -> Result<()> {
    debug!("Processing attach request");

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Get buffer info
    let buffer_lock = buffer.lock().await;
    let total_lines = buffer_lock.len();
    let all_lines = buffer_lock.get_all();

    // Get last timestamp (approximate - use current time as placeholder)
    let last_output_time = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();
    drop(buffer_lock);

    debug!("Attach: {} total lines in buffer", total_lines);

    // Send attach info
    let resp = CommandResponse::AttachInfo(last_output_time, total_lines);
    writer.write_all(&resp.to_bytes()).await?;
    writer.flush().await?;

    // Send recent lines
    let lines_to_send = all_lines
        .iter()
        .rev()
        .take(attach_req.initial_lines)
        .rev()
        .cloned()
        .collect::<Vec<_>>();

    for line in lines_to_send {
        let resp = CommandResponse::OutputLine(line);
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;
    }

    // Send attached marker
    let resp = CommandResponse::Attached;
    writer.write_all(&resp.to_bytes()).await?;
    writer.flush().await?;

    debug!("Client attached, entering streaming mode");

    // Subscribe to new output
    let mut output_rx = output_tx.subscribe();

    // Spawn task to forward client input to serial
    let serial_writer_clone = serial_writer.clone();
    let mut input_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    debug!("Client disconnected");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed == "DETACH" {
                        debug!("Client requested detach");
                        break;
                    }
                    // Forward to serial
                    let mut serial = serial_writer_clone.lock().await;
                    if let Err(e) = serial.write_all(line.as_bytes()).await {
                        warn!("Failed to write to serial: {}", e);
                        break;
                    }
                    if let Err(e) = serial.flush().await {
                        warn!("Failed to flush serial: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    warn!("Error reading from client: {}", e);
                    break;
                }
            }
        }
    });

    // Stream output to client
    loop {
        tokio::select! {
            // Forward VM output to client
            result = output_rx.recv() => {
                match result {
                    Ok(line) => {
                        let resp = CommandResponse::OutputLine(line);
                        if writer.write_all(&resp.to_bytes()).await.is_err() {
                            debug!("Client write failed, detaching");
                            break;
                        }
                        if writer.flush().await.is_err() {
                            debug!("Client flush failed, detaching");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Output channel error: {}", e);
                        break;
                    }
                }
            }

            // Check if input task finished (client detached)
            _ = &mut input_task => {
                debug!("Input task finished, detaching");
                break;
            }
        }
    }

    // Send detached marker
    let resp = CommandResponse::Detached;
    let _ = writer.write_all(&resp.to_bytes()).await;
    let _ = writer.flush().await;

    debug!("Client detached");
    Ok(())
}
