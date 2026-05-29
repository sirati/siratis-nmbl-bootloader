//! Console (stdin/stdout) implementation of [`RescueUi`].
//!
//! Minimal fallback until the ratatui screens (E.2) replace it.

use std::io::{self, Write as _};

use crate::error::{NmblError, Result};

use super::types::{DownloadStatus, HashConfirmation, RescueSource, RescueUi};

/// Minimal stdin/stdout [`RescueUi`] used until the ratatui screens
/// (E.2) replace it. Intentionally side-effect-y on the controlling
/// TTY so it works under any terminal the initramfs hands us.
pub struct ConsoleRescueUi;

impl ConsoleRescueUi {
    /// Helper that reads one line from stdin, trims the trailing
    /// newline, and surfaces I/O failures as a rescue error so the
    /// caller halts cleanly instead of looping on EOF.
    fn read_line(stage: &'static str) -> Result<String> {
        let mut buf = String::new();
        io::stdin()
            .read_line(&mut buf)
            .map_err(|source| NmblError::Rescue {
                stage,
                source: Box::new(NmblError::Io {
                    source,
                    context: "reading operator input".to_string(),
                }),
            })?;
        // Trim ONLY trailing CR/LF — the operator might legitimately
        // want trailing spaces in a URL.
        while buf.ends_with('\n') || buf.ends_with('\r') {
            buf.pop();
        }
        Ok(buf)
    }
}

impl RescueUi for ConsoleRescueUi {
    fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource> {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "--- nmbl rescue: source picker ---");
        let _ = writeln!(stderr, "disk rescue failed:\n  {disk_reason}");
        let _ = writeln!(stderr, "Choose: [n]etwork / [r]eboot / [h]alt");
        let _ = stderr.flush();
        loop {
            let line = Self::read_line("net-ui-pick-source")?;
            match line.trim() {
                "n" | "N" | "network" => return Ok(RescueSource::Network),
                "r" | "R" | "reboot" => return Ok(RescueSource::Reboot),
                "h" | "H" | "halt" => return Ok(RescueSource::Halt),
                _ => {
                    let _ = writeln!(stderr, "unrecognised choice {line:?}; try n/r/h");
                    let _ = stderr.flush();
                }
            }
        }
    }

    fn prompt_url(&mut self, prefill: &str) -> Result<String> {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "--- nmbl rescue: rescue URL ---");
        if !prefill.is_empty() {
            let _ = writeln!(stderr, "default: {prefill}");
            let _ = writeln!(stderr, "Press <enter> to accept, or type a new URL:");
        } else {
            let _ = writeln!(stderr, "Enter rescue URL (http://host/path):");
        }
        let _ = stderr.flush();
        let line = Self::read_line("net-ui-prompt-url")?;
        let trimmed = line.trim();
        if trimmed.is_empty() && !prefill.is_empty() {
            return Ok(prefill.to_string());
        }
        Ok(trimmed.to_string())
    }

    fn progress(&mut self, status: DownloadStatus) {
        // Stay terse — the console UI is a stopgap; rendering a real
        // progress bar belongs to the ratatui impl in E.2.
        let mut stderr = io::stderr();
        match status.total {
            Some(total) if total > 0 => {
                let pct = status.bytes.saturating_mul(100) / total;
                let _ = writeln!(
                    stderr,
                    "[nmbl] download: {} / {} bytes ({}%)",
                    status.bytes, total, pct
                );
            }
            _ => {
                let _ = writeln!(stderr, "[nmbl] download: {} bytes", status.bytes);
            }
        }
    }

    fn confirm_hash(
        &mut self,
        computed_hex: &str,
        prefill_expected: &str,
    ) -> Result<HashConfirmation> {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "--- nmbl rescue: hash confirm ---");
        let _ = writeln!(stderr, "computed: {computed_hex}");
        if !prefill_expected.is_empty() {
            let _ = writeln!(stderr, "expected: {prefill_expected}");
            let match_str = if computed_hex.eq_ignore_ascii_case(prefill_expected) {
                "MATCH"
            } else {
                "MISMATCH"
            };
            let _ = writeln!(stderr, "verdict: {match_str}");
        } else {
            let _ = writeln!(stderr, "no expected hash pre-filled");
        }
        let _ = writeln!(stderr, "Confirm? [y]es / [n]o-mismatch / [a]bort");
        let _ = stderr.flush();
        loop {
            let line = Self::read_line("net-ui-confirm-hash")?;
            match line.trim() {
                "y" | "Y" | "yes" => return Ok(HashConfirmation::Confirmed),
                "n" | "N" | "no" => return Ok(HashConfirmation::Mismatch),
                "a" | "A" | "abort" => return Ok(HashConfirmation::Aborted),
                _ => {
                    let _ = writeln!(stderr, "unrecognised choice {line:?}; try y/n/a");
                    let _ = stderr.flush();
                }
            }
        }
    }
}
