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
}

/// Command request with parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    /// Command to send to VM
    pub command: String,
    /// Duration to capture output
    pub duration: Duration,
}

/// Response from manager to client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandResponse {
    /// Buffered output before command
    BufferedOutput(Vec<String>),
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
