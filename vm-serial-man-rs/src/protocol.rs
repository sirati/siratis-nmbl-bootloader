//! Protocol definitions for VM Serial Manager communication
//!
//! Communication protocol:
//! 1. Client sends CommandType (stop/command)
//! 2. For commands: duration (u64), command string, then optional stdin lines
//! 3. Manager responds with buffered output, injected command marker, and captured output

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Command type sent from client to manager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandType {
    /// Stop the VM manager
    Stop,
    /// Execute a command
    Command(CommandRequest),
    /// Search through history
    Find(FindRequest),
    /// Trigger on pattern match
    Trigger(TriggerRequest),
    /// Attach to console (interactive mode)
    Attach(AttachRequest),
    /// Get specific lines from history
    Lines(LinesRequest),
    /// Get last N lines from history (tail)
    Tail(TailRequest),
}

/// Command request with parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    /// Command to send to VM
    pub command: String,
    /// Duration to capture output
    pub duration: Duration,
    /// Minimum number of previous lines to show
    pub min_prev_lines: usize,
    /// Time window for previous lines (seconds)
    pub prev_lines_within: Duration,
    /// Maximum number of previous lines to show
    pub max_prev_lines: usize,
}

/// Find request - search through history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindRequest {
    /// Regex pattern to search for
    pub pattern: String,
    /// Number of lines before match to show
    pub before: usize,
    /// Number of lines after match to show
    pub after: usize,
    /// Only return first N matches
    pub first_n: Option<usize>,
    /// Only return last N matches
    pub last_n: Option<usize>,
}

/// Trigger request - monitor new output for pattern
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRequest {
    /// Regex pattern to trigger on
    pub pattern: String,
    /// Number of lines before match to capture
    pub lines_before: usize,
    /// Number of lines after match to capture
    pub lines_after: usize,
    /// Timeout to wait for pattern match
    pub match_timeout: Duration,
    /// Timeout to wait for each line after match
    pub line_timeout: Duration,
}

/// Attach request - interactive console
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachRequest {
    /// Number of recent lines to send initially
    pub initial_lines: usize,
}

/// Lines request - get specific line range
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinesRequest {
    /// Starting line number (1-indexed)
    pub start: usize,
    /// Ending line number (inclusive)
    pub end: usize,
}

/// Tail request - get last N lines
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailRequest {
    /// Number of lines to retrieve from the end
    pub lines: usize,
}

/// Metadata about buffered output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferedOutputInfo {
    /// Lines being sent
    pub lines: Vec<String>,
    /// Total number of lines in buffer
    pub total_lines: usize,
    /// Starting line number (1-indexed)
    pub start_line: usize,
    /// Seconds since last output was received
    pub last_output_age_secs: Option<f64>,
}

/// Response from manager to client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandResponse {
    /// Buffered output before command with metadata
    BufferedOutput(BufferedOutputInfo),
    /// Command injection marker
    CommandInjected(String),
    /// Captured output line
    OutputLine(String),
    /// Command completed successfully
    Complete,
    /// Error occurred
    Error(String),
    /// Manager stopped
    Stopped,
    /// Find result - (line_number, context_lines)
    FindMatch(usize, Vec<String>),
    /// Trigger matched - matched line followed by N lines
    TriggerMatch(Vec<String>),
    /// Trigger timed out without match
    TriggerTimeout,
    /// Total number of matches found (sent before filtered results)
    /// Total number of matches found
    TotalMatches(usize),
    /// Attach initial info - (last_output_timestamp, total_lines)
    AttachInfo(String, usize),
    /// Attached successfully
    Attached,
    /// Input from attach client
    AttachInput(String),
    /// Detached from console
    Detached,
    /// Lines response - specific line range
    Lines(Vec<String>),
    /// Tail response - last N lines
    Tail(Vec<String>),
}

impl CommandType {
    /// Serialize to JSON bytes with newline delimiter
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).unwrap();
        bytes.push(b'\n');
        bytes
    }

    /// Deserialize from JSON bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

impl CommandResponse {
    /// Serialize to JSON bytes with newline delimiter
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).unwrap();
        bytes.push(b'\n');
        bytes
    }

    /// Deserialize from JSON bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_serialization() {
        let cmd = CommandType::Command(CommandRequest {
            command: "help".to_string(),
            duration: Duration::from_secs(5),
            min_prev_lines: 10,
            prev_lines_within: Duration::from_secs(10),
            max_prev_lines: 30,
        });

        let bytes = cmd.to_bytes();
        let deserialized = CommandType::from_bytes(&bytes[..bytes.len() - 1]).unwrap();

        match deserialized {
            CommandType::Command(req) => {
                assert_eq!(req.command, "help");
                assert_eq!(req.duration, Duration::from_secs(5));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_stop_serialization() {
        let cmd = CommandType::Stop;
        let bytes = cmd.to_bytes();
        let deserialized = CommandType::from_bytes(&bytes[..bytes.len() - 1]).unwrap();

        matches!(deserialized, CommandType::Stop);
    }

    #[test]
    fn test_response_serialization() {
        let resp = CommandResponse::OutputLine("test output".to_string());
        let bytes = resp.to_bytes();
        let deserialized = CommandResponse::from_bytes(&bytes[..bytes.len() - 1]).unwrap();

        match deserialized {
            CommandResponse::OutputLine(line) => {
                assert_eq!(line, "test output");
            }
            _ => panic!("Wrong variant"),
        }
    }
}
