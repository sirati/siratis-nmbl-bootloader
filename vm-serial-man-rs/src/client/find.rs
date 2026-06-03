//! Find command client implementation

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, trace, warn};

use crate::protocol::{CommandResponse, CommandType, FindRequest};
use crate::stdout_safe::{write_stdout_line, write_stdout_newline};

use super::utils::find_socket;

/// Find matching lines in VM output history
pub async fn find_in_history(
    pattern: String,
    before: usize,
    after: usize,
    first_n: Option<usize>,
    last_n: Option<usize>,
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

    // Send find request
    let find_req = FindRequest {
        pattern: pattern.clone(),
        before,
        after,
        first_n,
        last_n,
    };

    let cmd = CommandType::Find(find_req);
    let cmd_bytes = cmd.to_bytes();

    trace!("Sending find request: {} bytes", cmd_bytes.len());
    writer
        .write_all(&cmd_bytes)
        .await
        .context("Failed to send find request")?;
    writer.flush().await?;
    trace!("Find request sent and flushed");

    // Receive responses
    let mut line = String::new();
    let mut match_count = 0;
    let mut total_matches = 0;

    write_stdout_line(&format!("=== Searching for pattern: {} ===", pattern));
    if before > 0 || after > 0 {
        write_stdout_line(&format!("Context: {} before, {} after", before, after));
    }
    write_stdout_newline();

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
            CommandResponse::TotalMatches(total) => {
                total_matches = total;
                if let Some(n) = first_n {
                    write_stdout_line(&format!(
                        "Displaying first {} of {} matches",
                        n.min(total),
                        total
                    ));
                } else if let Some(n) = last_n {
                    write_stdout_line(&format!(
                        "Displaying last {} of {} matches",
                        n.min(total),
                        total
                    ));
                } else {
                    write_stdout_line(&format!("Found {} matches", total));
                }
                write_stdout_newline();
            }
            CommandResponse::FindMatch(line_num, context) => {
                match_count += 1;
                write_stdout_line(&format!("--- Match #{} (line {}) ---", match_count, line_num + 1));
                for ctx_line in context {
                    write_stdout_line(&ctx_line);
                }
                write_stdout_newline();
            }
            CommandResponse::Complete => {
                write_stdout_line("=== Search Complete ===");
                if total_matches > 0 && match_count < total_matches {
                    write_stdout_line(&format!(
                        "Displayed {} of {} total matches",
                        match_count, total_matches
                    ));
                } else {
                    write_stdout_line(&format!("Displayed {} matches", match_count));
                }
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
