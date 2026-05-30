use super::*;
use std::io::Write as _;

/// Build a minimal valid PE byte buffer with the given named sections.
/// Each section's payload is written contiguously after the section
/// table. Layout: DOS stub (PE sig pointer at 0x3C) → PE sig → COFF
/// header (no optional header) → section table → payloads.
fn build_pe(sections: &[(&str, &[u8])]) -> Vec<u8> {
    let e_lfanew: u32 = 0x40;
    let mut buf = vec![0u8; e_lfanew as usize];
    buf[0] = b'M';
    buf[1] = b'Z';
    buf[0x3C..0x40].copy_from_slice(&e_lfanew.to_le_bytes());

    // PE signature.
    buf.extend_from_slice(b"PE\0\0");
    // COFF File Header (20 bytes). Machine(2), NumberOfSections(2),
    // TimeDateStamp(4), PointerToSymbolTable(4), NumberOfSymbols(4),
    // SizeOfOptionalHeader(2), Characteristics(2).
    let coff_start = buf.len();
    let mut coff = vec![0u8; 20];
    coff[0..2].copy_from_slice(&0x8664u16.to_le_bytes()); // x86_64
    coff[2..4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
    coff[16..18].copy_from_slice(&0u16.to_le_bytes()); // no optional header
    buf.extend_from_slice(&coff);
    let _ = coff_start;

    // Section table starts here; payloads go after the whole table.
    let sec_table_off = buf.len();
    let payloads_off = sec_table_off + sections.len() * 40;
    let mut payload_cursor = payloads_off;
    let mut payload_blob: Vec<u8> = Vec::new();

    for (name, data) in sections {
        let mut entry = vec![0u8; 40];
        let name_bytes = name.as_bytes();
        let n = name_bytes.len().min(8);
        entry[0..n].copy_from_slice(&name_bytes[..n]);
        // VirtualSize(8..12), VirtualAddress(12..16),
        // SizeOfRawData(16..20), PointerToRawData(20..24).
        entry[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&(payload_cursor as u32).to_le_bytes());
        entry[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        entry[20..24].copy_from_slice(&(payload_cursor as u32).to_le_bytes());
        buf.extend_from_slice(&entry);

        payload_blob.extend_from_slice(data);
        payload_cursor += data.len();
    }
    buf.extend_from_slice(&payload_blob);
    buf
}

/// A `.linux` payload that carries the bzImage `HdrS` magic at 0x202.
fn bzimage_payload() -> Vec<u8> {
    let mut v = vec![0u8; 0x210];
    v[0x202..0x206].copy_from_slice(b"HdrS");
    v
}

/// A `.initrd` payload that carries the gzip magic.
fn gzip_payload() -> Vec<u8> {
    let mut v = vec![0x1f, 0x8b, 0x08, 0x00];
    v.extend_from_slice(&[0u8; 16]);
    v
}

fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("tmp");
    f.write_all(bytes).expect("write");
    f.flush().expect("flush");
    f
}

#[test]
fn well_formed_uki_with_matching_cmdline_has_no_findings() {
    let linux = bzimage_payload();
    let initrd = gzip_payload();
    let pe = build_pe(&[
        (".linux", &linux),
        (".initrd", &initrd),
        (".cmdline", b"root=/dev/sda1 quiet"),
        (".osrel", b"NAME=NMBL\n"),
    ]);
    let f = write_tmp(&pe);
    let findings = validate_uki(f.path(), Some("root=/dev/sda1 quiet")).expect("validate ok");
    assert!(
        findings.is_empty(),
        "expected no findings, got {findings:?}"
    );
}

#[test]
fn cmdline_trailing_nul_and_whitespace_is_trimmed() {
    let linux = bzimage_payload();
    let initrd = gzip_payload();
    let pe = build_pe(&[
        (".linux", &linux),
        (".initrd", &initrd),
        (".cmdline", b"root=/dev/sda1\n\0\0"),
        (".osrel", b"NAME=NMBL\n"),
    ]);
    let f = write_tmp(&pe);
    let findings = validate_uki(f.path(), Some("root=/dev/sda1")).expect("validate ok");
    assert!(findings.is_empty(), "trim mismatch: {findings:?}");
}

#[test]
fn missing_initrd_section_is_a_finding() {
    let linux = bzimage_payload();
    let pe = build_pe(&[
        (".linux", &linux),
        (".cmdline", b"quiet"),
        (".osrel", b"NAME=NMBL\n"),
    ]);
    let f = write_tmp(&pe);
    let findings = validate_uki(f.path(), None).expect("validate ok");
    assert!(
        findings
            .iter()
            .any(|x| x.kind == UkiFindingKind::MissingSection && x.detail.contains(".initrd")),
        "expected MissingSection .initrd, got {findings:?}"
    );
}

#[test]
fn bad_magic_linux_is_a_finding() {
    let initrd = gzip_payload();
    let pe = build_pe(&[
        (
            ".linux",
            b"not a kernel at all, just text bytes here padding",
        ),
        (".initrd", &initrd),
        (".cmdline", b"quiet"),
        (".osrel", b"NAME=NMBL\n"),
    ]);
    let f = write_tmp(&pe);
    let findings = validate_uki(f.path(), None).expect("validate ok");
    assert!(
        findings
            .iter()
            .any(|x| x.kind == UkiFindingKind::BadMagic && x.detail.contains(".linux")),
        "expected BadMagic .linux, got {findings:?}"
    );
}

#[test]
fn empty_linux_section_is_a_finding() {
    let initrd = gzip_payload();
    let pe = build_pe(&[
        (".linux", b""),
        (".initrd", &initrd),
        (".cmdline", b"quiet"),
        (".osrel", b"NAME=NMBL\n"),
    ]);
    let f = write_tmp(&pe);
    let findings = validate_uki(f.path(), None).expect("validate ok");
    assert!(
        findings
            .iter()
            .any(|x| x.kind == UkiFindingKind::EmptySection && x.detail.contains(".linux")),
        "expected EmptySection .linux, got {findings:?}"
    );
}

#[test]
fn cmdline_mismatch_is_a_finding_with_both_values() {
    let linux = bzimage_payload();
    let initrd = gzip_payload();
    let pe = build_pe(&[
        (".linux", &linux),
        (".initrd", &initrd),
        (".cmdline", b"root=/dev/sda1 ro"),
        (".osrel", b"NAME=NMBL\n"),
    ]);
    let f = write_tmp(&pe);
    let findings = validate_uki(f.path(), Some("root=/dev/nvme0n1p2 rw")).expect("validate ok");
    let m = findings
        .iter()
        .find(|x| x.kind == UkiFindingKind::CmdlineMismatch)
        .expect("expected a CmdlineMismatch finding");
    assert!(
        m.detail.contains("root=/dev/sda1 ro"),
        "found-value: {}",
        m.detail
    );
    assert!(
        m.detail.contains("root=/dev/nvme0n1p2 rw"),
        "expected-value: {}",
        m.detail
    );
}

#[test]
fn initrd_accepts_xz_zstd_and_cpio() {
    for magic in [
        &[0xfd, b'7', b'z', b'X', b'Z'][..],
        &[0x28, 0xb5, 0x2f, 0xfd][..],
        b"070701".as_slice(),
    ] {
        assert!(is_archive_magic(magic), "should accept {magic:?}");
    }
    assert!(!is_archive_magic(b"plain text"), "should reject plain text");
}

#[test]
fn truncated_file_is_a_graceful_parse_finding_not_a_panic() {
    // A buffer with the MZ magic but nothing else — every downstream
    // read must bounds-check and surface a ParseError finding.
    let f = write_tmp(b"MZ");
    let findings = validate_uki(f.path(), None).expect("validate ok");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].kind, UkiFindingKind::ParseError);
}

