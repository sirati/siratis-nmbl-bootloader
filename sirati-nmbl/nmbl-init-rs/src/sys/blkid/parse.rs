//! blkid invocation and KEY=VALUE output parsing.

use std::collections::HashMap;
use std::path::Path;

use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::activation::{ProcessOutcome, run_capture};
use crate::sys::poller::LocalSender;

use super::{BLKID_BINARY, BLKID_EXIT_NO_SUPERBLOCK};

/// Argv for `blkid -p -o export <dev>`.
///
/// `-p` selects LOW-LEVEL probing (bypass the blkid cache). NMBL has no
/// udev, so the cache-backed default mode returns only superblock
/// attributes (LABEL/UUID/TYPE) and omits partition-table attributes
/// (PARTLABEL/PARTUUID) — which a disko-style `device =
/// /dev/disk/by-partlabel/...` LUKS/activation entry depends on. The
/// low-level mode reads the GPT directly, but emits partition info under
/// `PART_ENTRY_*` keys; `parse_blkid_export` aliases those back to the
/// cache-mode `PARTLABEL`/`PARTUUID` names the symlink layer expects.
fn blkid_argv(dev: &Path) -> Vec<String> {
    vec![
        "-p".to_string(),
        "-o".to_string(),
        "export".to_string(),
        dev.display().to_string(),
    ]
}

/// Run `blkid -p -o export <dev>` and parse the result, reaping the
/// child asynchronously via the poller. Exit code 2 is remapped to
/// "empty attributes" — see [`BLKID_EXIT_NO_SUPERBLOCK`].
pub(super) async fn blkid_for(dev: &Path, sender: &LocalSender) -> Result<HashMap<String, String>> {
    let (outcome, captured) =
        run_capture(Path::new(BLKID_BINARY), &blkid_argv(dev), sender).await?;
    Ok(interpret_blkid(dev, outcome, &captured))
}

/// Map a blkid `(outcome, stdout)` into parsed attributes, applying the
/// exit-code policy: signal death, exit 2 (no superblock), or any other
/// non-zero exit all collapse to "empty attributes".
fn interpret_blkid(
    dev: &Path,
    outcome: ProcessOutcome,
    captured: &[u8],
) -> HashMap<String, String> {
    if !outcome.normal_exit {
        nmbl_warn!(
            "blkid: {} killed by signal (exit_code={}); treating as empty",
            dev.display(),
            outcome.exit_code,
        );
        return HashMap::new();
    }

    match outcome.exit_code {
        0 => {}
        BLKID_EXIT_NO_SUPERBLOCK => return HashMap::new(),
        other => {
            nmbl_warn!(
                "blkid: {} exited {} (not 0/2); treating as empty",
                dev.display(),
                other,
            );
            return HashMap::new();
        }
    }

    let text = match std::str::from_utf8(captured) {
        Ok(s) => s,
        Err(e) => {
            nmbl_warn!(
                "blkid: {} produced non-UTF8 stdout ({}); skipping",
                dev.display(),
                e,
            );
            return HashMap::new();
        }
    };

    parse_blkid_export(text)
}

