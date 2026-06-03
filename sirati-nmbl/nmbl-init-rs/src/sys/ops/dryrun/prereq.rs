//! Build-time presence check for the kernel modules the driver-image
//! loop-mount REQUIRES in NMBL's own initramfs.
//!
//! A signed driver image is a squashfs on the boot partition that NMBL
//! loop-mounts and `finit_module`s before kexec
//! ([`crate::imageload::mount::mount_squashfs_ro`]). That loop-mount needs the
//! `loop` and `squashfs` kernel modules loaded in NMBL's OWN kernel — and,
//! since NMBL ships without udev/kmod auto-load, their `.ko` must be STAGED in
//! the initramfs `/lib/modules` tree. Unlike the rescue disk path
//! (`rescue::disk::ensure_loop_squashfs_modules`), nothing in the Nix layer
//! force-stages them for `driverImages.enable`, so an operator can enable
//! driver images yet ship an initramfs that can never loop-mount one.
//!
//! The per-scenario module walk ([`super::DryRunSys::dryrun_modules`])
//! deliberately SOFT-SKIPS a module absent from the extracted `modules.dep`
//! (it cannot tell a built-in module from one dropped out of the initramfs).
//! For these two modules that soft-skip is wrong: `loop`/`squashfs` are real
//! `.ko` in every stock NixOS kernel (never built in), so their ABSENCE from
//! the shipped tree is a hard initramfs-completeness defect, not a built-in.
//! This check therefore treats them as REQUIRED: a missing one is recorded as
//! a [`MissingFile`] finding so `--validate-initrm` fails the build.
//!
//! Only runs when `config.driver_images.enable`; otherwise it is a no-op (no
//! loop-mount happens, so the modules are not required).

use std::path::Path;

use crate::config::Config;
use crate::sys::module::{self, canonical_module_name};

use super::closure::ClosureView;
use super::report::MissingFile;

/// The kernel modules the driver-image loop-mount requires staged in the
/// initramfs (the loop device + the squashfs filesystem driver). Kept in step
/// with [`crate::imageload::mount::mount_squashfs_ro`], which binds a `loop`
/// device and mounts the image as `squashfs`.
const DRIVER_IMAGE_LOOPMOUNT_MODULES: [&str; 2] = ["loop", "squashfs"];

/// Presence-check the driver-image loop-mount prerequisite modules against
/// `closure`. Returns one [`MissingFile`] per required module whose `.ko` is
/// not staged in the closure's `/lib/modules/<release>` tree.
///
/// A no-op (empty vec) when `config.driver_images.enable` is false: with no
/// driver images to load, the loop-mount never runs and the modules are not
/// required.
#[must_use]
pub fn driver_image_prereq_findings(config: &Config, closure_root: &Path) -> Vec<MissingFile> {
    if !config.driver_images.enable {
        return Vec::new();
    }
    let closure = ClosureView::new(closure_root.to_path_buf());

    // Resolve the shipped modules tree (`<modules_dir>/<release>`) the same way
    // the boot-time loader does. If we cannot even read the release or the
    // tree's modules.dep, that itself means the modules are not loadable —
    // report every required module as missing rather than silently passing.
    let modules_dir = config.kernel_modules.modules_dir.clone();
    let release = match crate::sys::uname::kernel_release() {
        Ok(r) => r,
        Err(_) => return missing_all("could not read kernel release to resolve modules tree"),
    };
    let root = modules_dir.join(&release);
    let dep_path = root.join("modules.dep");
    let Ok(dep_bytes) = closure.read_file(&dep_path) else {
        return missing_all("modules.dep absent — modules tree not staged in initrd");
    };
    let dep_text = String::from_utf8_lossy(&dep_bytes);
    let entries = module::parse_modules_dep_text(&dep_text, &root);
    let by_name = module::index_by_name(&entries);

    let mut findings = Vec::new();
    for name in DRIVER_IMAGE_LOOPMOUNT_MODULES {
        let canonical = canonical_module_name(name);
        // REQUIRED — no built-in soft-skip: a driver-image config that cannot
        // loop-mount is broken. The module must be IN modules.dep AND its `.ko`
        // must exist in the staged tree.
        match by_name.get(canonical.as_str()) {
            Some(entry) if closure.exists(&entry.path) => {}
            _ => findings.push(MissingFile::new(
                "driver_image_prereq",
                root.join(format!("{name}.ko")),
                format!(
                    "driver_images.enable requires the `{name}` module staged in the \
                     initramfs to loop-mount a signed driver image, but it is not in \
                     the shipped /lib/modules tree"
                ),
            )),
        }
    }
    findings
}

