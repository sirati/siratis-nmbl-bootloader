//! Pre-kexec handoff staging: build the cmdline, stage the NMBL log
//! transcript into the next kernel's initramfs, assemble the cpio
//! fragment, and (in F4) verify the generation + measure it into PCR-11
//! before loading the image via `kexec_file_load(2)`. Split out of
//! `boot/mod.rs` so the verify/measure insertion points live next to the
//! staging they gate. Load MUST happen before any unmount —
//! [`sys::kexec::load_with_extra_initrd_cpio`] reads kernel+initrd from
//! the still-mounted `/mnt/system`.

use std::path::Path;

use crate::activation::KeyInjection;
use crate::config::Config;
use crate::error::Result;
use crate::generations::Generation;
use crate::log;
use crate::sys;
use crate::sys::cpio::{InjectionEntry, build_fragment};
use crate::{nmbl_info, nmbl_warn};

// Tmpfs path the NMBL byte-ring is flushed to before kexec — recreated
// in the next kernel's initramfs by the cpio fragment we splice into
// `kexec_file_load(2)` below, so a stage-1 helper (e.g. `nmbl-log-import`)
// can pick the transcript up. Single source of truth in `log`.
use crate::log::NMBL_LOG_PATH;

/// Final cmdline.
///
/// * `cmdline_override` (TUI editor path) wins verbatim — an operator who has
///   hand-edited the line must not have their text silently mutated. No
///   `init=` injection happens in this branch.
/// * Otherwise the generation's own `kernel_params` are space-joined, and
///   `init=<stage2>` is appended unless the joined string already carries an
///   `init=` token (split on whitespace). The init value is the generation's
///   `init_path` stripped of `system_root`, with a leading `/` re-prepended so
///   the chained kernel — which mounts the store at `/`, not under our
///   `/mnt/system` prefix — sees a path that exists in its own namespace. If
///   `init_path` is somehow outside `system_root`, fall back to the raw path
///   with a warning rather than producing a broken cmdline.
fn build_cmdline(
    generation: &Generation,
    cmdline_override: Option<&str>,
    system_root: &Path,
) -> String {
    if let Some(s) = cmdline_override {
        return s.to_string();
    }

    let joined = generation.kernel_params.join(" ");
    if joined
        .split_ascii_whitespace()
        .any(|t| t.starts_with("init="))
    {
        return joined;
    }

    let init_arg = match generation.init_path.strip_prefix(system_root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => {
            nmbl_warn!(
                "init path {} is not under system_root {}; passing through unchanged",
                generation.init_path.display(),
                system_root.display(),
            );
            generation.init_path.display().to_string()
        }
    };

    if joined.is_empty() {
        format!("init={init_arg}")
    } else {
        format!("{joined} init={init_arg}")
    }
}

/// Persist the byte-ring transcript to NMBL_LOG_PATH and return the
/// resulting bytes for cpio injection. Failures degrade to an empty
/// transcript: we still want the kexec to fire, and the absence of an
/// `/nmbl-log/nmbl.log` entry in the next kernel's initramfs is a
/// recoverable diagnostic, not a boot-blocker. The `mkdir -p` of the
/// parent matches the same step in `execute_terminal_action`'s flush
/// so the file is reachable here even when the dispatcher flush in
/// `main` hasn't run yet (it runs after `kexec_into` returns).
fn stage_log_for_kexec() -> Vec<u8> {
    let log_path = Path::new(NMBL_LOG_PATH);
    if let Some(parent) = log_path.parent() {
        // EEXIST is benign; any harder failure surfaces through flush_to.
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = log::flush_to(log_path) {
        nmbl_warn!(
            "kexec: failed to flush log to {} for staging: {err}",
            log_path.display()
        );
        return Vec::new();
    }
    match std::fs::read(log_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            nmbl_warn!(
                "kexec: failed to read flushed log at {} for staging: {err}",
                log_path.display()
            );
            Vec::new()
        }
    }
}

