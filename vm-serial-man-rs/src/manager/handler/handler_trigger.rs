//! Trigger command handler - monitor new output for pattern match

use anyhow::Result;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::buffer::OutputBuffer;
use crate::protocol::{CommandResponse, TriggerRequest};

/// Handle a trigger request - monitor new output for pattern, stream results
pub async fn handle_trigger(
    trigger_req: TriggerRequest,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    output_tx: broadcast::Sender<String>,
    buffer: Arc<Mutex<OutputBuffer>>,
) -> Result<()> {
    debug!("Processing trigger: pattern={}", trigger_req.pattern);

    // Compile regex
    let re = match regex::Regex::new(&trigger_req.pattern) {
        Ok(r) => r,
        Err(e) => {
            let resp = CommandResponse::Error(format!("Invalid regex: {}", e));
            writer.write_all(&resp.to_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    // Get current buffer index (start monitoring from here)
    let start_idx = buffer.lock().await.current_index();
    debug!("Starting trigger from line index {}", start_idx);

    // Subscribe to new output
    let mut output_rx = output_tx.subscribe();

    // Maintain a rolling buffer for lines_before
    let mut rolling_buffer: VecDeque<String> = VecDeque::new();

    // Phase 1: Wait for pattern match (up to match_timeout)
    debug!(
        "Waiting for pattern match (timeout: {:?})",
        trigger_req.match_timeout
    );
    let match_start = tokio::time::Instant::now();
    let mut triggered = false;

    loop {
        let elapsed = match_start.elapsed();
        if elapsed >= trigger_req.match_timeout {
            debug!("Match timeout after {:?}", elapsed);
            break;
        }

        let remaining = trigger_req.match_timeout - elapsed;

        match tokio::time::timeout(remaining, output_rx.recv()).await {
            Ok(Ok(line)) => {
                debug!("Trigger received line: {:?}", line);
                let matches = re.is_match(&line);
                debug!(
                    "Checking pattern '{}' against line: matches={}",
                    trigger_req.pattern, matches
                );

                if matches {
                    debug!("Trigger matched on line: {}", line);
                    triggered = true;

                    // Stream lines_before from rolling buffer
                    if trigger_req.lines_before > 0 && !rolling_buffer.is_empty() {
                        debug!("Streaming {} lines before match", rolling_buffer.len());
                        let before_lines: Vec<String> = rolling_buffer.iter().cloned().collect();
                        let resp = CommandResponse::TriggerMatch(before_lines);
                        writer.write_all(&resp.to_bytes()).await?;
                        writer.flush().await?;
                    }

                    // Stream the matching line
                    let resp = CommandResponse::TriggerMatch(vec![line]);
                    writer.write_all(&resp.to_bytes()).await?;
                    writer.flush().await?;

                    break;
                } else {
                    // Add to rolling buffer for potential lines_before
                    if trigger_req.lines_before > 0 {
                        rolling_buffer.push_back(line);
                        // Keep only the last N lines
                        if rolling_buffer.len() > trigger_req.lines_before {
                            rolling_buffer.pop_front();
                        }
                    }
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                warn!("Trigger receiver lagged by {} messages, continuing...", n);
                continue;
            }
            Ok(Err(e)) => {
                warn!("Output channel closed during match wait: {}", e);
                break;
            }
            Err(_) => {
                debug!("Match timeout waiting for pattern");
                break;
            }
        }
    }

    if !triggered {
        // Pattern never matched
        let resp = CommandResponse::TriggerTimeout;
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;

        let resp = CommandResponse::Complete;
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;

        debug!("Trigger completed with match timeout");
        return Ok(());
    }

    // Phase 2: Capture lines_after with line-level timeout
    debug!(
        "Capturing {} lines after match (line timeout: {:?})",
        trigger_req.lines_after, trigger_req.line_timeout
    );

    for i in 0..trigger_req.lines_after {
        match tokio::time::timeout(trigger_req.line_timeout, output_rx.recv()).await {
            Ok(Ok(line)) => {
                debug!(
                    "Captured line {}/{}: {:?}",
                    i + 1,
                    trigger_req.lines_after,
                    line
                );

                // Stream each line immediately
                let resp = CommandResponse::TriggerMatch(vec![line]);
                writer.write_all(&resp.to_bytes()).await?;
                writer.flush().await?;
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                warn!(
                    "Trigger receiver lagged by {} messages during capture, continuing...",
                    n
                );
                continue;
            }
            Ok(Err(e)) => {
                warn!("Output channel closed during line capture: {}", e);
                break;
            }
            Err(_) => {
                debug!(
                    "Line timeout after capturing {}/{} lines",
                    i, trigger_req.lines_after
                );
                break;
            }
        }
    }

    // Send completion
    let resp = CommandResponse::Complete;
    writer.write_all(&resp.to_bytes()).await?;
    writer.flush().await?;

    debug!("Trigger completed successfully");
    Ok(())
}
