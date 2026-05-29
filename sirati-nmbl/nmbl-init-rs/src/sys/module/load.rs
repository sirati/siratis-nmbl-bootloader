//! Kernel module decompression and loading via `init_module(2)`.
//!
//! All compression backends are pure Rust — `lzma-rs` for XZ,
//! `ruzstd` for Zstandard, `flate2`'s `rust_backend` for gzip — so the
//! static-musl build stays free of C library dependencies.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::os::raw::{c_ulong, c_void};
use std::path::Path;

use nix::errno::Errno;

use crate::error::Result;
use crate::nmbl_warn;

use super::dep::{LoadOutcome, ModuleEntry, is_recoverable_module_error, module_err};

/// Compression scheme of a `.ko*` file, inferred from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression — raw ELF (`.ko`).
    None,
    /// XZ / LZMA2 (`.ko.xz`).
    Xz,
    /// Zstandard (`.ko.zst`).
    Zst,
    /// gzip / DEFLATE (`.ko.gz`).
    Gz,
}

/// Classify a `.ko*` path by compression suffix. Paths whose
/// `file_name()` is not UTF-8 or whose extension is unrecognised fall
/// back to `Compression::None` — `decompress_module` will then try to
/// load the file verbatim, and the kernel will reject it with
/// `ENOEXEC` if it is not a valid ELF.
pub(crate) fn compression_for_path(path: &Path) -> Compression {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return Compression::None;
    };
    if name.ends_with(".ko.xz") {
        Compression::Xz
    } else if name.ends_with(".ko.zst") {
        Compression::Zst
    } else if name.ends_with(".ko.gz") {
        Compression::Gz
    } else {
        Compression::None
    }
}

