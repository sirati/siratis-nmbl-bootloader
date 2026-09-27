//! LUKS / volume-unlock handover decoder.
//!
//! When NMBL unlocks a volume it can pass the unlock secret to the target's
//! stage 1 as a keyfile appended to the initrd (the `pass_to_stage1` path):
//! a passphrase the operator typed, or a key the TPM unsealed. This module
//! turns a [`crate::activation::KeyInjection`] (path + secret bytes) into a
//! human-readable [`KeyHandover`] for a status/harness view: WHICH volume,
//! WHICH method, the key's length and format — with the key value MASKED by
//! default and an explicit reveal for scenario test keys.
//!
//! Secrets are never printed unless the caller passes `reveal = true`. Even the
//! revealed form goes through [`mask_key`]'s hex/utf8 rendering so a binary key
//! is shown as hex rather than dumped raw into a terminal.

/// The mechanism by which NMBL obtained the unlock secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMethod {
    /// A passphrase the operator typed into the TUI modal.
    Passphrase,
    /// A key the TPM unsealed via the systemd-tpm2 token.
    TpmUnsealed,
    /// A keyfile bundled into the NMBL initramfs at build time.
    Keyfile,
    /// Method could not be inferred from the injection path alone.
    Unknown,
}

impl KeyMethod {
    /// A short label for the method.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            KeyMethod::Passphrase => "passphrase (operator-entered)",
            KeyMethod::TpmUnsealed => "TPM-unsealed",
            KeyMethod::Keyfile => "bundled keyfile",
            KeyMethod::Unknown => "unknown",
        }
    }
}

/// A described volume-unlock handover, safe to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyHandover {
    /// The in-initrd target path the keyfile is written to
    /// (e.g. `/etc/nmbl-luks/cryptroot`).
    pub target_path: String,
    /// The volume name inferred from the target path's basename.
    pub volume: String,
    /// How NMBL obtained the secret (best-effort inference; see notes).
    pub method: KeyMethod,
    /// The secret length in bytes.
    pub key_len: usize,
    /// The key's coarse format: `"utf8 text"` (a typed passphrase) or
    /// `"binary"` (an unsealed/random key).
    pub key_format: &'static str,
    /// The masked rendering (always safe to print), e.g. `••••••••` with the
    /// length. Populated regardless of `reveal`.
    pub masked: String,
    /// The revealed rendering, `Some` only when the caller asked to reveal.
    /// Binary keys are shown as hex; text keys as their UTF-8 value.
    pub revealed: Option<String>,
}

/// Mask a secret for display: never shows the value, only its length and a
/// fixed run of bullets so a status line reveals nothing about the key.
#[must_use]
pub fn mask_key(secret: &[u8]) -> String {
    format!("•••••••• ({} bytes, hidden)", secret.len())
}

/// The revealed rendering of a secret: hex for binary, the UTF-8 string for
/// text. Only ever called on the explicit reveal path.
fn reveal_key(secret: &[u8]) -> String {
    if is_text(secret) {
        // A typed passphrase: show it verbatim (scenario/test use only).
        String::from_utf8_lossy(secret).into_owned()
    } else {
        // Binary key: hex, so it is copy-pasteable and terminal-safe.
        let mut s = String::with_capacity(secret.len() * 2);
        for b in secret {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

fn is_text(secret: &[u8]) -> bool {
    !secret.is_empty()
        && std::str::from_utf8(secret).is_ok()
        && secret
            .iter()
            .all(|&b| b == b'\n' || b == b'\t' || (0x20..=0x7e).contains(&b))
}

/// Describe a key injection for display. `reveal` opts into showing the value
/// (scenario test keys only). `method` is provided by the caller when known
/// (the activation records it); pass [`KeyMethod::Unknown`] to infer nothing.
#[must_use]
pub fn describe_key_injection(
    target_path: &str,
    secret: &[u8],
    method: KeyMethod,
    reveal: bool,
) -> KeyHandover {
    let volume = target_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(target_path)
        .to_string();
    let key_format = if is_text(secret) {
        "utf8 text"
    } else {
        "binary"
    };
    KeyHandover {
        target_path: target_path.to_string(),
        volume,
        method,
        key_len: secret.len(),
        key_format,
        masked: mask_key(secret),
        revealed: reveal.then(|| reveal_key(secret)),
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn masks_by_default_and_names_the_volume() {
        let h = describe_key_injection(
            "/etc/nmbl-luks/cryptroot",
            b"hunter2",
            KeyMethod::Passphrase,
            false,
        );
        assert_eq!(h.volume, "cryptroot");
        assert_eq!(h.key_len, 7);
        assert_eq!(h.key_format, "utf8 text");
        assert!(h.revealed.is_none(), "must not reveal without opt-in");
        assert!(!h.masked.contains("hunter2"), "masked must hide the value");
        assert!(h.masked.contains("7 bytes"));
    }

    #[test]
    fn reveals_text_verbatim_when_asked() {
        let h = describe_key_injection(
            "/etc/nmbl-luks/root",
            b"hunter2",
            KeyMethod::Passphrase,
            true,
        );
        assert_eq!(h.revealed.as_deref(), Some("hunter2"));
    }

    #[test]
    fn reveals_binary_as_hex() {
        let secret = [0xde, 0xad, 0xbe, 0xef];
        let h =
            describe_key_injection("/etc/nmbl-luks/data", &secret, KeyMethod::TpmUnsealed, true);
        assert_eq!(h.key_format, "binary");
        assert_eq!(h.revealed.as_deref(), Some("deadbeef"));
        assert_eq!(h.method, KeyMethod::TpmUnsealed);
    }

    #[test]
    fn mask_never_contains_secret_bytes() {
        let secret = b"topsecretkey";
        let masked = mask_key(secret);
        assert!(!masked.contains("topsecret"));
        assert!(masked.contains("12 bytes"));
    }
}
