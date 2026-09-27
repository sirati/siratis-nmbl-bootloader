//! Render what NMBL handed to the kexec'd kernel, using the shared decoders in
//! `nmbl_init::handover` (the same ones `nmblctl status` uses).

use nmbl_init::handover::{
    CpioEntryKind, KeyMethod, LogLevel, decode_cpio_fragment, decode_log_buffer,
    describe_key_injection, parse_cmdline,
};

use crate::sim::KexecLoad;

/// The newc fragment NMBL appends starts at the LAST `070701` archive whose
/// entries are NMBL's (the system initrd is compressed, so its cpio magic is
/// not visible). Find the first uncompressed newc header that begins a run
/// of NMBL entries.
fn nmbl_fragment(initrd: &[u8]) -> Option<&[u8]> {
    let magic = b"070701";
    let mut start = None;
    let mut i = 0;
    while let Some(off) = initrd
        .get(i..)?
        .windows(magic.len())
        .position(|w| w == magic)
    {
        let at = i + off;
        let entries = decode_cpio_fragment(initrd.get(at..)?);
        if entries
            .iter()
            .any(|e| e.name == "nmbl-log/nmbl.log" || e.name.starts_with("etc/nmbl-luks"))
        {
            start = Some(at);
            break;
        }
        i = at + 1;
    }
    initrd.get(start?..)
}

pub struct Report {
    pub text: String,
    /// Structured facts the automated tests assert on.
    pub facts: Facts,
}

#[derive(Debug, Default, Clone)]
pub struct Facts {
    pub cmdline: String,
    pub init: Option<String>,
    pub rollback_marker: bool,
    pub initrd_len: usize,
    pub kernel_len: usize,
    pub kernel_path: Option<String>,
    pub appended_files: Vec<(String, usize)>,
    pub keyfiles: Vec<(String, usize)>,
    pub log_lines: usize,
}

