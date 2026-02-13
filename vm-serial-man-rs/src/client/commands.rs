//! Client command functions
//!
//! This module contains the main client commands for interacting with the VM manager

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, trace, warn};

use crate::protocol::{CommandRequest, CommandResponse, CommandType};

use super::utils::find_socket;

/// Send a command to the VM manager
pub async fn send_command(
    command: String,
    duration: u64,
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
            CommandResponse::BufferedOutput(lines) => {
                println!("=== Buffered Output (last 10s/100 lines) ===");
                if lines.is_empty() {
                    println!("(no buffered output available)");
                } else {
                    for line in lines {
                        println!("{}", line);
                    }
                }
                println!();
            }
            CommandResponse::CommandInjected(cmd) => {
                println!("=== Injecting command: {} ===", cmd);
                println!();
            }
            CommandResponse::OutputLine(output) => {
                println!("{}", output);
            }
            CommandResponse::Complete => {
                println!();
                println!("=== Command Complete ===");
                trace!("Received Complete response, total responses: {}", response_count);
                break;
            }
            CommandResponse::Error(err) => {
                eprintln!("Error: {}", err);
                warn!("Received Error response: {}", err);
                break;
            }
            CommandResponse::Stopped => {
                println!("VM manager stopped");
                debug!("Received Stopped response");
                break;
            }
        }
    }

    debug!("Client completed, received {} responses total", response_count);
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
            println!("VM manager stopped successfully");
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
