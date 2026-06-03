//! Trigger command client implementation

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, trace, warn};

use crate::protocol::{CommandResponse, CommandType, TriggerRequest};
use crate::stdout_safe::{write_stdout_line, write_stdout_newline};

use super::utils::find_socket;

/// Trigger on pattern match in new VM output
pub async fn trigger_on_pattern(
    pattern: String,
    lines_before: usize,
    lines_after: usize,
    match_timeout: u64,
    line_timeout: u64,
    socket: Option<PathBuf>,
) -> Result<()> {
    let socket_path = match socket {
        Some(p) => p,
        None => find_socket().await?,
    };

    debug!("Connecting to socket: {}", socket_path.display());
    let stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to VM manager")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send trigger request
    let trigger_req = TriggerRequest {
        pattern: pattern.clone(),
        lines_before,
        lines_after,
        match_timeout: std::time::Duration::from_secs(match_timeout),
        line_timeout: std::time::Duration::from_secs(line_timeout),
    };

    let cmd = CommandType::Trigger(trigger_req);
    let cmd_bytes = cmd.to_bytes();

    trace!("Sending trigger request: {} bytes", cmd_bytes.len());
    writer
        .write_all(&cmd_bytes)
        .await
        .context("Failed to send trigger request")?;
    writer.flush().await?;
    trace!("Trigger request sent and flushed");

    write_stdout_line(&format!("=== Waiting for trigger: {} ===", pattern));
    if lines_before > 0 {
        write_stdout_line(&format!("Will capture {} lines before match", lines_before));
    }
    write_stdout_line(&format!("Will capture {} lines after match", lines_after));
    write_stdout_line(&format!(
        "Match timeout: {}s, Line timeout: {}s",
        match_timeout, line_timeout
    ));
    write_stdout_newline();

    // Receive responses
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("Failed to read response")?;

        if n == 0 {
            warn!("Connection closed unexpectedly");
            break;
        }

        trace!("Received {} bytes: {:?}", n, line.trim());

        let response: CommandResponse = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to parse response: {}", e);
                continue;
            }
        };

        trace!("Parsed response: {:?}", response);

        match response {
            CommandResponse::TriggerMatch(captured) => {
                // Stream output as it arrives
                for output_line in captured {
                    write_stdout_line(&output_line);
                }
            }
            CommandResponse::TriggerTimeout => {
                write_stdout_line("=== Trigger Timeout ===");
                write_stdout_line(&format!(
                    "Pattern did not match within {} seconds",
                    match_timeout
                ));
            }
            CommandResponse::Complete => {
                break;
            }
            CommandResponse::Error(err) => {
                eprintln!("Error: {}", err);
                break;
            }
            _ => {
                warn!("Unexpected response type: {:?}", response);
            }
        }
    }

    Ok(())
}
