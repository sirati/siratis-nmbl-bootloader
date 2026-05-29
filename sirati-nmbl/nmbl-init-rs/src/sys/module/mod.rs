//! Kernel module loading via `init_module(2)`, with a dep-graph
//! resolver fed by `<modules_dir>/<release>/modules.dep`.
//!
//! This is the moral replacement of the `modprobe` invocation in the
//! bash `mount-and-kernel.sh.nix`. It deliberately does not shell out.
//!
//! ## Why `init_module(2)` instead of `finit_module(2)`?
//!
//! The kernel-side `MODULE_INIT_COMPRESSED_FILE` flag (passed to
//! `finit_module`) only works when the running kernel was built with
//! `CONFIG_MODULE_DECOMPRESS=y`. NixOS kernels do **not** enable that
//! option; userspace (`kmod`) is expected to decompress modules before
//! handing them to the kernel. Passing the flag against such a kernel
//! results in `EOPNOTSUPP` on every load and the boot can never
//! progress past phase 3a. We therefore decompress `.ko.xz` /
//! `.ko.zst` / `.ko.gz` in-process with pure-Rust crates and call the
//! raw `init_module(2)` syscall with the resulting bytes.

pub(crate) mod dep;
pub(crate) mod load;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests;

// Re-export the full public API at the same paths callers already use.
pub(crate) use dep::canonical_module_name;
pub use dep::{
    LoadOutcome, ModuleEntry, index_by_name, is_recoverable_module_error, load_modules_dep,
    parse_modules_dep_text, resolve_load_order,
};
pub use load::{Compression, load_module, load_with_deps};
