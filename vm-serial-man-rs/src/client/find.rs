//! Find command client implementation

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, trace, warn};

use crate::protocol::{CommandResponse, CommandType, FindRequest};

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

    println!("=== Searching for pattern: {} ===", pattern);
    if before > 0 || after > 0 {
        println!("Context: {} before, {} after", before, after);
    }
    println!();

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
                    println!("Displaying first {} of {} matches", n.min(total), total);
                } else if let Some(n) = last_n {
                    println!("Displaying last {} of {} matches", n.min(total), total);
                } else {
                    println!("Found {} matches", total);
                }
                println!();
            }
            CommandResponse::FindMatch(line_num, context) => {
                match_count += 1;
                println!("--- Match #{} (line {}) ---", match_count, line_num + 1);
                for ctx_line in context {
                    println!("{}", ctx_line);
                }
                println!();
            }
            CommandResponse::Complete => {
                println!("=== Search Complete ===");
                if total_matches > 0 && match_count < total_matches {
                    println!(
                        "Displayed {} of {} total matches",
                        match_count, total_matches
                    );
                } else {
                    println!("Displayed {} matches", match_count);
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
