//! Client command functions
//!
//! This module contains the main client commands for interacting with the VM manager

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, trace, warn};

use crate::protocol::{CommandRequest, CommandResponse, CommandType, LinesRequest, TailRequest};
use crate::stdout_safe::{write_stdout_line, write_stdout_newline};

use super::utils::find_socket;

/// Send a command to the VM manager
pub async fn send_command(
    command: String,
    duration: u64,
    min_prev_lines: usize,
    prev_lines_within: u64,
    max_prev_lines: usize,
    socket: Option<PathBuf>,
    _read_stdin: bool,
) -> Result<()> {
    let socket_path = match socket {
        Some(path) => path,
        None => find_socket().await?,
    };

    debug!("Connecting to VM manager at: {}", socket_path.display());

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to VM manager")?;

    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Send command
    let cmd_type = CommandType::Command(CommandRequest {
        command: command.clone(),
        duration: std::time::Duration::from_secs(duration),
        min_prev_lines,
        prev_lines_within: std::time::Duration::from_secs(prev_lines_within),
        max_prev_lines,
    });

    let cmd_bytes = cmd_type.to_bytes();
    trace!("Sending command: {} bytes", cmd_bytes.len());
    writer.write_all(&cmd_bytes).await?;
    writer.flush().await?;

    debug!("Command sent: {}", command);
    debug!("Capturing output for {}s...", duration);
    trace!("Waiting for responses...");

    // Read responses
    let mut line = String::new();
    let mut response_count = 0;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        trace!("Read {} bytes from server", n);
        if n == 0 {
            trace!("Server closed connection");
            break;
        }

        trace!("Raw response: {:?}", line.trim());
        let response: CommandResponse = serde_json::from_str(line.trim())
            .context(format!("Failed to parse response: {}", line.trim()))?;
        response_count += 1;
        trace!("Parsed response #{}: {:?}", response_count, response);

        match response {
            CommandResponse::BufferedOutput(info) => {
                // Build header with metadata
                let header = if info.total_lines == 0 {
                    "=== Buffered Output: empty ===".to_string()
                } else {
                    let end_line = info.start_line + info.lines.len() - 1;
                    let age_str = match info.last_output_age_secs {
                        Some(age) if age < 1.0 => "just now".to_string(),
                        Some(age) if age < 60.0 => format!("{:.0}s ago", age),
                        Some(age) if age < 3600.0 => format!("{:.0}m ago", age / 60.0),
                        Some(age) => format!("{:.1}h ago", age / 3600.0),
                        None => "unknown".to_string(),
                    };
                    format!(
                        "=== Buffered Output: lines {}-{} of {} total (last: {}) ===",
                        info.start_line, end_line, info.total_lines, age_str
                    )
                };
                write_stdout_line(&header);

                if info.lines.is_empty() {
                    write_stdout_line("(no recent output)");
                } else {
                    for line in info.lines {
                        write_stdout_line(&line);
                    }
                }
                write_stdout_newline();
            }
            CommandResponse::CommandInjected(cmd) => {
                write_stdout_line(&format!("=== Injecting command: {} ===", cmd));
                write_stdout_newline();
            }
            CommandResponse::OutputLine(output) => {
                write_stdout_line(&output);
            }
            CommandResponse::Complete => {
                write_stdout_newline();
                write_stdout_line("=== Command Complete ===");
                trace!(
                    "Received Complete response, total responses: {}",
                    response_count
                );
                break;
            }
            CommandResponse::Error(err) => {
                eprintln!("Error: {}", err);
                warn!("Received Error response: {}", err);
                break;
            }
            CommandResponse::Stopped => {
                write_stdout_line("VM manager stopped");
                debug!("Received Stopped response");
                break;
            }
            CommandResponse::FindMatch(_, _) => {
                warn!("Unexpected FindMatch response in send command");
            }
            CommandResponse::TriggerMatch(_) => {
                warn!("Unexpected TriggerMatch response in send command");
            }
            CommandResponse::TriggerTimeout => {
                warn!("Unexpected TriggerTimeout response in send command");
            }
            CommandResponse::TotalMatches(_) => {
                warn!("Unexpected TotalMatches response in send command");
            }
            CommandResponse::AttachInfo(_, _) => {
                warn!("Unexpected AttachInfo response in send command");
            }
            CommandResponse::Attached => {
                warn!("Unexpected Attached response in send command");
            }
            CommandResponse::AttachInput(_) => {
                warn!("Unexpected AttachInput response in send command");
            }
            CommandResponse::Detached => {
                warn!("Unexpected Detached response in send command");
            }
            CommandResponse::Lines(_) => {
                warn!("Unexpected Lines response in send command");
            }
            CommandResponse::Tail(_) => {
                warn!("Unexpected Tail response in send command");
            }
        }
    }

    debug!(
        "Client completed, received {} responses total",
        response_count
    );
    Ok(())
}