/// Build the cmdline, stage the log + key injections into the next
/// kernel's initramfs, then verify + measure the generation and load it
/// via `kexec_file_load(2)`. Returns the final cmdline so the caller can
/// log it before tearing the mounts down. The cutover syscall stays in
/// the dispatcher — this only fills the kexec image slot.
///
/// When `key_injections` is non-empty, an in-memory cpio fragment
/// containing those files is appended to the system initrd via
/// `memfd_create(2)` before `kexec_file_load(2)` — the typed
/// passphrases never touch disk.
pub(crate) fn verify_measure_then_load(
    config: &Config,
    generation: &Generation,
    cmdline_override: Option<&str>,
    key_injections: &[KeyInjection],
) -> Result<String> {
    let cmdline = build_cmdline(generation, cmdline_override, &config.paths.system_root);
    nmbl_info!(
        "kexec: loading generation {} (kernel={}, initrd={})",
        generation.number,
        generation.kernel.display(),
        generation.initrd.display()
    );

    // Stage the NMBL log transcript into the next kernel's initramfs.
    // The byte ring lives in RAM and the current tmpfs at NMBL_LOG_PATH
    // does not survive `reboot(LINUX_REBOOT_CMD_KEXEC)` — only what we
    // splice into the cpio fragment kexec_file_load(2) consumes reaches
    // the next kernel. We flush the ring to NMBL_LOG_PATH first (so the
    // helper that reads it back gets a header-aware snapshot identical
    // to the non-kexec terminal-action paths) and then read it back to
    // append as a cpio entry. Read failures degrade silently — the log
    // is best-effort and must never block the boot handoff.
    let log_bytes: Vec<u8> = stage_log_for_kexec();
    let log_path = Path::new(NMBL_LOG_PATH);

    let mut entries: Vec<InjectionEntry<'_>> = key_injections
        .iter()
        .map(|inj| InjectionEntry {
            path: inj.path.as_path(),
            content: inj.secret.as_slice(),
        })
        .collect();
    entries.push(InjectionEntry {
        path: log_path,
        content: log_bytes.as_slice(),
    });
    let fragment = build_fragment(&entries);
    if !key_injections.is_empty() {
        nmbl_info!(
            "kexec: injecting {} keyfile(s) + log into initrd via memfd ({} bytes)",
            key_injections.len(),
            fragment.len()
        );
    } else {
        nmbl_info!(
            "kexec: injecting log into initrd via memfd ({} bytes)",
            fragment.len()
        );
    }

    // F4 (FIX-02): verify generation over a single pinned fd HERE
    // F4 (FIX-12/27): measure into PCR-11 HERE
    sys::kexec::load_with_extra_initrd_cpio(
        &generation.kernel,
        &generation.initrd,
        fragment.as_slice(),
        &cmdline,
        0,
    )?;
    Ok(cmdline)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn gen_for(params: &[&str]) -> Generation {
        Generation {
            number: 42,
            profile_link: PathBuf::from("/mnt/system/nix/var/nix/profiles/system-42-link"),
            kernel: PathBuf::from("/mnt/system/boot/vmlinuz"),
            initrd: PathBuf::from("/mnt/system/boot/initrd"),
            init_path: PathBuf::from("/mnt/system/nix/var/nix/profiles/system-42-link/init"),
            kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
            label: String::new(),
        }
    }

    fn root() -> PathBuf {
        PathBuf::from("/mnt/system")
    }

    #[test]
    fn build_cmdline_override_used_verbatim() {
        let g = gen_for(&["root=/dev/sda1", "quiet"]);
        let s = "init=/sbin/init debug";
        assert_eq!(build_cmdline(&g, Some(s), &root()), s);
    }

    #[test]
    fn build_cmdline_no_override_joins_params_and_appends_init() {
        let g = gen_for(&["root=/dev/sda1", "ro", "quiet"]);
        assert_eq!(
            build_cmdline(&g, None, &root()),
            "root=/dev/sda1 ro quiet init=/nix/var/nix/profiles/system-42-link/init",
        );
    }

    #[test]
    fn build_cmdline_empty_override_yields_empty() {
        let g = gen_for(&["root=/dev/sda1"]);
        assert_eq!(build_cmdline(&g, Some(""), &root()), "");
    }

    #[test]
    fn injects_init_when_missing() {
        let mut g = gen_for(&["root=fstab"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
        let out = build_cmdline(&g, None, &root());
        assert!(
            out.ends_with(" init=/nix/store/abc/init"),
            "unexpected cmdline: {out}",
        );
    }

    #[test]
    fn respects_existing_init_in_params() {
        let mut g = gen_for(&["init=/explicit"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/xyz/init");
        assert_eq!(build_cmdline(&g, None, &root()), "init=/explicit");
    }

    #[test]
    fn override_passes_through() {
        let mut g = gen_for(&["root=fstab"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/xyz/init");
        assert_eq!(build_cmdline(&g, Some("foo bar"), &root()), "foo bar");
    }

    #[test]
    fn init_outside_system_root_warns_but_uses_raw() {
        let mut g = gen_for(&["root=fstab"]);
        g.init_path = PathBuf::from("/elsewhere/init");
        let out = build_cmdline(&g, None, &root());
        assert!(
            out.ends_with(" init=/elsewhere/init"),
            "unexpected cmdline: {out}",
        );
    }

    #[test]
    fn empty_params_still_inject_init() {
        let mut g = gen_for(&[]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
        assert_eq!(build_cmdline(&g, None, &root()), "init=/nix/store/abc/init");
    }

    #[test]
    fn init_token_matched_only_at_token_start() {
        // A param ending in "init=" must NOT short-circuit injection — the
        // check looks at whole whitespace tokens, not substrings.
        let mut g = gen_for(&["weird_suffix_init=foo"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
        let out = build_cmdline(&g, None, &root());
        assert!(out.contains(" init=/nix/store/abc/init"), "got: {out}");
    }
}
