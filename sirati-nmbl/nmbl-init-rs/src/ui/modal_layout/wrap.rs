/// Hand-rolled word-wrap. Splits each input line on ASCII whitespace
/// and re-emits chunks whose char-width does not exceed `width`. A
/// single word longer than `width` is hard-split at `width` chars.
/// Empty input yields one empty line so the caller always has at least
/// one row to render.
///
/// Char counting (not byte counting) so multi-byte text (UTF-8) doesn't
/// throw the wrap width off — same correctness target as
/// `view::char_column_for_byte_cursor`.
pub fn wrap_message(msg: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut out: Vec<String> = Vec::new();
    // Preserve hard newlines but treat the rest of each line as
    // whitespace-separated words.
    for paragraph in msg.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_chars: usize = 0;
        for word in paragraph.split_whitespace() {
            let word_chars = word.chars().count();
            if word_chars > width {
                // Flush current line first if non-empty.
                if line_chars > 0 {
                    out.push(std::mem::take(&mut line));
                    line_chars = 0;
                }
                // Hard-split the oversized word.
                let mut buf = String::new();
                let mut buf_chars: usize = 0;
                for c in word.chars() {
                    if buf_chars >= width {
                        out.push(std::mem::take(&mut buf));
                        buf_chars = 0;
                    }
                    buf.push(c);
                    buf_chars = buf_chars.saturating_add(1);
                }
                if buf_chars > 0 {
                    line = buf;
                    line_chars = buf_chars;
                }
                continue;
            }
            let needed = if line_chars == 0 {
                word_chars
            } else {
                line_chars.saturating_add(1).saturating_add(word_chars)
            };
            if needed > width {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
                line_chars = word_chars;
            } else {
                if line_chars > 0 {
                    line.push(' ');
                    line_chars = line_chars.saturating_add(1);
                }
                line.push_str(word);
                line_chars = line_chars.saturating_add(word_chars);
            }
        }
        if line_chars > 0 {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
