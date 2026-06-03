//! Per-generation sidecar location (#18 — R-4/FIX-07).
//!
//! Finds a generation's detached signature sidecars on the writable boot
//! partition, under the layout the install signer (#53) writes to:
//!
//! ```text
//! <runtime_boot_mountpoint>/nmbl/sigs/<gen-id>/kernel<suffix>
//! <runtime_boot_mountpoint>/nmbl/sigs/<gen-id>/initrd<suffix>
//! ```
//!
//! where `<gen-id>` is the SHARED [`crate::generations::gen_id`] derivation
//! (FIX-07 — the SAME id the host signer computes via `--print-gen-id`) and
//! `<suffix>` is `config.signing.sig_path_suffix` (default `.sig`). The sidecar
//! directory lives on the writable boot FS, NOT in the read-only Nix store, so
//! the installer can drop new sidecars next to an existing generation without
//! rebuilding it.
//!
//! This module ONLY resolves and locates — it never parses or verifies. The
//! verify pipeline ([`crate::sig::verify`]) opens the resolved path and runs
//! the ML-DSA check; the Wave-2 pre-kexec guard (#20) calls
//! [`crate::sig::gate`] which routes through here for the path and through the
//! verifier for the bytes.

use std::path::PathBuf;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::{Generation, gen_id};

/// Sub-directory under the boot mountpoint holding all per-generation sidecar
/// dirs. Frozen as part of the R-4 layout the install signer also hard-codes.
pub const SIGS_SUBDIR: &str = "nmbl/sigs";

/// Which blob of a generation a sidecar covers. Selects the sidecar filename
/// stem (`kernel` / `initrd`) under the per-generation directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenBlob {
    /// The generation's kernel image (`kernel<suffix>`).
    Kernel,
    /// The generation's initrd image (`initrd<suffix>`).
    Initrd,
}

impl GenBlob {
    /// The filename stem for this blob's sidecar (before `sig_path_suffix`).
    #[must_use]
    pub fn stem(self) -> &'static str {
        match self {
            Self::Kernel => "kernel",
            Self::Initrd => "initrd",
        }
    }
}

/// Outcome of locating a single generation sidecar.
///
/// Carries the fully-resolved path and whether the file is present on disk.
/// The locator NEVER decides policy: a `Missing` sidecar is reported as-is, and
/// the caller (audit vs enforce — see [`crate::sig::gate::apply_policy`])
/// decides whether absence is fatal. This keeps the present/absent fact in ONE
/// place while leaving the gate to own the decision.
#[derive(Debug, Clone)]
pub struct SidecarResolution {
    /// Absolute path the sidecar should live at.
    pub path: PathBuf,
    /// `true` iff the path exists and is a regular file. A path that exists but
    /// is a directory / dangling symlink is reported `false` (it is not a
    /// usable sidecar) rather than erroring, so the gate sees a clean
    /// present/absent signal.
    pub present: bool,
}

/// Resolve the per-generation sidecar directory `<boot>/nmbl/sigs/<gen-id>/`.
///
/// `<gen-id>` is the shared [`gen_id`] (FIX-07). Fails if there is no writable
/// boot mountpoint recorded (Phase 0.5 sets `runtime_boot_mountpoint`) or the
/// generation's toplevel has no resolvable store basename — either is a hard
/// "cannot locate sidecars" error, never a silent allow-all.
pub fn generation_sig_dir(config: &Config, generation: &Generation) -> Result<PathBuf> {
    let boot = config
        .runtime_boot_mountpoint
        .as_deref()
        .ok_or_else(|| NmblError::Signature {
            stage: "gen-sig-dir",
            detail: "no runtime boot mountpoint to locate generation sidecars".to_string(),
        })?;
    let id = gen_id(generation)?;
    Ok(boot.join(SIGS_SUBDIR).join(id))
}

