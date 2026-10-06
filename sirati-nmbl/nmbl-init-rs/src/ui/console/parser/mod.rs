//! Input byte-stream → [`ConsoleEvent`] translator.
//!
//! NMBL drives terminal input through [`termwiz::input::InputParser`]
//! (a pure byte-stream parser, no `OnceLock` and no fd ownership)
//! rather than letting `crossterm::event::read` grab stdin behind our
//! back. This module owns the small amount of glue around that:
//!
//! 1. A pre-filter that scans for `CSI 8;rows;cols t` host-terminal
//!    size reports and emits [`ConsoleEvent::Resize`]. termwiz only
//!    synthesises `InputEvent::Resized` from a `SIGWINCH` pipe, which
//!    serial-attached consoles never deliver, so we have to recognise
//!    the in-band report ourselves.
//! 2. A translator from the rest of the byte stream's
//!    [`termwiz::input::InputEvent`] output into the
//!    `crossterm::event::KeyEvent` shape the rest of the UI matches
//!    against. (Crossterm stays as a leaf data-type dep purely so the
//!    App state machine and modal handlers don't have to be rewritten;
//!    none of crossterm's runtime entry points are ever called.)
//!
//! [`InputDecoder`] chains the two over reads of any length: a pasted
//! burst is decoded whole, and an escape sequence split across reads is
//! held back until its rest arrives. All of it is pure (no fd, no
//! syscalls) and unit-tested on canned slices.

mod resize_filter;
mod translator;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests;

use std::collections::VecDeque;

use crossterm::event::KeyEvent;

pub(crate) use translator::TermwizToCrossterm;

use crate::ui::console::ConsoleEvent;
use resize_filter::ResizeFilter;

/// Decoded input not yet surfaced is capped here. Only a consumer that
/// stops polling while the operator keeps typing can reach it; the
/// oldest events are dropped first, never part of a fresh read.
const MAX_PENDING_EVENTS: usize = 1 << 16;

/// Terminal input decoder: resize pre-filter plus termwiz, with the
/// decoded keys, wheel notches and the latest resize queued for the
/// console's poll loop.
pub(crate) struct InputDecoder {
    resize_filter: ResizeFilter,
    key_parser: TermwizToCrossterm,
    forward: Vec<u8>,
    pub(crate) keys: VecDeque<KeyEvent>,
    pub(crate) scrolls: VecDeque<ConsoleEvent>,
    /// Latest host-size report not yet surfaced; an older one is
    /// superseded by a newer one in the same burst.
    pub(crate) resize: Option<ConsoleEvent>,
}

impl InputDecoder {
    pub(crate) fn new() -> Self {
        Self {
            resize_filter: ResizeFilter::new(),
            key_parser: TermwizToCrossterm::new(),
            forward: Vec::new(),
            keys: VecDeque::new(),
            scrolls: VecDeque::new(),
            resize: None,
        }
    }

    /// Decode one read. Nothing in `bytes` is dropped: what does not
    /// complete an event yet is kept for the next read.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.forward.clear();
        let resize = &mut self.resize;
        self.resize_filter
            .feed(bytes, &mut self.forward, |ev| *resize = Some(ev));
        if self.forward.is_empty() {
            return;
        }
        let mut keys = Vec::new();
        let mut scrolls = Vec::new();
        // More bytes may follow, so termwiz keeps a dangling escape
        // sequence for the next read rather than committing it.
        self.key_parser.feed_events(
            &self.forward,
            /*maybe_more=*/ true,
            &mut keys,
            &mut scrolls,
        );
        push_bounded(&mut self.keys, keys);
        push_bounded(&mut self.scrolls, scrolls);
    }
}

fn push_bounded<T>(queue: &mut VecDeque<T>, items: Vec<T>) {
    queue.extend(items);
    let excess = queue.len().saturating_sub(MAX_PENDING_EVENTS);
    if excess > 0 {
        crate::nmbl_warn!("console input backlog full: dropping {excess} oldest events");
        queue.drain(..excess);
    }
}
