//! Output buffer management for VM serial output
//!
//! This module provides a circular buffer that stores recent VM output lines
//! with timestamps, allowing retrieval of lines within a time window.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::time::Duration;

/// A buffered output line with timestamp
#[derive(Debug, Clone)]
pub struct BufferedLine {
    pub timestamp: DateTime<Utc>,
    pub line: String,
}

/// Circular buffer for VM output lines
#[derive(Debug)]
pub struct OutputBuffer {
    lines: VecDeque<BufferedLine>,
    max_lines: usize,
    max_age: Duration,
}

impl OutputBuffer {
    /// Create a new output buffer
    pub fn new(max_lines: usize, max_age: Duration) -> Self {
        Self {
            lines: VecDeque::with_capacity(max_lines),
            max_lines,
            max_age,
        }
    }

    /// Add a line to the buffer
    pub fn push(&mut self, line: String) {
        let buffered = BufferedLine {
            timestamp: Utc::now(),
            line,
        };

        self.lines.push_back(buffered);

        // Remove old lines if we exceed max_lines
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }

        // Remove lines older than max_age
        self.cleanup_old();
    }

    /// Remove lines older than max_age
    fn cleanup_old(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::from_std(self.max_age).unwrap();

        while let Some(front) = self.lines.front() {
            if front.timestamp < cutoff {
                self.lines.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get all lines within the time window
    pub fn get_recent(&mut self, max_age: Duration) -> Vec<String> {
        self.cleanup_old();

        let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap();

        self.lines
            .iter()
            .filter(|l| l.timestamp >= cutoff)
            .map(|l| l.line.clone())
            .collect()
    }

    /// Get all buffered lines
    pub fn get_all(&mut self) -> Vec<String> {
        self.cleanup_old();
        self.lines.iter().map(|l| l.line.clone()).collect()
    }

    /// Clear the buffer
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Get the number of buffered lines
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_buffer_basic() {
        let mut buffer = OutputBuffer::new(10, Duration::from_secs(60));

        buffer.push("line 1".to_string());
        buffer.push("line 2".to_string());
        buffer.push("line 3".to_string());

        assert_eq!(buffer.len(), 3);
        let all = buffer.get_all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], "line 1");
        assert_eq!(all[2], "line 3");
    }

    #[test]
    fn test_buffer_max_lines() {
        let mut buffer = OutputBuffer::new(3, Duration::from_secs(60));

        for i in 0..5 {
            buffer.push(format!("line {}", i));
        }

        assert_eq!(buffer.len(), 3);
        let all = buffer.get_all();
        assert_eq!(all[0], "line 2");
        assert_eq!(all[2], "line 4");
    }

    #[test]
    fn test_buffer_time_window() {
        let mut buffer = OutputBuffer::new(100, Duration::from_millis(100));

        buffer.push("old line".to_string());
        sleep(Duration::from_millis(150));
        buffer.push("new line".to_string());

        let recent = buffer.get_recent(Duration::from_millis(50));
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0], "new line");
    }

    #[test]
    fn test_buffer_clear() {
        let mut buffer = OutputBuffer::new(10, Duration::from_secs(60));

        buffer.push("line 1".to_string());
        buffer.push("line 2".to_string());
        assert_eq!(buffer.len(), 2);

        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
    }
}
