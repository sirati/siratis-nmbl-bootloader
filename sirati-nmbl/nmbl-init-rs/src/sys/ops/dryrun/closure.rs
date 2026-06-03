//! [`ClosureView`]: presence/read oracle over an initramfs closure.
//!
//! A `--validate-initrm` run needs to answer one question for every file
//! the boot would touch: *is this path present (and readable) in the
//! image we are about to ship?* `ClosureView` answers it against either
//!
//! * an EXTRACTED initramfs cpio ROOT directory (build / sandbox mode —
//!   a real directory on disk that is the unpacked initrd), or
//! * `/` itself (runtime mode — validating against the live initramfs we
//!   already booted into).
//!
//! It does NOT extract cpio: the caller (the nix derivation, or the next
//! phase's `--validate-initrm` driver) unpacks the initrd to a temp dir
//! and hands the root in. Reusing the unpacked tree keeps this module a
//! thin path-join + `std::fs` wrapper rather than a cpio reimplementation.
//!
//! ## Path-join semantics
//!
//! Boot code references files by their RUNTIME absolute path, e.g.
//! `/lib/modules/<release>/kernel/fs/ext4/ext4.ko.xz` or `/bin/blkid`.
//! `ClosureView` resolves such a path P **under** `root`:
//!
//! * a leading `/` on P is stripped, then the remainder is joined onto
//!   `root` — so `/lib/.../ext4.ko` resolves to
//!   `<root>/lib/.../ext4.ko`;
//! * for the runtime view (`root == "/"`) that join is the identity, so
//!   `/bin/blkid` resolves back to `/bin/blkid`;
//! * a relative P (no leading `/`) is joined verbatim under `root`.
//!
//! This is deliberately a *prefix graft*, NOT a symlink-resolving chroot:
//! we never follow `..` out of the root and never canonicalize, because
//! the closure tree is trusted build output and the boot references are
//! already absolute. A path containing `..` is left to `std::fs` to
//! resolve lexically against the grafted root.

use std::io;
use std::path::{Component, Path, PathBuf};

/// Presence/read oracle over an initramfs closure rooted at `root`.
///
/// `root` is either the extracted-initrd directory (build mode) or `/`
/// (runtime mode). See the module docs for the path-join contract.
#[derive(Debug, Clone)]
pub struct ClosureView {
    root: PathBuf,
}

impl ClosureView {
    /// Build a view rooted at `root`. Pass the extracted-initrd directory
    /// for build/sandbox validation, or `/` to validate against the live
    /// initramfs at runtime.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Borrow the closure root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a boot-referenced path P to its on-disk location under
    /// `root`. A leading `/` is stripped so the absolute boot path grafts
    /// under the closure root; `.` / leading-`..` components are dropped
    /// so the resolved path can never escape the root. For the runtime
    /// view (`root == "/"`) this returns P unchanged.
    fn resolve(&self, p: &Path) -> PathBuf {
        // Fast path: runtime root is the filesystem root, so the graft is
        // the identity. Comparing against "/" keeps `/bin/blkid` →
        // `/bin/blkid` rather than the lexically-equivalent but uglier
        // `//bin/blkid` a naive join would produce.
        if self.root == Path::new("/") {
            return p.to_path_buf();
        }
        let mut out = self.root.clone();
        for comp in p.components() {
            match comp {
                // Drop the leading `/` (RootDir) and any `.` so the
                // remainder grafts under root.
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
                // Refuse to climb out of the closure root; a boot path
                // should never contain `..`, but if it did we clamp it to
                // root rather than escape.
                Component::ParentDir => {}
                Component::Normal(seg) => out.push(seg),
            }
        }
        out
    }

    /// `true` if P resolves to an existing entry under `root`. A stat
    /// error (permission, broken symlink) collapses to `false`, matching
    /// `RealSys::exists`.
    #[must_use]
    pub fn exists(&self, p: &Path) -> bool {
        self.resolve(p).try_exists().unwrap_or(false)
    }

    /// Read the whole file at P (resolved under `root`). Propagates the
    /// `io::Error` so callers can mirror `RealSys::read_file`'s fallback
    /// behaviour (optional reads degrade; required reads record a
    /// finding).
    pub fn read_file(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(self.resolve(p))
    }

