//! Tests for the tty console backend.

use std::os::fd::AsFd;
use std::path::Path;

use super::TtyConsole;
use super::caps::BUNDLED_TERMINFO;
use super::caps::caps_from_env_with_fallback;
use super::kd::{KD_TEXT, enter_kd_graphics, restore_kd_mode};

/// `/dev/null` is not a tty, so opening it as a [`TtyConsole`]
/// must fail at the `enter_raw` step (ENOTTY).
#[test]
fn open_path_on_non_tty_errors() {
    if std::fs::metadata("/dev/null").is_err() {
        return;
    }
    let res = TtyConsole::open_path(Path::new("/dev/null"));
    assert!(res.is_err(), "expected ENOTTY-style failure on /dev/null");
}

#[test]
fn enter_kd_graphics_on_non_vt_returns_none() {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let result = enter_kd_graphics(file.as_fd());
    assert!(
        result.is_none(),
        "expected None on non-VT fd (KDGETMODE→ENOTTY), got {result:?}"
    );
}

#[test]
fn restore_kd_mode_on_non_vt_does_not_panic() {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        Ok(f) => f,
        Err(_) => return,
    };
    restore_kd_mode(file.as_fd(), KD_TEXT);
}

/// The bundled terminfo must parse and define `cup`
/// (`CursorAddress`). This is the single fact that keeps termwiz off
/// its row/col-transposing CSI fallback in `move_cursor_absolute`.
#[test]
fn bundled_terminfo_defines_cursor_address() {
    use terminfo::capability::CursorAddress;
    let db = terminfo::Database::from_buffer(BUNDLED_TERMINFO)
        .expect("bundled xterm-256color terminfo must parse");
    assert!(
        db.get::<CursorAddress>().is_some(),
        "bundled terminfo must define cup (CursorAddress); without it \
         termwiz transposes row/col on every incremental repaint"
    );
}

/// Regression pin for the horizontal→vertical-down flip. Render an
/// absolute cursor move `(x=col, y=row)` through the *actual*
/// capabilities NMBL builds and assert termwiz emits
/// `CSI {row+1};{col+1} H` — row first, then column. The pre-fix
/// no-terminfo fallback emitted `CSI {col+1};{row+1} H` (transposed),
/// which is exactly the corruption the operator reported.
#[test]
fn absolute_cursor_move_is_row_then_col() {
    use std::io::Write;
    use termwiz::render::RenderTty;
    use termwiz::render::terminfo::TerminfoRenderer;
    use termwiz::surface::{Change, Position};

    struct CaptureTty {
        buf: Vec<u8>,
    }
    impl Write for CaptureTty {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl RenderTty for CaptureTty {
        fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
            Ok((200, 60))
        }
    }

    let caps = caps_from_env_with_fallback().expect("caps must build");
    let mut renderer = TerminfoRenderer::new(caps);
    let mut tty = CaptureTty { buf: Vec::new() };

    // x = column 7, y = row 3. A correct backend emits a move to
    // row 3, column 7.
    let change = Change::CursorPosition {
        x: Position::Absolute(7),
        y: Position::Absolute(3),
    };
    renderer
        .render_to(&[change], &mut tty)
        .expect("render must succeed");

    let out = String::from_utf8_lossy(&tty.buf);
    assert!(
        out.contains("\x1b[4;8H"),
        "expected row-first CSI cursor address \\x1b[4;8H (row 3+1, col 7+1), got {out:?}"
    );
    assert!(
        !out.contains("\x1b[8;4H"),
        "transposed (col-first) cursor address \\x1b[8;4H must NOT appear: {out:?}"
    );
}
