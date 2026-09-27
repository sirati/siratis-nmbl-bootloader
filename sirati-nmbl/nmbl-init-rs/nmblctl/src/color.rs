//! ANSI colour rendering with a git-style `--color=auto|always|never` policy.
//!
//! The renderer is a pure string transform: [`Palette`] holds whether colour
//! is on, and [`Style`] wraps text in SGR escapes only when it is. This keeps
//! the whole `chain`/`status` output testable — a test builds a `never`
//! palette and asserts on plain text, or an `always` palette and asserts the
//! escapes are present — without a terminal.

/// A styled-output palette. `enabled` is resolved once from the
/// `--color` policy and whether stdout is a TTY (see
/// [`crate::color::Palette::resolve`]).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    enabled: bool,
}

/// A foreground style, applied via [`Palette::paint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Section headers.
    Header,
    /// A field label.
    Label,
    /// A healthy / affirmative value (green).
    Good,
    /// A warning value (yellow).
    Warn,
    /// A bad / failed value (red).
    Bad,
    /// A dimmed / secondary value.
    Dim,
    /// A neutral highlighted value (cyan).
    Value,
}

impl Style {
    /// The SGR parameter for this style's foreground (plus bold for headers).
    fn sgr(self) -> &'static str {
        match self {
            Style::Header => "1;36", // bold cyan
            Style::Label => "1",     // bold
            Style::Good => "32",     // green
            Style::Warn => "33",     // yellow
            Style::Bad => "31",      // red
            Style::Dim => "2",       // dim
            Style::Value => "36",    // cyan
        }
    }
}

impl Palette {
    /// A palette with colour forced on or off.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Resolve the effective palette from the `--color` policy and whether
    /// stdout is a TTY. `always` forces on, `never` forces off, `auto` mirrors
    /// the TTY-ness (as git does).
    #[must_use]
    pub fn resolve(choice: crate::args::ColorChoice, stdout_is_tty: bool) -> Self {
        let enabled = match choice {
            crate::args::ColorChoice::Always => true,
            crate::args::ColorChoice::Never => false,
            crate::args::ColorChoice::Auto => stdout_is_tty,
        };
        Self { enabled }
    }

    /// Whether colour is on.
    #[must_use]
    pub fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Wrap `text` in this style's SGR escapes, or return it unchanged when
    /// colour is disabled.
    #[must_use]
    pub fn paint(self, style: Style, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{}m{text}\x1b[0m", style.sgr())
        } else {
            text.to_string()
        }
    }

    /// Render a `label: value` line with the label and value styled. The
    /// caller picks the value's style so a health value can be green/red.
    #[must_use]
    pub fn field(self, label: &str, value_style: Style, value: &str) -> String {
        format!(
            "  {}: {}",
            self.paint(Style::Label, label),
            self.paint(value_style, value)
        )
    }

    /// Render a section header line.
    #[must_use]
    pub fn header(self, text: &str) -> String {
        self.paint(Style::Header, text)
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;
    use crate::args::ColorChoice;

    #[test]
    fn never_emits_no_escapes() {
        let p = Palette::new(false);
        assert_eq!(p.paint(Style::Good, "ok"), "ok");
        assert!(!p.field("firmware", Style::Value, "uefi").contains('\x1b'));
        assert!(!p.header("Boot chain").contains('\x1b'));
    }

    #[test]
    fn always_wraps_in_sgr() {
        let p = Palette::new(true);
        let painted = p.paint(Style::Good, "ok");
        assert!(painted.starts_with("\x1b[32m"));
        assert!(painted.ends_with("\x1b[0m"));
        assert!(painted.contains("ok"));
    }

    #[test]
    fn resolve_follows_git_policy() {
        assert!(Palette::resolve(ColorChoice::Always, false).is_enabled());
        assert!(!Palette::resolve(ColorChoice::Never, true).is_enabled());
        assert!(Palette::resolve(ColorChoice::Auto, true).is_enabled());
        assert!(!Palette::resolve(ColorChoice::Auto, false).is_enabled());
    }

    #[test]
    fn field_contains_label_and_value() {
        let p = Palette::new(false);
        assert_eq!(p.field("loader", Style::Value, "grub"), "  loader: grub");
    }
}