    /// Resolve P to its on-disk location under `root` (the prefix-graft the
    /// presence/read oracle uses), so a caller can `File::open` the shipped
    /// bytes read-only (the `open_ro` dry-run path). Side-effect-free.
    #[must_use]
    pub fn resolve_path(&self, p: &Path) -> PathBuf {
        self.resolve(p)
    }

    /// Canonicalize P WITHIN the closure: graft P under `root` (the same
    /// prefix-graft `open_ro`/`read_file` use), then `std::fs::canonicalize` the
    /// grafted path so symlinks resolve INSIDE the extracted tree rather than on
    /// the host. The returned path is the real on-disk location under `root`;
    /// `gen_id` takes only its basename, so the closure-resolved store basename
    /// is what a closure-rooted `--validate-initrm` run derives. For the runtime
    /// view (`root == "/"`) the graft is the identity, so this is a plain
    /// `std::fs::canonicalize` — byte-identical to the real boot.
    pub fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(self.resolve(p))
    }
}

// Closure-probing helpers for `DryRunSys` that don't perform side effects:
// the module-load presence walk and the console file-dep probes both read
// (or stat) the closure and record findings, so they live next to
// `ClosureView` rather than bloating the top-level impl.
use std::collections::HashSet;

use crate::config::Config;
use crate::sys::module::{self, canonical_module_name};

use super::DryRunSys;
use super::report::MissingFile;

impl DryRunSys {
    /// Walk the chosen module list with the PURE order/dependency
    /// computation from `sys::module` + `crate::modules::filter_blacklisted`,
    /// then presence-check each `.ko` under the closure. Records a
    /// `"load_module"` finding per absent module file. Replaces ONLY the
    /// `init_module(2)` syscall with the presence check; the ordering,
    /// blacklist, and built-in-soft-skip logic mirror
    /// `modules::load_modules_inner` exactly. Reads `modules.dep` THROUGH
    /// the closure so it works in build/sandbox mode where the host has no
    /// `/lib/modules`.
    pub(super) fn dryrun_modules(
        &mut self,
        modules_dir: &Path,
        explicit: &[String],
        blacklist: &[String],
    ) {
        if explicit.is_empty() {
            return;
        }
        let release = match crate::sys::uname::kernel_release() {
            Ok(r) => r,
            Err(_) => return,
        };
        let root = modules_dir.join(&release);
        let dep_path = root.join("modules.dep");
        let dep_bytes = match self.closure().read_file(&dep_path) {
            Ok(b) => b,
            Err(_) => {
                self.record(MissingFile::new(
                    "load_module",
                    dep_path,
                    "modules.dep absent — modules tree not staged in initrd",
                ));
                return;
            }
        };
        let dep_text = String::from_utf8_lossy(&dep_bytes);
        let entries = module::parse_modules_dep_text(&dep_text, &root);
        let by_name = module::index_by_name(&entries);
        let blacklist_canonical: HashSet<String> =
            blacklist.iter().map(|b| canonical_module_name(b)).collect();
        let blacklist: HashSet<&str> = blacklist_canonical.iter().map(String::as_str).collect();

        for name in explicit {
            let canonical = canonical_module_name(name);
            // Same soft-skips the real loader uses: a blacklisted or
            // built-in (not in the tree) module is NOT a missing file.
            if blacklist.contains(canonical.as_str()) || !by_name.contains_key(canonical.as_str()) {
                continue;
            }
            let order = match module::resolve_load_order(name, &by_name) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let (to_load, _skipped) = crate::modules::filter_blacklisted(order, &blacklist);
            for entry in to_load {
                if !self.closure().exists(&entry.path) {
                    self.record(MissingFile::new(
                        "load_module",
                        entry.path.clone(),
                        format!("module {} (dep of {name}) not in initrd", entry.name),
                    ));
                }
            }
        }
    }