/// Stop the VM manager
pub async fn stop_manager(socket: Option<PathBuf>) -> Result<()> {
    let socket_path = match socket {
        Some(path) => path,
        None => find_socket().await?,
    };

    debug!("Connecting to VM manager at: {}", socket_path.display());

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to VM manager")?;

    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Send stop command
    let cmd_type = CommandType::Stop;
    let cmd_bytes = cmd_type.to_bytes();
    trace!("Sending stop command: {} bytes", cmd_bytes.len());
    writer.write_all(&cmd_bytes).await?;
    writer.flush().await?;

    debug!("Stop command sent");
    trace!("Waiting for response...");

    // Wait for response
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    trace!("Read {} bytes from server: {:?}", n, line.trim());

    let response: CommandResponse = serde_json::from_str(line.trim())
        .context(format!("Failed to parse response: {}", line.trim()))?;
    trace!("Parsed response: {:?}", response);

    match response {
        CommandResponse::Stopped => {
            write_stdout_line("VM manager stopped successfully");
            Ok(())
        }
        CommandResponse::Error(err) => {
            anyhow::bail!("Failed to stop VM manager: {}", err);
        }
        _ => {
            anyhow::bail!("Unexpected response from VM manager");
        }
    }
}

/// Get specific lines from output history
pub async fn get_lines(start: usize, end: usize, socket: Option<PathBuf>) -> Result<()> {
    let socket_path = match socket {
        Some(path) => path,
        None => find_socket().await?,
    };

    debug!("Connecting to VM manager at: {}", socket_path.display());

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to VM manager")?;

    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Send lines request
    let cmd_type = CommandType::Lines(LinesRequest { start, end });

    let cmd_bytes = cmd_type.to_bytes();
    trace!("Sending lines request: {} bytes", cmd_bytes.len());
    writer.write_all(&cmd_bytes).await?;
    writer.flush().await?;

    debug!("Lines request sent: {}-{}", start, end);

    // Read response
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .context("Failed to read response")?;

    if n == 0 {
        anyhow::bail!("Connection closed before receiving response");
    }

    trace!("Read {} bytes from server: {:?}", n, line.trim());

    let response: CommandResponse = serde_json::from_str(line.trim())
        .context(format!("Failed to parse response: {}", line.trim()))?;
    trace!("Parsed response: {:?}", response);

    match response {
        CommandResponse::Lines(lines) => {
            if lines.is_empty() {
                write_stdout_line(&format!("No lines found in range {}-{}", start, end));
            } else {
                write_stdout_line(&format!(
                    "=== Lines {}-{} (showing {} lines) ===",
                    start,
                    end,
                    lines.len()
                ));
                for (i, line) in lines.iter().enumerate() {
                    write_stdout_line(&format!("[{}] {}", start + i, line));
                }
            }
            Ok(())
        }
        CommandResponse::Error(err) => {
            anyhow::bail!("Failed to get lines: {}", err);
        }
        _ => {
            anyhow::bail!("Unexpected response from VM manager");
        }
    }
}

/// Get last N lines from output history (tail)
pub async fn get_tail(lines: usize, socket: Option<PathBuf>) -> Result<()> {
    let socket_path = match socket {
        Some(path) => path,
        None => find_socket().await?,
    };

    debug!("Connecting to VM manager at: {}", socket_path.display());

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to VM manager")?;

    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Send tail request
    let cmd_type = CommandType::Tail(TailRequest { lines });

    let cmd_bytes = cmd_type.to_bytes();
    trace!("Sending tail request: {} bytes", cmd_bytes.len());
    writer.write_all(&cmd_bytes).await?;
    writer.flush().await?;

    debug!("Tail request sent: last {} lines", lines);

    // Read response
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .context("Failed to read response")?;

    if n == 0 {
        anyhow::bail!("Connection closed before receiving response");
    }

    trace!("Read {} bytes from server: {:?}", n, line.trim());

    let response: CommandResponse = serde_json::from_str(line.trim())
        .context(format!("Failed to parse response: {}", line.trim()))?;
    trace!("Parsed response: {:?}", response);

    match response {
        CommandResponse::Tail(tail_lines) => {
            if tail_lines.is_empty() {
                write_stdout_line("No lines in buffer");
            } else {
                write_stdout_line(&format!(
                    "=== Last {} lines (showing {} lines) ===",
                    lines,
                    tail_lines.len()
                ));
                for line in tail_lines.iter() {
                    write_stdout_line(line);
                }
            }
            Ok(())
        }
        CommandResponse::Error(err) => {
            anyhow::bail!("Failed to get tail: {}", err);
        }
        _ => {
            anyhow::bail!("Unexpected response from VM manager");
        }
    }
}
