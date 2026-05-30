//! UKI (Unified Kernel Image) validator for the `efi-stub` boot target.
//!
//! A UKI is a PE/COFF executable (the systemd/efi stub) that the firmware
//! loads directly. `ukify` appends named PE *sections* carrying the payload:
//! `.linux` (the kernel image), `.initrd` (the initramfs), `.cmdline` (the
//! kernel command line, ASCII), `.osrel` (os-release text), and optionally
//! `.uname`/`.splash`/`.sbat`/… . In this repo the artifact is built by
//! `ukify build --linux=… --initrd=… --cmdline=… --os-release=@…` (see
//! `sirati-nmbl/lib/config.nix`, `system.build.nmblUki`) and installed at
//! `EFI/BOOT/BOOTX64.EFI` for the `loader = "efi-stub"` install path.
//!
//! [`validate_uki`] walks the PE section table, confirms the four required
//! sections are present and carry a plausible payload, and — when the caller
//! supplies the baked cmdline — checks the `.cmdline` section against it.
//! The cmdline expectation is a *caller-passed* parameter: the nix derivation
//! that wires this into `--validate-initrm` passes the value of `nmblBootConfig`
//! (`kernelParams ++ console=…`) in a later phase.
//!
//! ## Why a hand-rolled PE parser (no `object`/`goblin`)
//!
//! We deliberately do NOT pull in `object` or `goblin`. NMBL ships a tiny
//! static-musl PID-1 `/init`; those crates are heavyweight multi-format
//! parsers (ELF/Mach-O/COFF/archive, plus their own bounds/abstraction
//! layers) and would bloat the closure for what is, for a UKI, a fixed and
//! trivial walk: DOS header → PE header → COFF file header → section table.
//! The hand-rolled walk below is ~110 lines, has ZERO `unsafe`, and every
//! offset/length read is bounds-checked against the file length, so a
//! malformed artifact yields a finding rather than a panic. This is the
//! "avoid a heavy dependency in a minimal init" trade-off; the crates we
//! avoid are `object`/`goblin`.

use std::path::Path;

use crate::error::{NmblError, Result};

/// The four PE sections `ukify` always emits and that the efi-stub boot
/// target requires. Kept as the canonical require-list for [`validate_uki`].
const REQUIRED_SECTIONS: [&str; 4] = [".linux", ".initrd", ".cmdline", ".osrel"];

/// One problem found while validating a UKI. An empty `Vec<UkiFinding>` from
/// [`validate_uki`] means the artifact passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UkiFinding {
    pub kind: UkiFindingKind,
    pub detail: String,
}

/// The category of a [`UkiFinding`]. Stable string-ish discriminants so a
/// caller (e.g. the `--validate-initrm` reporter) can group/format them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UkiFindingKind {
    /// The PE container itself could not be parsed (truncated, bad magic,
    /// nonsensical section table). Inspection of sections is not possible.
    ParseError,
    /// A required section (`.linux`/`.initrd`/`.cmdline`/`.osrel`) is absent.
    MissingSection,
    /// A section is present but empty.
    EmptySection,
    /// A section's payload does not carry a magic this validator recognises
    /// (`.linux` not a kernel image, `.initrd` not a known archive format).
    BadMagic,
    /// The `.cmdline` section content did not match the caller's expectation.
    CmdlineMismatch,
}

