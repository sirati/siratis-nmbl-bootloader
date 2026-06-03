//! Attach command client implementation - interactive console

use anyhow::{Context, Result};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::protocol::{AttachRequest, CommandResponse, CommandType};
use crate::stdout_safe::{write_stdout_line, write_stdout_newline};

use super::utils::find_socket;

/// Attach to VM console in interactive mode
pub async fn attach_console(socket: Option<PathBuf>) -> Result<()> {
    let socket_path = match socket {
        Some(p) => p,
        None => find_socket().await?,
    };

    debug!("Connecting to socket: {}", socket_path.display());
    let stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to VM manager")?;

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send attach request
    let attach_req = AttachRequest { initial_lines: 100 };
    let cmd = CommandType::Attach(attach_req);
    let cmd_bytes = cmd.to_bytes();

    let writer_shared = Arc::new(Mutex::new(writer));
    {
        let mut writer_lock = writer_shared.lock().await;
        writer_lock
            .write_all(&cmd_bytes)
            .await
            .context("Failed to send attach request")?;
        writer_lock.flush().await?;
    }

    // Receive initial info and recent lines
    let mut line = String::new();
    let mut attached = false;

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("Failed to read response")?;

        if n == 0 {
            warn!("Connection closed unexpectedly");
            return Ok(());
        }

        let response: CommandResponse = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to parse initial response: {}", e);
                continue;
            }
        };

        match response {
            CommandResponse::AttachInfo(timestamp, total_lines) => {
                write_stdout_line("=== Attaching to VM Console ===");
                write_stdout_line(&format!("Last output: {}", timestamp));
                write_stdout_line(&format!("Total lines: {}", total_lines));
                write_stdout_line("Showing last 100 lines:");
                write_stdout_line("---");
            }
            CommandResponse::OutputLine(output) => {
                // Output lines have newlines stripped, add them back
                write_stdout_line(&output);
            }
            CommandResponse::Attached => {
                write_stdout_line("---");
                write_stdout_line("=== Attached (Press Ctrl-D or type 'exit' + Enter to detach) ===");
                write_stdout_newline();
                attached = true;
                break;
            }
            _ => {
                warn!("Unexpected initial response: {:?}", response);
            }
        }
    }

    // Spawn task to read stdin and send to manager (line-based)
    let writer_clone = writer_shared.clone();
    let mut stdin_task = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut stdin_reader = BufReader::new(stdin);
        let mut input_line = String::new();

        loop {
            input_line.clear();
            match stdin_reader.read_line(&mut input_line).await {
                Ok(0) => {
                    debug!("EOF on stdin, detaching");
                    let mut writer_lock = writer_clone.lock().await;
                    let _ = writer_lock.write_all(b"DETACH\n").await;
                    let _ = writer_lock.flush().await;
                    break;
                }
                Ok(_) => {
                    // Check for exit command
                    if input_line.trim() == "exit" || input_line.trim() == "DETACH" {
                        debug!("Exit command detected, detaching");
                        let mut writer_lock = writer_clone.lock().await;
                        let _ = writer_lock.write_all(b"DETACH\n").await;
                        let _ = writer_lock.flush().await;
                        break;
                    }

                    // Send input to manager
                    let mut writer_lock = writer_clone.lock().await;
                    if let Err(e) = writer_lock.write_all(input_line.as_bytes()).await {
                        warn!("Failed to send input: {}", e);
                        break;
                    }
                    if let Err(e) = writer_lock.flush().await {
                        warn!("Failed to flush: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    warn!("Error reading stdin: {}", e);
                    break;
                }
            }
        }
    });

    // Stream output from manager
    loop {
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => {
                        debug!("Connection closed");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            line.clear();
                            continue;
                        }

                        let response: CommandResponse = match serde_json::from_str(trimmed) {
                            Ok(r) => r,
                            Err(e) => {
                                warn!("Failed to parse response: {}", e);
                                line.clear();
                                continue;
                            }
                        };

                        match response {
                            CommandResponse::OutputLine(output) => {
                                // Output lines have newlines stripped, add them back
                                write_stdout_line(&output);
                                io::stdout().flush().ok();
                            }
                            CommandResponse::Detached => {
                                write_stdout_newline();
                                write_stdout_line("=== Detached from VM Console ===");
                                break;
                            }
                            _ => {
                                debug!("Unexpected response type during streaming");
                            }
                        }
                        line.clear();
                    }
                    Err(e) => {
                        warn!("Error reading from manager: {}", e);
                        break;
                    }
                }
            }

            _ = &mut stdin_task => {
                debug!("stdin task finished");
                // Send detach to manager
                let mut writer_lock = writer_shared.lock().await;
                let _ = writer_lock.write_all(b"DETACH\n").await;
                let _ = writer_lock.flush().await;
                break;
            }
        }
    }

    Ok(())
}
