//! Thin wrapper around `uname(2)` via `nix::sys::utsname`.
//!
//! Used by `modules.rs` to locate `/lib/modules/<release>/modules.dep`
//! and by the debug banner that prints what kernel we booted under.

use std::ffi::OsStr;

use nix::sys::utsname::uname as nix_uname;

use crate::error::{NmblError, Result};

/// Full uname output — useful for debugging banners.
pub struct Uname {
    pub sysname: String,
    pub nodename: String,
    pub release: String,
    pub version: String,
    pub machine: String,
}

/// Calls `uname(2)` and returns the `release` field as an owned String,
/// e.g. `"6.6.51"`.
pub fn kernel_release() -> Result<String> {
    Ok(uname()?.release)
}

/// Calls `uname(2)` and returns every field as an owned String.
pub fn uname() -> Result<Uname> {
    let uts = nix_uname().map_err(|errno| NmblError::Io {
        source: std::io::Error::from(errno),
        context: "uname".to_string(),
    })?;

    // NixOS kernels are built with an ASCII version string (no kernelversion
    // glyphs), so to_string_lossy() is effectively lossless here.
    Ok(Uname {
        sysname: to_owned(uts.sysname()),
        nodename: to_owned(uts.nodename()),
        release: to_owned(uts.release()),
        version: to_owned(uts.version()),
        machine: to_owned(uts.machine()),
    })
}

fn to_owned(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_release_is_non_empty() -> Result<()> {
        let release = kernel_release()?;
        if release.is_empty() {
            return Err(NmblError::Io {
                source: std::io::Error::other("empty kernel release"),
                context: "uname-test".to_string(),
            });
        }
        Ok(())
    }
}
