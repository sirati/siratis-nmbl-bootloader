use crate::ui::console::ConsoleEvent;

/// Maximum bytes the pre-filter retains while waiting for the rest of
/// a partial `CSI 8;rows;cols t` sequence to arrive. 64 is comfortably
/// larger than the longest legal report (`CSI 8;65535;65535t` at 18
/// bytes); overflow forces a resync.
pub(crate) const BUF: usize = 64;

/// Streaming byte filter that splits an input stream into
/// [`ConsoleEvent::Resize`] reports plus the leftover byte stream
/// (which the caller hands to [`termwiz::input::InputParser`] to
/// produce key / mouse / paste events).
///
/// `push(bytes)` appends incoming bytes; `drain(scratch)` extracts up
/// to one Resize event from the front of the buffer, returning the
/// non-resize prefix to forward to termwiz. Partial sequences are
/// retained across calls so `\x1b[8;5` then `0;200t` still parses.
pub(crate) struct ResizeFilter {
    buf: [u8; BUF],
    len: usize,
}

impl ResizeFilter {
    pub(crate) fn new() -> Self {
        Self {
            buf: [0u8; BUF],
            len: 0,
        }
    }

    /// Append `bytes` to the internal buffer. Bytes that overflow the
    /// buffer drop on the floor — a pathological overflow means the
    /// producer is feeding garbage and the resync path below will
    /// recover. Returns the number actually retained.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> usize {
        let room = BUF.saturating_sub(self.len);
        let take = bytes.len().min(room);
        for i in 0..take {
            let dst = self.len.saturating_add(i);
            if let (Some(d), Some(s)) = (self.buf.get_mut(dst), bytes.get(i)) {
                *d = *s;
            }
        }
        self.len = self.len.saturating_add(take);
        take
    }

    /// Drain the buffer once: emit at most one [`ConsoleEvent::Resize`]
    /// and copy any preceding non-Resize bytes into `scratch`. After
    /// the call, the buffer holds either (a) nothing (everything
    /// classified), (b) a partial sequence (caller pushes more and
    /// retries), or (c) bytes after the Resize.
    ///
    /// Returns `(forwarded_byte_count, Option<Resize>)`. The caller
    /// hands the first `forwarded_byte_count` bytes of `scratch` to
    /// termwiz's `InputParser`.
    pub(crate) fn drain(&mut self, scratch: &mut [u8]) -> (usize, Option<ConsoleEvent>) {
        let mut idx: usize = 0;
        let mut written: usize = 0;
        let buf_len = self.len;
        while idx < buf_len {
            // Look for the ESC introducing a possible CSI 8t.
            let head = self.buf.get(idx).copied().unwrap_or(0);
            if head != 0x1b {
                // Plain byte — forward to scratch and move on.
                Self::copy_into(scratch, written, head);
                written = written.saturating_add(1);
                idx = idx.saturating_add(1);
                continue;
            }
            // Try to recognise CSI 8;<rows>;<cols>t starting at idx.
            match recognise_csi_8t(self.buf.get(idx..buf_len).unwrap_or(&[])) {
                CsiOutcome::Resize {
                    rows,
                    cols,
                    consumed,
                } => {
                    // Drop the bytes BEFORE the CSI (already forwarded
                    // into scratch) AND the CSI itself (consumed). Shift
                    // the trailing tail down to buf[0] so the next call
                    // starts from a clean head.
                    let after = idx.saturating_add(consumed);
                    self.shift_tail_left(after, 0);
                    self.len = self.len.saturating_sub(after);
                    return (written, Some(ConsoleEvent::Resize { rows, cols }));
                }
                CsiOutcome::NotMine { consumed } => {
                    // The bytes at `idx..idx+consumed` are an escape
                    // sequence we don't claim — forward them all to
                    // termwiz verbatim.
                    for j in 0..consumed {
                        if let Some(&b) = self.buf.get(idx.saturating_add(j)) {
                            Self::copy_into(scratch, written, b);
                            written = written.saturating_add(1);
                        }
                    }
                    idx = idx.saturating_add(consumed);
                }
                CsiOutcome::NeedMore => {
                    // Partial sequence at the tail — leave it in the
                    // buffer and emit only what we've classified so
                    // far. The next push fills in the rest.
                    self.shift_tail_left(idx, 0);
                    self.len = self.len.saturating_sub(idx);
                    return (written, None);
                }
            }
        }
        // Drained the whole buffer; nothing partial left.
        self.len = 0;
        (written, None)
    }

    /// Move `self.buf[src..]` to `self.buf[dst..]` in place. `dst <=
    /// src` by contract.
    fn shift_tail_left(&mut self, src: usize, dst: usize) {
        if src == dst {
            return;
        }
        let tail_len = self.len.saturating_sub(src);
        for i in 0..tail_len {
            let from = src.saturating_add(i);
            let to = dst.saturating_add(i);
            if let (Some(b), Some(slot)) = (self.buf.get(from).copied(), self.buf.get_mut(to)) {
                *slot = b;
            }
        }
    }

    fn copy_into(scratch: &mut [u8], at: usize, byte: u8) {
        if let Some(slot) = scratch.get_mut(at) {
            *slot = byte;
        }
    }

    #[cfg(test)]
    pub(crate) fn buffered_len(&self) -> usize {
        self.len
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
        if idx >= BUF {
            // Pathological sequence longer than our buffer — give up
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

    fn drain_all(f: &mut ResizeFilter) -> (Vec<u8>, Vec<ConsoleEvent>) {
        let mut forwarded = Vec::new();
        let mut resizes = Vec::new();
        loop {
            let mut scratch = [0u8; BUF];
            let (n, ev) = f.drain(&mut scratch);
            forwarded.extend_from_slice(scratch.get(..n).unwrap_or(&[]));
            match ev {
                Some(r) => resizes.push(r),
                None if n == 0 => return (forwarded, resizes),
                None => return (forwarded, resizes),
            }
        }
    }

    #[test]
    fn empty_input_drains_to_nothing() {
        let mut f = ResizeFilter::new();
        let (fwd, ev) = drain_all(&mut f);
        assert!(fwd.is_empty());
        assert!(ev.is_empty());
    }

    #[test]
    fn plain_ascii_forwards_verbatim() {
        let mut f = ResizeFilter::new();
        f.push(b"abc");
        let (fwd, ev) = drain_all(&mut f);
        assert_eq!(fwd, b"abc");
        assert!(ev.is_empty());
    }

    #[test]
    fn csi_8_50_200_emits_resize_and_consumes_bytes() {
        let mut f = ResizeFilter::new();
        f.push(b"\x1b[8;50;200t");
        let (fwd, ev) = drain_all(&mut f);
        assert!(fwd.is_empty(), "resize bytes must NOT be forwarded");
        assert_eq!(ev.len(), 1);
        match ev[0] {
            ConsoleEvent::Resize { rows, cols } => {
                assert_eq!(rows, 50);
                assert_eq!(cols, 200);
            }
            other => panic!("expected Resize, got {other:?}"),
        }
    }

    #[test]
    fn csi_8_1_1_degenerate_but_valid() {
        let mut f = ResizeFilter::new();
        f.push(b"\x1b[8;1;1t");
        let (_, ev) = drain_all(&mut f);
        assert_eq!(ev.len(), 1);
        match ev[0] {
            ConsoleEvent::Resize { rows, cols } => {
                assert_eq!(rows, 1);
                assert_eq!(cols, 1);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn interleaved_char_resize_char() {
        let mut f = ResizeFilter::new();
        f.push(b"a\x1b[8;30;120tb");
        // First drain: emits 'a' before the resize, then the resize.
        let mut scratch = [0u8; BUF];
        let (n, ev) = f.drain(&mut scratch);
        assert_eq!(&scratch[..n], b"a");
        match ev {
            Some(ConsoleEvent::Resize { rows, cols }) => {
                assert_eq!(rows, 30);
                assert_eq!(cols, 120);
            }
            other => panic!("expected Resize, got {other:?}"),
        }
        // Second drain: emits 'b'.
        let (n2, ev2) = f.drain(&mut scratch);
        assert_eq!(&scratch[..n2], b"b");
        assert!(ev2.is_none());
    }

    #[test]
    fn partial_then_completion() {
        let mut f = ResizeFilter::new();
        f.push(b"\x1b[8;5");
        let mut scratch = [0u8; BUF];
        let (n, ev) = f.drain(&mut scratch);
        assert_eq!(n, 0, "partial CSI must not forward bytes yet");
        assert!(ev.is_none());
        assert!(f.buffered_len() > 0, "partial buffer retained");
        f.push(b"0;200t");
        let (n2, ev2) = f.drain(&mut scratch);
        assert_eq!(n2, 0);
        match ev2 {
            Some(ConsoleEvent::Resize { rows, cols }) => {
                assert_eq!(rows, 50);
                assert_eq!(cols, 200);
            }
            other => panic!("expected Resize, got {other:?}"),
        }
    }

    #[test]
    fn unknown_csi_forwarded_to_termwiz() {
        let mut f = ResizeFilter::new();
        // ESC [ A is "Up". Not ours; must forward verbatim.
        f.push(b"\x1b[A");
        let (fwd, ev) = drain_all(&mut f);
        assert_eq!(fwd, b"\x1b[A");
        assert!(ev.is_empty());
    }

    #[test]
    fn csi_8_with_extra_params_dropped() {
        // `CSI 8;rows;cols;extra t` — we don't accept the 4-param form.
        // Forward verbatim so termwiz can ignore it itself.
        let mut f = ResizeFilter::new();
        f.push(b"\x1b[8;1;2;3t");
        let (fwd, ev) = drain_all(&mut f);
        assert_eq!(fwd, b"\x1b[8;1;2;3t");
        assert!(ev.is_empty());
    }
}
