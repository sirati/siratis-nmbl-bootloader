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
//! The pre-filter is a tiny state machine (~80 lines) over a fixed
//! `[u8; 64]` buffer. The translator is a single `match` from
//! `termwiz::KeyCode` to `crossterm::KeyCode`. Both are pure functions
//! (no fd, no syscalls) and fully unit-tested on canned slices.

mod resize_filter;
mod translator;

pub(super) use resize_filter::ResizeFilter;
pub(crate) use translator::TermwizToCrossterm;
