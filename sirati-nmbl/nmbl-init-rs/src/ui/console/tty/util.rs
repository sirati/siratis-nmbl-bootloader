//! Small error-conversion helpers and duration utilities for the tty backend.

use std::time::Duration;

use crate::error::NmblError;

pub(super) fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

pub(super) fn tw_err(e: termwiz::Error) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::other(format!("termwiz: {e}")),
    }
}

pub(super) fn rustix_io_err(e: rustix::io::Errno) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::from(e),
    }
}

pub(super) fn duration_to_ms(d: Duration) -> i32 {
    let ms = d.as_millis();
    if ms > i32::MAX as u128 {
        i32::MAX
    } else {
        i32::try_from(ms).unwrap_or(i32::MAX)
    }
}