    /// Drive the SAME plain-file probes `SplashConsole::open` depends on,
    /// via the closure, WITHOUT opening any device. The font is the
    /// hard-required asset (a missing font drops the operator to the
    /// embedded fallback, so it is recorded but informational); the
    /// background PNG and DRM card / `/dev/tty1` are best-effort. Mirrors
    /// `SplashConsole::open`'s file deps; never touches DRM or the tty.
    #[cfg(feature = "image-splash")]
    pub(super) fn probe_console_files(&mut self, config: &Config) {
        use crate::config::SplashBackgroundLocation;
        if !config.splash.enable {
            return;
        }
        let font = config.splash.font_path.clone();
        if !self.closure().exists(&font) {
            self.record(MissingFile::new(
                "open_console",
                font,
                "splash font absent — boot degrades to embedded fallback font",
            ));
        }
        // Background PNG: only the Initrd location reads a closure file;
        // BootPartition reads a sidecar that does not live in the initrd.
        if matches!(
            config.splash.background_location,
            SplashBackgroundLocation::Initrd
        ) {
            let bg = config.splash.background_image.clone();
            if !self.closure().exists(&bg) {
                self.record(MissingFile::new(
                    "open_console",
                    bg,
                    "splash background PNG absent — best-effort, boot uses solid fill",
                ));
            }
        }
        // DRM card is a device, not a shippable file; existence in a
        // closure is best-effort/informational and we do not fail on it.
    }

    /// No-op probe when the splash feature is compiled out.
    #[cfg(not(feature = "image-splash"))]
    pub(super) fn probe_console_files(&mut self, _config: &Config) {}
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a unique temp dir for one test, populated by `setup`.
    fn temp_closure(tag: &str, setup: impl FnOnce(&Path)) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "nmbl-closure-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir temp closure");
        setup(&dir);
        dir
    }

    #[test]
    fn absolute_path_grafts_under_root() {
        let root = temp_closure("graft", |d| {
            fs::create_dir_all(d.join("lib/modules")).expect("mkdir");
            fs::write(d.join("lib/modules/ext4.ko"), b"ko").expect("write");
        });
        let view = ClosureView::new(root.clone());
        assert!(view.exists(Path::new("/lib/modules/ext4.ko")));
        assert!(!view.exists(Path::new("/lib/modules/missing.ko")));
        assert_eq!(
            view.read_file(Path::new("/lib/modules/ext4.ko")).unwrap(),
            b"ko"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn relative_path_joins_under_root() {
        let root = temp_closure("rel", |d| {
            fs::write(d.join("init"), b"x").expect("write");
        });
        let view = ClosureView::new(root.clone());
        assert!(view.exists(Path::new("init")));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parent_dir_cannot_escape_root() {
        let root = temp_closure("escape", |d| {
            fs::write(d.join("inside"), b"x").expect("write");
        });
        let view = ClosureView::new(root.clone());
        // `..` components are dropped, so this clamps to root/etc/passwd
        // and does NOT read the host's real /etc/passwd.
        let resolved = view.resolve(Path::new("/../../../etc/passwd"));
        assert!(resolved.starts_with(&root));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn runtime_root_is_identity() {
        let view = ClosureView::new(PathBuf::from("/"));
        assert_eq!(
            view.resolve(Path::new("/bin/blkid")),
            PathBuf::from("/bin/blkid")
        );
    }

    #[test]
    fn canonicalize_resolves_symlink_within_closure() {
        // A closure-rooted canonicalize must follow a symlink to the store dir
        // INSIDE the extracted tree (not escape to the host fs) and land on a
        // path whose basename is the store basename `gen_id` keys the sidecar
        // dir on.
        let root = temp_closure("canon", |d| {
            fs::create_dir_all(d.join("nix/store/abc123-system")).expect("store dir");
            std::os::unix::fs::symlink(d.join("nix/store/abc123-system"), d.join("system-link"))
                .expect("symlink");
        });
        let view = ClosureView::new(root.clone());
        // Boot-absolute profile-link path grafts under root, then canonicalize
        // follows the link to the store dir under root.
        let resolved = view
            .canonicalize(Path::new("/system-link"))
            .expect("canonicalize within closure");
        assert!(resolved.starts_with(&root), "must stay under closure root");
        assert_eq!(
            resolved.file_name().and_then(|n| n.to_str()),
            Some("abc123-system"),
            "basename must be the store basename gen_id derives",
        );
        fs::remove_dir_all(&root).ok();
    }
}