/// Parse one `blkid -o export` payload into a `HashMap`.
///
/// Format (per `blkid(8)` OUTPUT FORMAT, "export" mode): one
/// `KEY=VALUE` per line, blank lines separate device records (we
/// always call with a single device, so we just merge keys). VALUEs
/// are unquoted. Lines without `=` are ignored. Whitespace is
/// trimmed off the KEY side; the VALUE is taken verbatim except for
/// trailing CR / LF.
pub fn parse_blkid_export(text: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some(eq_idx) = line.find('=') else {
            continue;
        };
        // `split_at` is total here because `eq_idx` came from `find`.
        let (key, value_with_eq) = line.split_at(eq_idx);
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        // Strip the leading '='. `value_with_eq` starts with '=' so
        // `get(1..)` is total too; `unwrap_or_default` collapses the
        // would-never-fire None branch into an empty `&str`.
        let value = value_with_eq.get(1..).unwrap_or_default();
        out.insert(key.to_string(), value.to_string());
    }

    // Low-level (`blkid -p`) probing emits partition-table attributes
    // under `PART_ENTRY_*`; alias them to the cache-mode names the
    // symlink layer keys on, without clobbering any cache-mode value
    // that was already present. PART_ENTRY_NAME is the GPT partition
    // name (== PARTLABEL); PART_ENTRY_UUID is the partition GUID
    // (== PARTUUID).
    for (low_level, cache_mode) in [
        ("PART_ENTRY_NAME", "PARTLABEL"),
        ("PART_ENTRY_UUID", "PARTUUID"),
    ] {
        if !out.contains_key(cache_mode)
            && let Some(value) = out.get(low_level).cloned()
        {
            out.insert(cache_mode.to_string(), value);
        }
    }

    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_blkid_output() {
        let text = "\
DEVNAME=/dev/sda1
LABEL=boot
UUID=1234-ABCD
TYPE=vfat
PARTLABEL=disk-main-ESP
PARTUUID=abcdef01-1234-5678-9abc-def012345678
";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("LABEL"), Some(&"boot".to_string()));
        assert_eq!(map.get("UUID"), Some(&"1234-ABCD".to_string()));
        assert_eq!(map.get("PARTLABEL"), Some(&"disk-main-ESP".to_string()));
        assert_eq!(
            map.get("PARTUUID"),
            Some(&"abcdef01-1234-5678-9abc-def012345678".to_string()),
        );
        assert_eq!(map.get("DEVNAME"), Some(&"/dev/sda1".to_string()));
        assert_eq!(map.get("TYPE"), Some(&"vfat".to_string()));
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn parse_low_level_part_entry_aliases_to_cache_names() {
        // `blkid -p -o export` on a GPT partition emits partition-table
        // attributes under PART_ENTRY_*; we alias them to PARTLABEL /
        // PARTUUID so the symlink layer (which keys on the cache-mode
        // names) creates by-partlabel / by-partuuid links in the
        // udev-less environment.
        let text = "\
DEVNAME=/dev/vda2
TYPE=crypto_LUKS
PART_ENTRY_NAME=disk-main-luks
PART_ENTRY_UUID=11111111-2222-3333-4444-555555555555
PART_ENTRY_TYPE=0fc63daf-8483-4772-8e79-3d69d8477de4
";
        let map = parse_blkid_export(text);
        assert_eq!(
            map.get("PARTLABEL"),
            Some(&"disk-main-luks".to_string()),
            "PART_ENTRY_NAME must alias to PARTLABEL",
        );
        assert_eq!(
            map.get("PARTUUID"),
            Some(&"11111111-2222-3333-4444-555555555555".to_string()),
            "PART_ENTRY_UUID must alias to PARTUUID",
        );
        // The crypto_LUKS TYPE is still surfaced unchanged.
        assert_eq!(map.get("TYPE"), Some(&"crypto_LUKS".to_string()));
    }

    #[test]
    fn parse_existing_cache_names_not_clobbered_by_alias() {
        // If both cache-mode and low-level names are present (mixed
        // output), the cache-mode value wins and is not overwritten.
        let text = "PARTLABEL=keep-me\nPART_ENTRY_NAME=ignore-me\n";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("PARTLABEL"), Some(&"keep-me".to_string()));
    }

    #[test]
    fn parse_empty_input_returns_empty_map() {
        assert!(parse_blkid_export("").is_empty());
    }

    #[test]
    fn parse_skips_blank_lines_and_lines_without_eq() {
        let text = "\n\nUUID=abc\n\njust-a-comment-line\nLABEL=root\n\n";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("UUID"), Some(&"abc".to_string()));
        assert_eq!(map.get("LABEL"), Some(&"root".to_string()));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_handles_crlf_line_endings() {
        let text = "UUID=abc\r\nLABEL=root\r\n";
        let map = parse_blkid_export(text);
        // CR is stripped from the end of the value.
        assert_eq!(map.get("UUID"), Some(&"abc".to_string()));
        assert_eq!(map.get("LABEL"), Some(&"root".to_string()));
    }

    #[test]
    fn parse_value_with_embedded_equals_keeps_full_value() {
        // blkid never emits such values for the keys we care about,
        // but the parser shouldn't truncate at the second '='.
        let text = "WEIRD=foo=bar=baz";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("WEIRD"), Some(&"foo=bar=baz".to_string()));
    }

    #[test]
    fn parse_value_with_trailing_whitespace_kept() {
        // We do NOT trim values — blkid's export format never quotes
        // them, so trailing whitespace would be meaningful.
        let text = "LABEL=root \n";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("LABEL"), Some(&"root ".to_string()));
    }
}
