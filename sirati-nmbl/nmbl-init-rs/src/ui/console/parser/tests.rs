use crossterm::event::{KeyCode, KeyModifiers};

use super::*;

/// One unit of a pasted burst: its bytes and what it must decode to.
enum Unit {
    Key(&'static [u8], KeyCode),
    Scroll(&'static [u8], bool),
    Resize(&'static [u8], u16, u16),
}

impl Unit {
    fn bytes(&self) -> &'static [u8] {
        match self {
            Unit::Key(b, _) | Unit::Scroll(b, _) | Unit::Resize(b, _, _) => b,
        }
    }
}

/// A burst well over 4 KiB of hex digits, Enter, arrow keys, wheel
/// notches and host-size reports, with what each unit decodes to.
fn burst() -> (Vec<u8>, Vec<KeyCode>, Vec<bool>, (u16, u16)) {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut units = Vec::new();
    let mut resize = 0u16;
    for i in 0..1200usize {
        let digit = HEX.get(i % 16..i % 16 + 1).unwrap_or(b"0");
        units.push(Unit::Key(digit, KeyCode::Char(char::from(digit[0]))));
        match i % 9 {
            0 => units.push(Unit::Key(b"\x1b[A", KeyCode::Up)),
            3 => units.push(Unit::Key(b"\x1b[B", KeyCode::Down)),
            5 => units.push(Unit::Scroll(b"\x1b[<64;10;5M", true)),
            6 => {
                resize = resize.wrapping_add(1);
                units.push(if resize.is_multiple_of(2) {
                    Unit::Resize(b"\x1b[8;40;132t", 40, 132)
                } else {
                    Unit::Resize(b"\x1b[8;50;200t", 50, 200)
                });
            }
            8 => units.push(Unit::Key(b"\r", KeyCode::Enter)),
            _ => {}
        }
    }
    // End on a report so the last resize is known exactly.
    units.push(Unit::Resize(b"\x1b[8;33;99t", 33, 99));
    let mut bytes = Vec::new();
    let mut keys = Vec::new();
    let mut scrolls = Vec::new();
    let mut last = (0, 0);
    for unit in &units {
        bytes.extend_from_slice(unit.bytes());
        match *unit {
            Unit::Key(_, code) => keys.push(code),
            Unit::Scroll(_, up) => scrolls.push(up),
            Unit::Resize(_, rows, cols) => last = (rows, cols),
        }
    }
    (bytes, keys, scrolls, last)
}

fn decode(reads: &[&[u8]]) -> (Vec<KeyCode>, Vec<bool>, Option<ConsoleEvent>) {
    let mut decoder = InputDecoder::new();
    for read in reads {
        decoder.feed(read);
    }
    let keys = decoder
        .keys
        .iter()
        .map(|k| {
            assert_eq!(k.modifiers, KeyModifiers::NONE, "{k:?}");
            k.code
        })
        .collect();
    let scrolls = decoder
        .scrolls
        .iter()
        .map(|s| match s {
            ConsoleEvent::Scroll { up } => *up,
            other => panic!("not a scroll: {other:?}"),
        })
        .collect();
    (keys, scrolls, decoder.resize)
}

#[test]
fn a_pasted_burst_decodes_without_loss_in_one_read() {
    let (bytes, keys, scrolls, (rows, cols)) = burst();
    assert!(bytes.len() >= 4096, "burst is only {} bytes", bytes.len());
    let (got_keys, got_scrolls, resize) = decode(&[&bytes]);
    assert_eq!(got_keys, keys);
    assert_eq!(got_scrolls, scrolls);
    assert!(
        matches!(resize, Some(ConsoleEvent::Resize { rows: r, cols: c }) if r == rows && c == cols)
    );
}

#[test]
fn a_pasted_burst_decodes_without_loss_across_split_reads() {
    let (bytes, keys, scrolls, (rows, cols)) = burst();
    // Uneven read sizes, so reads end inside every kind of escape
    // sequence (asserted below), as a slow serial line delivers them.
    let sizes = [1usize, 2, 3, 5, 7, 11, 13, 64, 65, 255, 4];
    let mut reads = Vec::new();
    let mut at = 0usize;
    let mut splits_inside_escape = 0usize;
    for size in sizes.iter().cycle() {
        if at >= bytes.len() {
            break;
        }
        let end = (at + size).min(bytes.len());
        reads.push(&bytes[at..end]);
        // A read ending inside an escape: the next byte continues it.
        let open_escape = bytes[at.saturating_sub(12)..end]
            .iter()
            .rposition(|b| *b == 0x1b)
            .is_some_and(|esc| {
                let tail = &bytes[at.saturating_sub(12) + esc..end];
                !tail.iter().skip(2).any(|b| (0x40..=0x7e).contains(b))
            });
        if open_escape && end < bytes.len() {
            splits_inside_escape += 1;
        }
        at = end;
    }
    assert!(
        splits_inside_escape > 50,
        "only {splits_inside_escape} splits inside escapes"
    );
    let (got_keys, got_scrolls, resize) = decode(&reads);
    assert_eq!(got_keys.len(), keys.len());
    assert_eq!(got_keys, keys);
    assert_eq!(got_scrolls, scrolls);
    assert!(
        matches!(resize, Some(ConsoleEvent::Resize { rows: r, cols: c }) if r == rows && c == cols)
    );
}

#[test]
fn an_escape_split_after_esc_completes_on_the_next_read() {
    let (keys, _, _) = decode(&[b"a\x1b", b"[", b"Ab"]);
    assert_eq!(keys, [KeyCode::Char('a'), KeyCode::Up, KeyCode::Char('b')]);
}
