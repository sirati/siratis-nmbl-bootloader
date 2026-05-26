//! Headless terminal pipeline.
//!
//! Pipes ratatui-rendered bytes through `alacritty_terminal::Term`
//! so the compositor can walk the resulting cell grid.
//!
//! Skeleton only — Phase 2 fills in the body.

#![allow(dead_code, unused_variables)]

use crate::splash::types::CellDims;

/// Wraps an `alacritty_terminal::Term` plus its `vte` parser.
pub struct SplashTerminal {
    _private: (),
}

impl SplashTerminal {
    /// Build a Term sized to the given grid.
    pub fn new(_dims: CellDims) -> Self {
        Self { _private: () }
    }

    /// Push parsed bytes from a ratatui frame into the underlying Term.
    pub fn feed(&mut self, _bytes: &[u8]) {}
}