/// Read `path` from disk and decompress it according to the suffix
/// reported by [`compression_for_path`]. Returns the raw module image
/// suitable for `init_module(2)`.
///
/// Decompression failures are surfaced as `NmblError::Module { …,
/// source: Errno::EIO }` (we have no errno for "userspace
/// decompressor blew up"); a `nmbl_warn!` immediately precedes the
/// return so the operator sees the real decompressor message.
pub(crate) fn decompress_module(path: &Path, name: &str) -> Result<Vec<u8>> {
    use crate::error::NmblError;
    let raw = std::fs::read(path).map_err(|source| NmblError::Io {
        source,
        context: format!("reading kernel module {}", path.display()),
    })?;
    match compression_for_path(path) {
        Compression::None => Ok(raw),
        Compression::Xz => {
            // `lzma_rs::xz_decompress` wants a `BufRead`; wrap the
            // owned `Vec<u8>` in a `Cursor` (which is `BufRead`).
            let mut reader = std::io::Cursor::new(&raw);
            let mut out = Vec::with_capacity(raw.len() * 3);
            lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| {
                nmbl_warn!(
                    "xz decompression failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            Ok(out)
        }
        Compression::Zst => {
            let mut decoder = ruzstd::StreamingDecoder::new(raw.as_slice()).map_err(|e| {
                nmbl_warn!(
                    "zstd frame init failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            let mut out = Vec::with_capacity(raw.len() * 3);
            decoder.read_to_end(&mut out).map_err(|e| {
                nmbl_warn!(
                    "zstd decompression failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            Ok(out)
        }
        Compression::Gz => {
            let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
            let mut out = Vec::with_capacity(raw.len() * 3);
            decoder.read_to_end(&mut out).map_err(|e| {
                nmbl_warn!(
                    "gzip decompression failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            Ok(out)
        }
    }
}

/// Call `init_module(image, len, params)` — the original
/// (non-`f`-prefixed) module-load syscall — on a raw module image.
///
/// We use this rather than `finit_module(2)` because NixOS kernels are
/// not built with `CONFIG_MODULE_DECOMPRESS=y`; userspace must
/// decompress (`decompress_module`) and then pass the resulting ELF
/// bytes here. See module-level docs for the full rationale.
///
/// Returns:
/// * `Ok(LoadOutcome::Loaded)` on success.
/// * `Ok(LoadOutcome::AlreadyLoaded)` when the kernel reports `EEXIST`.
/// * `Ok(LoadOutcome::KernelRefused { source })` for errnos classified
///   as recoverable by [`is_recoverable_module_error`].
/// * `Err(NmblError::Module)` for every other errno.
fn init_module(image: &[u8], params: &CString, name: &str, path: &Path) -> Result<LoadOutcome> {
    // SAFETY: Unavoidable raw syscall.
    //   * Why no safe wrapper: no Rust crate wraps `init_module(2)` —
    //     `nix` (0.29) exposes neither this nor `finit_module`;
    //     `rustix` 0.38 has no covering API; `libkmod` is a C library
    //     we explicitly do not want in PID 1. (Verified by inspecting
    //     `nix` 0.29 and `rustix` 0.38 sources in the offline cache.)
    //   * Why this is safe: `image` is a live borrow valid for the
    //     duration of the call, `params` is a NUL-terminated C string
    //     owned by the caller, and `image.len()` cannot exceed
    //     `c_ulong::MAX` because a `Vec<u8>` can hold at most
    //     `isize::MAX` bytes. The syscall reads, never writes, our
    //     pointers; failure is reported via the return value + errno.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_init_module,
            image.as_ptr() as *const c_void,
            image.len() as c_ulong,
            params.as_ptr(),
        )
    };
    if rc == 0 {
        return Ok(LoadOutcome::Loaded);
    }
    let errno = Errno::last();
    if errno == Errno::EEXIST {
        // Module is already in the kernel — treat as success.
        return Ok(LoadOutcome::AlreadyLoaded);
    }
    if is_recoverable_module_error(errno) {
        return Ok(LoadOutcome::KernelRefused { source: errno });
    }
    Err(module_err(name, path, errno))
}

/// Read `entry.path` from disk, decompress it if its extension says
/// so, and hand the raw module image to `init_module(2)`.
///
/// File-IO and decompression errors surface as `NmblError::Module`.
/// Idempotent re-loads return `Ok(LoadOutcome::AlreadyLoaded)`;
/// kernel-side refusals return `Ok(LoadOutcome::KernelRefused { … })`
/// and must be logged + skipped by the caller rather than aborting
/// the boot.
pub fn load_module(entry: &ModuleEntry) -> Result<LoadOutcome> {
    // Shrunk module closures (NixOS `makeModulesClosure { allowMissing
    // = true; }`) can leave `modules.dep` referencing `.ko` files that
    // aren't on disk. Surface that as `FileMissing` so callers warn +
    // skip instead of aborting the boot for an over-eager soft dep.
    if !entry.path.exists() {
        return Ok(LoadOutcome::FileMissing);
    }
    let image = decompress_module(&entry.path, &entry.name)?;
    // `modules.dep` carries no parameters and the bash bootloader never
    // set them — pass an empty string and move on.
    let params = CString::default();
    init_module(&image, &params, &entry.name, &entry.path)
}

/// Resolve `name` against `by_name`, then load every entry in load
/// order. Idempotent because individual `load_module` calls report
/// `EEXIST` as [`LoadOutcome::AlreadyLoaded`].
///
/// A kernel-refused dep is logged as a warning and skipped — the parent
/// module will most likely also be refused on the next iteration and
/// the operator will see the cascade. If a downstream phase actually
/// needs the missing module, it will fail with its own (more pointed)
/// error there.
pub fn load_with_deps(name: &str, by_name: &HashMap<String, &ModuleEntry>) -> Result<()> {
    use super::dep::resolve_load_order;
    for entry in resolve_load_order(name, by_name)? {
        match load_module(entry)? {
            LoadOutcome::Loaded | LoadOutcome::AlreadyLoaded => {}
            LoadOutcome::KernelRefused { source } => {
                nmbl_warn!(
                    "module {} could not be loaded by the kernel ({}); \
                     continuing — if a downstream phase needs it the \
                     error will be clearer there",
                    entry.name,
                    source
                );
            }
            LoadOutcome::FileMissing => {
                nmbl_warn!(
                    "module {} listed in modules.dep but {} is not in the \
                     initrd; skipping (closure was likely shrunk with \
                     allowMissing=true)",
                    entry.name,
                    entry.path.display()
                );
            }
        }
    }
    Ok(())
}