#[test]
fn garbage_file_is_a_graceful_parse_finding() {
    let f = write_tmp(b"this is definitely not a PE image at all, just random bytes");
    let findings = validate_uki(f.path(), None).expect("validate ok");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].kind, UkiFindingKind::ParseError);
}

#[test]
fn nonexistent_file_is_an_err() {
    let res = validate_uki(Path::new("/nonexistent/uki/path.efi"), None);
    assert!(res.is_err(), "missing file must be Err, got {res:?}");
}

#[test]
fn absurd_section_count_does_not_panic() {
    // Hand-craft a PE claiming 0xFFFF sections with a tiny file — the
    // section-table bounds check must reject it as a ParseError.
    let e_lfanew: u32 = 0x40;
    let mut buf = vec![0u8; e_lfanew as usize];
    buf[0] = b'M';
    buf[1] = b'Z';
    buf[0x3C..0x40].copy_from_slice(&e_lfanew.to_le_bytes());
    buf.extend_from_slice(b"PE\0\0");
    let mut coff = vec![0u8; 20];
    coff[2..4].copy_from_slice(&0xFFFFu16.to_le_bytes());
    buf.extend_from_slice(&coff);
    let f = write_tmp(&buf);
    let findings = validate_uki(f.path(), None).expect("validate ok");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].kind, UkiFindingKind::ParseError);
}
