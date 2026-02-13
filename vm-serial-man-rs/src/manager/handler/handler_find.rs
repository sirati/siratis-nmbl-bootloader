//! Find command handler - search through output history

use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::debug;

use crate::buffer::OutputBuffer;
use crate::protocol::{CommandResponse, FindRequest};

/// Handle a find request - search through history
pub async fn handle_find(
    find_req: FindRequest,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    buffer: Arc<Mutex<OutputBuffer>>,
) -> Result<()> {
    debug!("Processing find: pattern={}", find_req.pattern);

    let buffer_lock = buffer.lock().await;
    let matches = match buffer_lock.search(
        &find_req.pattern,
        find_req.before,
        find_req.after,
        find_req.first_n,
        find_req.last_n,
    ) {
        Ok(m) => m,
        Err(e) => {
            drop(buffer_lock);
            let resp = CommandResponse::Error(format!("Invalid regex: {}", e));
            writer.write_all(&resp.to_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let total_matches = matches.len();
    drop(buffer_lock);

    debug!("Found {} matches", total_matches);

    // Send total match count first
    let resp = CommandResponse::TotalMatches(total_matches);
    writer.write_all(&resp.to_bytes()).await?;
    writer.flush().await?;

    // Send each match
    for (line_num, context) in matches {
        let resp = CommandResponse::FindMatch(line_num, context);
        writer.write_all(&resp.to_bytes()).await?;
        writer.flush().await?;
    }

    // Send completion
    let resp = CommandResponse::Complete;
    writer.write_all(&resp.to_bytes()).await?;
    writer.flush().await?;

    debug!("Find completed successfully");
    Ok(())
}