/// Report every loop-mount prerequisite module as missing with `reason` — used
/// when the modules tree itself is unreadable, so none of them can be loaded.
fn missing_all(reason: &'static str) -> Vec<MissingFile> {
    DRIVER_IMAGE_LOOPMOUNT_MODULES
        .iter()
        .map(|name| {
            MissingFile::new(
                "driver_image_prereq",
                Path::new("/lib/modules").join(format!("{name}.ko")),
                format!("driver-image loop-mount prerequisite `{name}`: {reason}"),
            )
        })
        .collect()
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
    use std::path::PathBuf;

    /// Build a temp closure whose `/lib/modules/<release>` tree contains the
    /// given module `.ko` files plus a `modules.dep` listing them.
    fn closure_with_modules(tag: &str, modules: &[&str]) -> PathBuf {
        let release = crate::sys::uname::kernel_release().expect("kernel release");
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "nmbl-prereq-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let modroot = dir.join("lib/modules").join(&release);
        fs::create_dir_all(&modroot).expect("mkdir modroot");
        fs::create_dir_all(modroot.join("kernel")).expect("mkdir kernel");
        let mut dep = String::new();
        for m in modules {
            // The dep path and the on-disk `.ko` MUST agree: `modules.dep`
            // lists each module relative to the tree root, and the presence
            // check resolves that exact path under the closure.
            let rel = format!("kernel/{m}.ko");
            fs::write(modroot.join(&rel), b"ko").expect("write ko");
            dep.push_str(&format!("{rel}:\n"));
        }
        fs::write(modroot.join("modules.dep"), dep).expect("write dep");
        dir
    }

    fn driver_config(enable: bool) -> Config {
        let mut c = Config::recovery_default();
        c.driver_images.enable = enable;
        // `recovery_default` leaves `modules_dir` at its `Default` (empty); the
        // real build-time config.toml carries `/lib/modules`, so set it here to
        // mirror what `nmblInitrmCheck` validates.
        c.kernel_modules.modules_dir = PathBuf::from("/lib/modules");
        c
    }

    #[test]
    fn disabled_driver_images_is_a_noop() {
        let root = closure_with_modules("disabled", &[]);
        let findings = driver_image_prereq_findings(&driver_config(false), &root);
        assert!(findings.is_empty(), "no check when driver images are off");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn both_prereqs_present_passes() {
        let root = closure_with_modules("present", &["loop", "squashfs"]);
        let findings = driver_image_prereq_findings(&driver_config(true), &root);
        assert!(
            findings.is_empty(),
            "loop+squashfs staged ⇒ no finding: {findings:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_squashfs_is_reported() {
        // The exact build-time defect the SB BROKEN config models: loop is
        // staged but squashfs is not, so the driver image can never loop-mount.
        let root = closure_with_modules("no-squashfs", &["loop"]);
        let findings = driver_image_prereq_findings(&driver_config(true), &root);
        assert_eq!(findings.len(), 1, "exactly squashfs is missing: {findings:?}");
        let f = &findings[0];
        assert_eq!(f.op, "driver_image_prereq");
        assert!(
            f.path.to_string_lossy().contains("squashfs"),
            "finding names squashfs: {f:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_tree_reports_all_prereqs() {
        let empty = std::env::temp_dir().join(format!("nmbl-prereq-empty-{}", std::process::id()));
        fs::create_dir_all(&empty).expect("mkdir");
        let findings = driver_image_prereq_findings(&driver_config(true), &empty);
        assert_eq!(findings.len(), 2, "no modules tree ⇒ both reported");
        fs::remove_dir_all(&empty).ok();
    }
}