impl UkiFinding {
    fn new(kind: UkiFindingKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// A section located in the PE section table, with its raw payload bytes.
struct Section<'a> {
    name: String,
    data: &'a [u8],
}

/// Validate the UKI at `path`.
///
/// `expected_cmdline`, when `Some`, is compared against the `.cmdline`
/// section content (trailing NUL/whitespace trimmed on both). When `None`
/// the cmdline check is skipped.
///
/// Returns the list of problems found (empty = valid). A read error
/// (missing/unreadable file) is returned as `Err`; a *parse* error that
/// prevents inspection is returned as a single [`UkiFindingKind::ParseError`]
/// finding so the caller can report it alongside other findings.
pub fn validate_uki(path: &Path, expected_cmdline: Option<&str>) -> Result<Vec<UkiFinding>> {
    // UKIs are tens of MB; reading the whole file into a Vec is acceptable
    // for a one-shot build-time validator and keeps the parser allocation-
    // free and bounds-checkable against a single slice.
    let bytes = std::fs::read(path).map_err(|source| NmblError::Io {
        source,
        context: format!("reading UKI {}", path.display()),
    })?;

    let sections = match parse_sections(&bytes) {
        Ok(s) => s,
        Err(detail) => {
            return Ok(vec![UkiFinding::new(UkiFindingKind::ParseError, detail)]);
        }
    };

    let mut findings = Vec::new();

    for required in REQUIRED_SECTIONS {
        if !sections.iter().any(|s| s.name == required) {
            findings.push(UkiFinding::new(
                UkiFindingKind::MissingSection,
                format!("required section `{required}` is missing"),
            ));
        }
    }

    if let Some(linux) = sections.iter().find(|s| s.name == ".linux") {
        check_payload(
            &mut findings,
            ".linux",
            linux.data,
            is_kernel_magic,
            "kernel image",
        );
    }
    if let Some(initrd) = sections.iter().find(|s| s.name == ".initrd") {
        check_payload(
            &mut findings,
            ".initrd",
            initrd.data,
            is_archive_magic,
            "known archive format (gzip/xz/zstd/lz4/cpio)",
        );
    }

    if let Some(expected) = expected_cmdline
        && let Some(cmdline) = sections.iter().find(|s| s.name == ".cmdline")
    {
        let actual = String::from_utf8_lossy(cmdline.data);
        let actual = actual.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        let expected = expected.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if actual != expected {
            findings.push(UkiFinding::new(
                UkiFindingKind::CmdlineMismatch,
                format!(".cmdline mismatch: expected `{expected}`, found `{actual}`"),
            ));
        }
    }

    Ok(findings)
}

/// Common per-section emptiness + magic check.
fn check_payload(
    findings: &mut Vec<UkiFinding>,
    name: &str,
    data: &[u8],
    magic_ok: fn(&[u8]) -> bool,
    expected_desc: &str,
) {
    if data.is_empty() {
        findings.push(UkiFinding::new(
            UkiFindingKind::EmptySection,
            format!("section `{name}` is empty"),
        ));
    } else if !magic_ok(data) {
        findings.push(UkiFinding::new(
            UkiFindingKind::BadMagic,
            format!("section `{name}` does not look like a {expected_desc}"),
        ));
    }
}

/// `.linux` should look like a kernel. Accept an x86 bzImage (the `HdrS`
/// magic at offset 0x202 of the payload), a PE (`MZ`), or an ELF (`\x7fELF`).
/// Lenient across arches — we only reject obviously-non-kernel payloads.
fn is_kernel_magic(data: &[u8]) -> bool {
    let mz = matches!(data.get(0..2), Some(b"MZ"));
    let elf = matches!(data.get(0..4), Some(b"\x7fELF"));
    let bzimage = matches!(data.get(0x202..0x206), Some(b"HdrS"));
    mz || elf || bzimage
}

/// `.initrd` should look like an archive. Accept gzip, xz, zstd, lz4, or a
/// raw newc/odc cpio (`"070701"`/`"070707"` etc. — all start `"07070"`).
fn is_archive_magic(data: &[u8]) -> bool {
    const GZIP: &[u8] = &[0x1f, 0x8b];
    const XZ: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z'];
    const ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
    const LZ4: &[u8] = &[0x04, 0x22, 0x4d, 0x18];
    const CPIO: &[u8] = b"07070";

    starts_with(data, GZIP)
        || starts_with(data, XZ)
        || starts_with(data, ZSTD)
        || starts_with(data, LZ4)
        || starts_with(data, CPIO)
}

fn starts_with(data: &[u8], prefix: &[u8]) -> bool {
    data.get(0..prefix.len()) == Some(prefix)
}

// ---------------------------------------------------------------------------
// PE/COFF section-table walk. Hand-rolled, zero `unsafe`, fully bounds-checked.
// ---------------------------------------------------------------------------

/// Read a little-endian u16 at `off`, bounds-checked.
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(off..off.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

/// Read a little-endian u32 at `off`, bounds-checked.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(off..off.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

/// Walk the PE section table and return every section with its raw payload.
/// On any structural problem returns `Err(detail)`; the caller turns that
/// into a single `ParseError` finding.
fn parse_sections(bytes: &[u8]) -> std::result::Result<Vec<Section<'_>>, String> {
    // DOS header: "MZ" at 0, e_lfanew (u32 LE) at 0x3C.
    if !starts_with(bytes, b"MZ") {
        return Err("not a PE image: missing `MZ` DOS magic".to_string());
    }
    let e_lfanew = read_u32(bytes, 0x3C).ok_or("truncated DOS header (no e_lfanew)")? as usize;

    // PE signature "PE\0\0" at e_lfanew.
    let pe_sig = bytes
        .get(e_lfanew..e_lfanew.checked_add(4).ok_or("e_lfanew overflow")?)
        .ok_or("e_lfanew points past end of file")?;
    if pe_sig != b"PE\0\0" {
        return Err("missing `PE\\0\\0` signature at e_lfanew".to_string());
    }

    // COFF File Header immediately follows the 4-byte signature.
    // NumberOfSections: u16 at coff+2; SizeOfOptionalHeader: u16 at coff+16.
    let coff = e_lfanew.checked_add(4).ok_or("PE header offset overflow")?;
    let num_sections = read_u16(bytes, coff.checked_add(2).ok_or("coff overflow")?)
        .ok_or("truncated COFF header (NumberOfSections)")? as usize;
    let opt_hdr_size = read_u16(bytes, coff.checked_add(16).ok_or("coff overflow")?)
        .ok_or("truncated COFF header (SizeOfOptionalHeader)")? as usize;

    // The COFF File Header is 20 bytes; the optional header follows; then the
    // section table. Each section header entry is 40 bytes.
    let sec_table = coff
        .checked_add(20)
        .and_then(|v| v.checked_add(opt_hdr_size))
        .ok_or("section-table offset overflow")?;

    if num_sections == 0 {
        return Err("PE has zero sections".to_string());
    }
    // Guard against an absurd NumberOfSections (corrupt header) before we
    // start indexing 40 bytes per entry.
    let table_bytes = num_sections
        .checked_mul(40)
        .ok_or("section count overflow")?;
    let _ = bytes
        .get(
            sec_table
                ..sec_table
                    .checked_add(table_bytes)
                    .ok_or("section-table overflow")?,
        )
        .ok_or("section table extends past end of file")?;

    let mut sections = Vec::with_capacity(num_sections);
    for i in 0..num_sections {
        let entry = sec_table
            .checked_add(i.checked_mul(40).ok_or("entry offset overflow")?)
            .ok_or("entry offset overflow")?;

        // Name[8]: NUL-padded inline ASCII. ukify section names are short
        // (`.linux`, `.initrd`, …) so they always fit inline; a `/offset`
        // long-name reference into the string table is never used here, so
        // we read the 8 raw bytes and treat a leading `/` name as opaque
        // (it simply won't match any required name) rather than resolving it.
        let name_raw = bytes
            .get(entry..entry.checked_add(8).ok_or("name overflow")?)
            .ok_or("section name extends past end of file")?;
        let name = name_to_string(name_raw);

        let size_of_raw = read_u32(bytes, entry.checked_add(16).ok_or("entry overflow")?)
            .ok_or("truncated section header (SizeOfRawData)")? as usize;
        let ptr_to_raw = read_u32(bytes, entry.checked_add(20).ok_or("entry overflow")?)
            .ok_or("truncated section header (PointerToRawData)")?
            as usize;

        // A section may legitimately have zero raw data (e.g. .bss); for a
        // UKI payload section that means "empty", which the magic/empty
        // check downstream reports. Read the payload defensively.
        let data: &[u8] = if size_of_raw == 0 {
            &[]
        } else {
            let end = ptr_to_raw
                .checked_add(size_of_raw)
                .ok_or("section payload range overflow")?;
            bytes
                .get(ptr_to_raw..end)
                .ok_or_else(|| format!("section `{name}` payload extends past end of file"))?
        };

        sections.push(Section { name, data });
    }

    Ok(sections)
}

/// Decode an 8-byte inline section name: bytes up to the first NUL, as UTF-8
/// (lossy; PE names are ASCII). PE permits either NUL- or SPACE-padding for
/// names shorter than 8 bytes, so trailing spaces are trimmed too — without
/// this a valid space-padded `.linux  ` would be read as `.linux  ` and
/// false-flagged as a missing `.linux` section.
fn name_to_string(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let slice = raw.get(..end).unwrap_or(raw);
    String::from_utf8_lossy(slice)
        .trim_end_matches(' ')
        .to_owned()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests build fixtures and assert on contract failures"
)]
mod tests;