fn c(on: bool, sgr: &str, s: &str) -> String {
    if on {
        format!("\x1b[{sgr}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn render(k: &KexecLoad, reveal: bool, color: bool) -> Report {
    let mut f = Facts {
        cmdline: k.cmdline.clone(),
        kernel_len: k.kernel.len(),
        kernel_path: k.kernel_path.clone(),
        initrd_len: k.initrd.as_ref().map_or(0, Vec::len),
        ..Facts::default()
    };
    let mut o = String::new();
    let h = |o: &mut String, s: &str| {
        o.push('\n');
        o.push_str(&c(color, "1;36", s));
        o.push('\n');
    };
    o.push_str(&c(color, "1;32", "═══ NMBL kexec handover ═══"));
    o.push('\n');

    h(&mut o, "Kernel");
    o.push_str(&format!(
        "  image: {} bytes{}\n",
        k.kernel.len(),
        k.kernel_path
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default()
    ));

    h(&mut o, "Kernel cmdline");
    let parsed = parse_cmdline(&k.cmdline);
    f.rollback_marker = parsed.has_rollback_marker();
    f.init = parsed
        .params
        .iter()
        .find(|p| p.key == "init")
        .and_then(|p| p.value.clone());
    o.push_str("  set by NMBL:\n");
    for p in parsed.nmbl_params() {
        o.push_str(&format!("    {}\n", c(color, "33", &p.raw)));
    }
    o.push_str("  from the generation:\n");
    for p in parsed.generation_params() {
        o.push_str(&format!("    {}\n", p.raw));
    }

    h(&mut o, "Initrd");
    match &k.initrd {
        None => o.push_str("  none (KEXEC_FILE_NO_INITRAMFS)\n"),
        Some(initrd) => {
            o.push_str(&format!("  total: {} bytes\n", initrd.len()));
            match nmbl_fragment(initrd) {
                None => o.push_str("  no NMBL cpio fragment found\n"),
                Some(frag) => {
                    o.push_str("  files NMBL appended:\n");
                    for e in decode_cpio_fragment(frag) {
                        if e.kind != CpioEntryKind::File {
                            continue;
                        }
                        let hint = e.content_hint.unwrap_or("?");
                        o.push_str(&format!("    /{}  {} bytes  [{hint}]\n", e.name, e.size));
                        f.appended_files.push((format!("/{}", e.name), e.size));
                        let body = frag
                            .get(e.data_offset..e.data_offset + e.size)
                            .unwrap_or_default();
                        if e.name.starts_with("etc/nmbl-luks/") {
                            let d = describe_key_injection(
                                &format!("/{}", e.name),
                                body,
                                KeyMethod::Unknown,
                                reveal,
                            );
                            f.keyfiles.push((d.volume.clone(), d.key_len));
                            o.push_str(&format!(
                                "      LUKS unlock handover: volume {}, {} key, {}\n",
                                d.volume, d.key_format, d.masked
                            ));
                            o.push_str(
                                "      method: passphrase or TPM-unsealed token (both hand the \
                                 secret to stage 1 as this keyfile)\n",
                            );
                            if let Some(v) = d.revealed {
                                o.push_str(&format!(
                                    "      {} {}\n",
                                    c(color, "1;31", "REVEALED (scenario test key):"),
                                    v
                                ));
                            }
                        } else if e.name == "nmbl-log/nmbl.log" {
                            let text = String::from_utf8_lossy(body);
                            let lines = decode_log_buffer(&text);
                            f.log_lines = lines.len();
                            o.push_str(&format!("      boot log: {} lines\n", lines.len()));
                            for l in &lines {
                                let (sgr, tag) = match l.level {
                                    LogLevel::Warn => ("33", "WARN"),
                                    LogLevel::Meta => ("2", "META"),
                                    LogLevel::Info => ("0", "INFO"),
                                };
                                o.push_str(&format!(
                                    "        {} {}\n",
                                    c(color, sgr, tag),
                                    l.message
                                ));
                            }
                        } else {
                            o.push_str(&format!("      {}\n", hex_strings(body)));
                        }
                    }
                }
            }
        }
    }
    if !reveal && !f.keyfiles.is_empty() {
        o.push_str(
            "\n  (key values hidden; rerun with --reveal-keys to show scenario test keys)\n",
        );
    }
    Report { text: o, facts: f }
}

/// Fallback view for unknown blobs: first bytes as hex plus printable strings.
fn hex_strings(b: &[u8]) -> String {
    let hex: String = b.iter().take(32).map(|x| format!("{x:02x}")).collect();
    let strings: String = b
        .iter()
        .take(256)
        .map(|&x| {
            if (0x20..0x7f).contains(&x) {
                x as char
            } else {
                '.'
            }
        })
        .collect();
    format!("hex {hex}  strings {strings:?}")
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, reason = "tests assert")]
mod tests {
    use super::*;
    use nmbl_init::sys::cpio::{InjectionEntry, build_fragment};
    use std::path::Path;

    #[test]
    fn decodes_a_keyfile_and_log_after_a_system_initrd() {
        let frag = build_fragment(&[
            InjectionEntry {
                path: Path::new("/etc/nmbl-luks/cryptroot"),
                content: b"test-pass",
            },
            InjectionEntry {
                path: Path::new("/nmbl-log/nmbl.log"),
                content: b"phase 1\nkexec failed? no\n",
            },
        ]);
        // A fake compressed system initrd in front, 4-byte aligned like NMBL's.
        let mut initrd = b"\x1f\x8b\x08\x00compressed....".to_vec();
        while !initrd.len().is_multiple_of(4) {
            initrd.push(0);
        }
        initrd.extend_from_slice(&frag);
        let k = KexecLoad {
            kernel: vec![0; 10],
            kernel_path: None,
            initrd: Some(initrd),
            cmdline: "console=ttyS0 init=/nix/var/nix/profiles/system-3-link/init".into(),
        };
        let r = render(&k, false, false);
        assert_eq!(r.facts.keyfiles, vec![("cryptroot".to_string(), 9)]);
        assert_eq!(r.facts.log_lines, 2);
        assert!(!r.text.contains("test-pass"), "key must be masked");
        let revealed = render(&k, true, false);
        assert!(revealed.text.contains("test-pass"));
        assert_eq!(
            r.facts.init.as_deref(),
            Some("/nix/var/nix/profiles/system-3-link/init")
        );
    }
}