/// Locate one generation sidecar (kernel or initrd) and report whether it is
/// present.
///
/// Builds `<boot>/nmbl/sigs/<gen-id>/<stem><suffix>` and stats it. A read error
/// other than "not found" (e.g. EACCES on the boot FS) is surfaced as
/// [`NmblError::Io`] so a broken boot partition is not mistaken for a missing
/// sidecar; plain absence resolves to `present: false`.
pub fn resolve_sig_sidecar(
    config: &Config,
    generation: &Generation,
    blob: GenBlob,
) -> Result<SidecarResolution> {
    let dir = generation_sig_dir(config, generation)?;
    let suffix = config.signing.sig_path_suffix.as_str();
    let path = dir.join(format!("{}{suffix}", blob.stem()));

    let present = match std::fs::metadata(&path) {
        Ok(meta) => meta.is_file(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(NmblError::Io {
                source,
                context: format!("stat generation sidecar {}", path.display()),
            });
        }
    };

    Ok(SidecarResolution { path, present })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on the sidecar-resolution contract"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;

    /// Build a `Config` whose `runtime_boot_mountpoint` is `boot` and whose
    /// signing suffix is the default `.sig`. `runtime_boot_mountpoint` is
    /// `#[serde(skip)]` (set by Phase 0.5, never parsed), so we set the field
    /// directly after parsing.
    fn config_with_boot(boot: &Path) -> Config {
        let mut config =
            toml::from_str::<Config>("[paths]\nshell = \"/bin/sh\"\n[signing]\nenable = true\n")
                .expect("config parses");
        config.runtime_boot_mountpoint = Some(boot.to_path_buf());
        config
    }

    /// Build a `Generation` whose `toplevel` canonicalizes to a store-like
    /// basename, so [`gen_id`] resolves deterministically in the test.
    fn generation_with_toplevel(toplevel: &Path) -> Generation {
        Generation {
            number: 7,
            profile_link: toplevel.to_path_buf(),
            toplevel: toplevel.to_path_buf(),
            kernel: toplevel.join("kernel"),
            initrd: toplevel.join("initrd"),
            init_path: toplevel.join("init"),
            kernel_params: Vec::new(),
            label: String::new(),
        }
    }

    /// A real on-disk store-style toplevel `<root>/<basename>`; canonicalize
    /// yields exactly `<basename>` for the gen-id.
    fn make_toplevel(root: &Path, basename: &str) -> PathBuf {
        let top = root.join(basename);
        std::fs::create_dir_all(&top).expect("toplevel dir");
        top
    }

    #[test]
    fn resolves_missing_sidecar_as_absent() {
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        std::fs::create_dir_all(&boot).expect("boot");
        let top = make_toplevel(tmp.path(), "abc123-nixos-system");
        let cfg = config_with_boot(&boot);
        let g = generation_with_toplevel(&top);

        let res = resolve_sig_sidecar(&cfg, &g, GenBlob::Kernel).expect("resolve ok");
        assert!(!res.present, "no file written ⇒ absent");
        // Path layout: <boot>/nmbl/sigs/<gen-id>/kernel.sig
        assert_eq!(
            res.path,
            boot.join("nmbl/sigs/abc123-nixos-system/kernel.sig")
        );
    }

    #[test]
    fn resolves_present_sidecar() {
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        let top = make_toplevel(tmp.path(), "def456-nixos-system");
        let cfg = config_with_boot(&boot);
        let g = generation_with_toplevel(&top);

        // Write both sidecars under the resolved per-generation dir.
        let dir = generation_sig_dir(&cfg, &g).expect("dir");
        std::fs::create_dir_all(&dir).expect("sig dir");
        std::fs::write(dir.join("kernel.sig"), b"k").expect("kernel sig");
        std::fs::write(dir.join("initrd.sig"), b"i").expect("initrd sig");

        let k = resolve_sig_sidecar(&cfg, &g, GenBlob::Kernel).expect("kernel resolve");
        let i = resolve_sig_sidecar(&cfg, &g, GenBlob::Initrd).expect("initrd resolve");
        assert!(k.present && i.present);
        assert!(k.path.ends_with("kernel.sig"));
        assert!(i.path.ends_with("initrd.sig"));
    }

    #[test]
    fn honours_custom_suffix() {
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        std::fs::create_dir_all(&boot).expect("boot");
        let top = make_toplevel(tmp.path(), "ghi789-nixos-system");
        let mut cfg = toml::from_str::<Config>(
            "[paths]\nshell = \"/bin/sh\"\n[signing]\nenable = true\nsig_path_suffix = \".mldsa\"\n",
        )
        .expect("config parses");
        cfg.runtime_boot_mountpoint = Some(boot.to_path_buf());
        let g = generation_with_toplevel(&top);

        let res = resolve_sig_sidecar(&cfg, &g, GenBlob::Initrd).expect("resolve");
        assert!(res.path.ends_with("initrd.mldsa"));
    }

    #[test]
    fn directory_in_place_of_sidecar_is_absent_not_present() {
        // A directory at the sidecar path is not a usable sidecar; the locator
        // must report it absent rather than present-but-unparseable.
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        let top = make_toplevel(tmp.path(), "jkl012-nixos-system");
        let cfg = config_with_boot(&boot);
        let g = generation_with_toplevel(&top);
        let dir = generation_sig_dir(&cfg, &g).expect("dir");
        std::fs::create_dir_all(dir.join("kernel.sig")).expect("dir-as-sidecar");

        let res = resolve_sig_sidecar(&cfg, &g, GenBlob::Kernel).expect("resolve");
        assert!(!res.present, "a directory is not a usable sidecar");
    }

    #[test]
    fn missing_boot_mountpoint_is_hard_error() {
        let tmp = TempDir::new().expect("temp");
        let top = make_toplevel(tmp.path(), "mno345-nixos-system");
        // No runtime_boot_mountpoint set.
        let cfg = toml::from_str::<Config>("[paths]\nshell = \"/bin/sh\"\n").expect("config");
        let g = generation_with_toplevel(&top);
        let err = resolve_sig_sidecar(&cfg, &g, GenBlob::Kernel).expect_err("must error");
        assert!(matches!(
            err,
            NmblError::Signature {
                stage: "gen-sig-dir",
                ..
            }
        ));
    }

    #[test]
    fn gen_id_is_basename_of_canonicalized_toplevel() {
        // gen_id follows the symlink to the store path and takes its basename;
        // it is the stable, content-addressed id the sidecar dir is keyed on.
        let tmp = TempDir::new().expect("temp");
        let store = make_toplevel(tmp.path(), "wxyz999-nixos-system-host");
        let link = tmp.path().join("system-9-link");
        symlink(&store, &link).expect("symlink");

        let mut g = generation_with_toplevel(&store);
        // Point toplevel at the LINK; gen_id must still resolve to the store
        // basename (rollback-stability property).
        g.toplevel = link;
        assert_eq!(gen_id(&g).expect("gen_id"), "wxyz999-nixos-system-host");
    }
}
