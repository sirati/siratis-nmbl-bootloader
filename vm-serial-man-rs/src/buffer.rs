//! Output buffer management for VM serial output
//!
//! This module provides a buffer that stores all VM output lines indefinitely
//! with timestamps, allowing retrieval and search operations.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::time::Duration;

/// A buffered output line with timestamp
#[derive(Debug, Clone)]
pub struct BufferedLine {
    pub timestamp: DateTime<Utc>,
    pub line: String,
}

/// Buffer for VM output lines (stores all lines indefinitely)
#[derive(Debug)]
pub struct OutputBuffer {
    lines: VecDeque<BufferedLine>,
    min_lines: usize,  // Minimum lines to return
    min_age: Duration, // Minimum age window to return
}

impl OutputBuffer {
    /// Create a new output buffer
    pub fn new(min_lines: usize, min_age: Duration) -> Self {
        Self {
            lines: VecDeque::new(),
            min_lines,
            min_age,
        }
    }

    /// Add a line to the buffer (keeps indefinitely)
    pub fn push(&mut self, line: String) {
        let buffered = BufferedLine {
            timestamp: Utc::now(),
            line,
        };

        self.lines.push_back(buffered);
    }

    /// Get recent lines: returns lines from last min_age OR last min_lines (whichever is MORE)
    pub fn get_recent(&self) -> Vec<String> {
        self.get_recent_custom(self.min_lines, self.min_age, None)
    }

    /// Get recent lines with custom parameters
    pub fn get_recent_custom(
        &self,
        min_lines: usize,
        min_age: Duration,
        max_lines: Option<usize>,
    ) -> Vec<String> {
        let cutoff = Utc::now() - chrono::Duration::from_std(min_age).unwrap();

        // Find lines within time window
        let time_based: Vec<_> = self
            .lines
            .iter()
            .rev()
            .take_while(|l| l.timestamp >= cutoff)
            .collect();

        // Determine how many lines to return (at least min_lines, or all time-based lines)
        let mut count = time_based.len().max(min_lines);

        // Apply max_lines cap if specified
        if let Some(max) = max_lines {
            count = count.min(max);
        }

        // Return last 'count' lines
        self.lines
            .iter()
            .rev()
            .take(count)
            .rev()
            .map(|l| l.line.clone())
            .collect()
    }

    /// Get all buffered lines (entire history)
    pub fn get_all(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.line.clone()).collect()
    }

    /// Search through all lines with regex, return matches with context
    pub fn search(
        &self,
        pattern: &str,
        before: usize,
        after: usize,
        first_n: Option<usize>,
        last_n: Option<usize>,
    ) -> Result<Vec<(usize, Vec<String>)>, regex::Error> {
        let re = regex::Regex::new(pattern)?;
        let lines: Vec<&String> = self.lines.iter().map(|l| &l.line).collect();
        let mut matches: Vec<(usize, Vec<String>)> = Vec::new();

        for (idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                let start = idx.saturating_sub(before);
                let end = (idx + after + 1).min(lines.len());

                let context_lines: Vec<String> =
                    lines[start..end].iter().map(|s| (*s).clone()).collect();

                matches.push((idx, context_lines));
            }
        }

        // Apply first_n or last_n filtering
        let filtered_matches = if let Some(n) = first_n {
            matches.into_iter().take(n).collect()
        } else if let Some(n) = last_n {
            matches.into_iter().rev().take(n).rev().collect()
        } else {
            matches
        };

        Ok(filtered_matches)
    }

    /// Get lines starting from a specific index
    pub fn get_from_index(&self, start_idx: usize) -> Vec<String> {
        self.lines
            .iter()
            .skip(start_idx)
            .map(|l| l.line.clone())
            .collect()
    }

    /// Get current line count (for tracking read position)
    pub fn current_index(&self) -> usize {
        self.lines.len()
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

    /// Get the timestamp of the last output line
    pub fn last_output_timestamp(&self) -> Option<DateTime<Utc>> {
        self.lines.back().map(|l| l.timestamp)
    }

    /// Get a specific range of lines (1-indexed, inclusive)
    pub fn get_lines_range(&self, start: usize, end: usize) -> Vec<String> {
        if start == 0 || start > end || end > self.lines.len() {
            return Vec::new();
        }

        self.lines
            .iter()
            .skip(start - 1)
            .take(end - start + 1)
            .map(|l| l.line.clone())
            .collect()
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
    fn test_buffer_indefinite() {
        let mut buffer = OutputBuffer::new(10, Duration::from_secs(60));

        // Add many lines
        for i in 0..1000 {
            buffer.push(format!("line {}", i));
        }

        // All lines should still be in buffer
        assert_eq!(buffer.len(), 1000);
        let all = buffer.get_all();
        assert_eq!(all.len(), 1000);
        assert_eq!(all[0], "line 0");
        assert_eq!(all[999], "line 999");
    }

    #[test]
    fn test_buffer_get_recent() {
        let mut buffer = OutputBuffer::new(5, Duration::from_secs(60));

        for i in 0..20 {
            buffer.push(format!("line {}", i));
        }

        // Should return at least min_lines (5)
        let recent = buffer.get_recent();
        assert!(recent.len() >= 5);
        // Within 60 seconds, should return all lines
        assert_eq!(recent.len(), 20);
    }

    #[test]
    fn test_buffer_search() {
        let mut buffer = OutputBuffer::new(10, Duration::from_secs(60));

        buffer.push("first line".to_string());
        buffer.push("error: something failed".to_string());
        buffer.push("another line".to_string());
        buffer.push("error: another failure".to_string());
        buffer.push("last line".to_string());

        let matches = buffer.search("error:", 1, 1, None, None).unwrap();
        assert_eq!(matches.len(), 2);

        // First match should have context
        assert_eq!(matches[0].1.len(), 3); // before + match + after
        assert!(matches[0].1[1].contains("error: something failed"));
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
