use crate::ui::console::ConsoleEvent;

/// Longest escape sequence the pre-filter holds back while waiting for
/// the rest of a possible `CSI 8;rows;cols t` report (the longest legal
/// one, `CSI 8;65535;65535t`, is 18 bytes). It bounds only the
/// unfinished sequence carried over to the next read: a longer run is
/// forwarded to termwiz verbatim, never dropped.
pub(crate) const MAX_PARTIAL: usize = 256;

/// Streaming byte filter that splits an input stream into
/// [`ConsoleEvent::Resize`] reports plus the leftover byte stream
/// (which the caller hands to [`termwiz::input::InputParser`] to
/// produce key / mouse / paste events).
///
/// [`Self::feed`] takes a read of any length. Every byte is forwarded,
/// consumed as part of a resize report, or held back as the unfinished
/// escape sequence ending the read, which the next read completes, so
/// `\x1b[8;5` then `0;200t` still parses.
pub(crate) struct ResizeFilter {
    partial: Vec<u8>,
}

impl ResizeFilter {
    pub(crate) fn new() -> Self {
        Self {
            partial: Vec::new(),
        }
    }

    /// Classify `bytes`, appended to any held-back partial sequence.
    /// Non-resize bytes go to `forward` in stream order and each
    /// complete report to `resize`; an unfinished sequence at the end
    /// is kept for the next call.
    pub(crate) fn feed(
        &mut self,
        bytes: &[u8],
        forward: &mut Vec<u8>,
        mut resize: impl FnMut(ConsoleEvent),
    ) {
        let mut stream = std::mem::take(&mut self.partial);
        stream.extend_from_slice(bytes);
        let mut idx = 0usize;
        while let Some(&head) = stream.get(idx) {
            if head != 0x1b {
                forward.push(head);
                idx = idx.saturating_add(1);
                continue;
            }
            let rest = stream.get(idx..).unwrap_or(&[]);
            match recognise_csi_8t(rest) {
                CsiOutcome::Resize {
                    rows,
                    cols,
                    consumed,
                } => {
                    resize(ConsoleEvent::Resize { rows, cols });
                    idx = idx.saturating_add(consumed);
                }
                CsiOutcome::NotMine { consumed } => {
                    // An escape sequence we don't claim: termwiz gets
                    // it verbatim.
                    forward.extend_from_slice(rest.get(..consumed).unwrap_or(rest));
                    idx = idx.saturating_add(consumed.max(1));
                }
                CsiOutcome::NeedMore => {
                    self.partial = rest.to_vec();
                    return;
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn buffered_len(&self) -> usize {
        self.partial.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsiOutcome {
    /// Recognised `CSI 8;rows;cols t`; consumed the listed byte count.
    Resize {
        rows: u16,
        cols: u16,
        consumed: usize,
    },
    /// Either not a CSI at all, or a CSI we don't claim — caller
    /// forwards `consumed` bytes verbatim to termwiz.
    NotMine { consumed: usize },
    /// Buffer truncated mid-sequence — caller leaves the bytes alone
    /// and pushes more on the next read.
    NeedMore,
}

/// Classify a slice that begins with `ESC` (`0x1b`).
fn recognise_csi_8t(bytes: &[u8]) -> CsiOutcome {
    debug_assert!(bytes.first().copied() == Some(0x1b));
    // Need at least ESC [ to commit to a CSI shape.
    let Some(&second) = bytes.get(1) else {
        return CsiOutcome::NeedMore;
    };
    if second != b'[' {
        // Some other escape — let termwiz parse it. We forward only
        // the ESC for now; termwiz reassembles when the next byte
        // arrives.
        return CsiOutcome::NotMine { consumed: 1 };
    }
    // Have ESC [. Walk parameter / intermediate bytes until the final
    // byte (any of 0x40..=0x7e).
    let mut idx = 2usize;
    let final_idx = loop {
        match bytes.get(idx) {
            None => return CsiOutcome::NeedMore,
            Some(&b) if (0x40..=0x7e).contains(&b) => break idx,
            Some(_) => idx = idx.saturating_add(1),
        }
        if idx >= MAX_PARTIAL {
            // Pathological sequence longer than any report — give up
            // and let termwiz handle whatever it can.
            return CsiOutcome::NotMine { consumed: idx };
        }
    };
    let final_byte = bytes.get(final_idx).copied().unwrap_or(0);
    let consumed = final_idx.saturating_add(1);
    if final_byte != b't' {
        return CsiOutcome::NotMine { consumed };
    }
    // Parameters live in `bytes[2..final_idx]`. We accept the form
    // `8;<rows>;<cols>` and nothing else.
    let params = bytes.get(2..final_idx).unwrap_or(&[]);
    let mut parts = params.split(|b| *b == b';');
    let Some(first) = parts.next() else {
        return CsiOutcome::NotMine { consumed };
    };
    if parse_u32(first) != Some(8) {
        return CsiOutcome::NotMine { consumed };
    }
    let Some(rows_bytes) = parts.next() else {
        return CsiOutcome::NotMine { consumed };
    };
    let Some(cols_bytes) = parts.next() else {
        return CsiOutcome::NotMine { consumed };
    };
    let (Some(rows), Some(cols)) = (parse_u32(rows_bytes), parse_u32(cols_bytes)) else {
        return CsiOutcome::NotMine { consumed };
    };
    // Only the strict 3-tuple form `8;rows;cols`. A 4th param (e.g.
    // `8;1;2;3`) is a different escape; forward verbatim.
    if parts.next().is_some() {
        return CsiOutcome::NotMine { consumed };
    }
    let rows = u16::try_from(rows).unwrap_or(u16::MAX);
    let cols = u16::try_from(cols).unwrap_or(u16::MAX);
    CsiOutcome::Resize {
        rows,
        cols,
        consumed,
    }
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut acc: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        let digit = u32::from(b.saturating_sub(b'0'));
        acc = acc.checked_mul(10)?.checked_add(digit)?;
    }
    Some(acc)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn feed_all(f: &mut ResizeFilter, bytes: &[u8]) -> (Vec<u8>, Vec<ConsoleEvent>) {
        let mut forwarded = Vec::new();
        let mut resizes = Vec::new();
        f.feed(bytes, &mut forwarded, |ev| resizes.push(ev));
        (forwarded, resizes)
    }

    fn only_resize(ev: &[ConsoleEvent]) -> (u16, u16) {
        assert_eq!(ev.len(), 1, "{ev:?}");
        match ev[0] {
            ConsoleEvent::Resize { rows, cols } => (rows, cols),
            other => panic!("expected Resize, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_forwards_nothing() {
        let mut f = ResizeFilter::new();
        let (fwd, ev) = feed_all(&mut f, b"");
        assert!(fwd.is_empty());
        assert!(ev.is_empty());
    }

    #[test]
    fn plain_ascii_forwards_verbatim() {
        let mut f = ResizeFilter::new();
        let (fwd, ev) = feed_all(&mut f, b"abc");
        assert_eq!(fwd, b"abc");
        assert!(ev.is_empty());
    }

    #[test]
    fn csi_8_50_200_emits_resize_and_consumes_bytes() {
        let mut f = ResizeFilter::new();
        let (fwd, ev) = feed_all(&mut f, b"\x1b[8;50;200t");
        assert!(fwd.is_empty(), "resize bytes must NOT be forwarded");
        assert_eq!(only_resize(&ev), (50, 200));
    }

    #[test]
    fn csi_8_1_1_degenerate_but_valid() {
        let mut f = ResizeFilter::new();
        let (_, ev) = feed_all(&mut f, b"\x1b[8;1;1t");
        assert_eq!(only_resize(&ev), (1, 1));
    }

    #[test]
    fn interleaved_char_resize_char() {
        let mut f = ResizeFilter::new();
        let (fwd, ev) = feed_all(&mut f, b"a\x1b[8;30;120tb");
        assert_eq!(fwd, b"ab");
        assert_eq!(only_resize(&ev), (30, 120));
    }

    #[test]
    fn partial_then_completion() {
        let mut f = ResizeFilter::new();
        let (fwd, ev) = feed_all(&mut f, b"\x1b[8;5");
        assert!(fwd.is_empty(), "partial CSI must not forward bytes yet");
        assert!(ev.is_empty());
        assert!(f.buffered_len() > 0, "partial buffer retained");
        let (fwd2, ev2) = feed_all(&mut f, b"0;200t");
        assert!(fwd2.is_empty());
        assert_eq!(only_resize(&ev2), (50, 200));
        assert_eq!(f.buffered_len(), 0);
    }

    #[test]
    fn unknown_csi_forwarded_to_termwiz() {
        let mut f = ResizeFilter::new();
        // ESC [ A is "Up". Not ours; must forward verbatim.
        let (fwd, ev) = feed_all(&mut f, b"\x1b[A");
        assert_eq!(fwd, b"\x1b[A");
        assert!(ev.is_empty());
    }

    #[test]
    fn csi_8_with_extra_params_dropped() {
        // `CSI 8;rows;cols;extra t` — we don't accept the 4-param form.
        // Forward verbatim so termwiz can ignore it itself.
        let mut f = ResizeFilter::new();
        let (fwd, ev) = feed_all(&mut f, b"\x1b[8;1;2;3t");
        assert_eq!(fwd, b"\x1b[8;1;2;3t");
        assert!(ev.is_empty());
    }

    #[test]
    fn a_read_longer_than_any_buffer_is_forwarded_whole() {
        let mut f = ResizeFilter::new();
        let burst: Vec<u8> = (0..8192u32).map(|i| b'a' + (i % 26) as u8).collect();
        let (fwd, ev) = feed_all(&mut f, &burst);
        assert_eq!(fwd, burst);
        assert!(ev.is_empty());
    }

    #[test]
    fn an_overlong_unterminated_csi_is_forwarded_not_held() {
        let mut f = ResizeFilter::new();
        let mut burst = b"\x1b[".to_vec();
        burst.extend(std::iter::repeat_n(b'1', MAX_PARTIAL * 2));
        let (fwd, _) = feed_all(&mut f, &burst);
        assert!(f.buffered_len() <= MAX_PARTIAL, "{}", f.buffered_len());
        assert_eq!(fwd.len() + f.buffered_len(), burst.len());
    }
}
