//! UAPI constants and `#[repr(C)]` structs for the loop-device interface.
//!
//! These types mirror `<linux/loop.h>` exactly; layout is pinned by the
//! unit tests in this module.

use rustix::ioctl::RawOpcode;

/// `/dev/loop-control` — single global control node used to allocate
/// and release loop indices.
pub const LOOP_CONTROL_PATH: &str = "/dev/loop-control";

/// sysfs path exposing `major:minor` for the loop-control misc device.
/// Read at runtime so we never hard-code the (10:237) pair.
pub(super) const LOOP_CONTROL_SYSFS: &str = "/sys/class/misc/loop-control/dev";

/// Loop-control ioctl: return the index of an unused loop device,
/// allocating one if none is free. Result is the index (≥0); negative
/// means failure.
pub const LOOP_CTL_GET_FREE: RawOpcode = 0x4C82;

/// Per-loop ioctl: atomically bind a backing file fd and configure
/// the device parameters in one shot. Added in Linux 5.8 (commit
/// 3448914e8cc5, "loop: add LOOP_CONFIGURE ioctl"). NMBL targets
/// kernels ≥ 5.8 so we never need the legacy `LOOP_SET_FD` +
/// `LOOP_SET_STATUS64` fallback.
pub const LOOP_CONFIGURE: RawOpcode = 0x4C0A;

/// Per-loop ioctl: detach the backing file (the inverse of the
/// `LOOP_SET_FD` half of the old configure path). Takes no argument.
pub const LOOP_CLR_FD: RawOpcode = 0x4C01;

/// `LO_FLAGS_READ_ONLY` — set in `loop_info64.lo_flags` /
/// `loop_config.info.lo_flags` to mark the device read-only.
pub const LO_FLAGS_READ_ONLY: u32 = 1;

/// Size of the `lo_file_name` / `lo_crypt_name` fields in
/// `struct loop_info64` — `LO_NAME_SIZE` from `<linux/loop.h>`.
pub const LO_NAME_SIZE: usize = 64;

/// Size of the `lo_encrypt_key` field in `struct loop_info64` —
/// `LO_KEY_SIZE` from `<linux/loop.h>`.
pub const LO_KEY_SIZE: usize = 32;

/// Mirror of `struct loop_info64` from `<linux/loop.h>`.
///
/// Field order, sizes, and the trailing buffers must match the kernel
/// UAPI exactly: the kernel will read this verbatim and reject any
/// mismatched layout with `EINVAL`. The `info_size_matches_uapi`
/// unit test pins the size to 232 bytes (8*8 + 4*4 + 64 + 64 + 32 +
/// 2*8) so an accidental field re-order is caught at `cargo test`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LoopInfo64 {
    pub lo_device: u64,
    pub lo_inode: u64,
    pub lo_rdevice: u64,
    pub lo_offset: u64,
    pub lo_sizelimit: u64,
    pub lo_number: u32,
    pub lo_encrypt_type: u32,
    pub lo_encrypt_key_size: u32,
    pub lo_flags: u32,
    pub lo_file_name: [u8; LO_NAME_SIZE],
    pub lo_crypt_name: [u8; LO_NAME_SIZE],
    pub lo_encrypt_key: [u8; LO_KEY_SIZE],
    pub lo_init: [u64; 2],
}

impl LoopInfo64 {
    /// All-zeroes default. Plain `Default::default()` won't derive for
    /// the long byte arrays without `serde`/`bytemuck`, so spell it
    /// out — every field zero is also exactly what `LOOP_CONFIGURE`
    /// wants when the caller has no special parameters to set.
    #[inline]
    pub const fn zeroed() -> Self {
        Self {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_sizelimit: 0,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_file_name: [0; LO_NAME_SIZE],
            lo_crypt_name: [0; LO_NAME_SIZE],
            lo_encrypt_key: [0; LO_KEY_SIZE],
            lo_init: [0; 2],
        }
    }
}

/// Mirror of `struct loop_config` from `<linux/loop.h>` (Linux 5.8+).
///
/// Layout is `fd, block_size, loop_info64 info, __u64 __reserved[8]`.
/// The trailing reserved field MUST be present and zero — the kernel
/// reads `sizeof(struct loop_config)` bytes and would `EINVAL` on a
/// short struct. The `config_size_matches_uapi` test pins the total
/// at 304 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    pub fd: u32,
    pub block_size: u32,
    pub info: LoopInfo64,
    // Maps to the C struct's `__u64 __reserved[8]`; named without the
    // leading underscores because Rust reserves those for the language.
    pub reserved: [u64; 8],
}

impl LoopConfig {
    /// Zeroed-out config with only `fd` populated. `block_size = 0`
    /// asks the kernel to pick its default (typically the backing
    /// file's I/O block size); `info` is all zeroes which means
    /// "no offset, no size limit, no flags".
    #[inline]
    pub const fn for_fd(fd: u32) -> Self {
        Self {
            fd,
            block_size: 0,
            info: LoopInfo64::zeroed(),
            reserved: [0; 8],
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn ioctl_constants_match_kernel_uapi() {
        // Pin the opcodes from <linux/loop.h> so a stray edit gets
        // caught in CI without booting a kernel.
        assert_eq!(LOOP_CTL_GET_FREE, 0x4C82);
        assert_eq!(LOOP_CONFIGURE, 0x4C0A);
        assert_eq!(LOOP_CLR_FD, 0x4C01);
        assert_eq!(LO_FLAGS_READ_ONLY, 1);
        assert_eq!(LO_NAME_SIZE, 64);
        assert_eq!(LO_KEY_SIZE, 32);
    }

    #[test]
    fn info_size_matches_uapi() {
        // 5*8 (u64s) + 4*4 (u32s) + 64 + 64 + 32 + 2*8 = 232.
        assert_eq!(size_of::<LoopInfo64>(), 232);
    }

    #[test]
    fn config_size_matches_uapi() {
        // Layout of `struct loop_config`:
        //   fd:         4
        //   block_size: 4
        //   info:     232  (LoopInfo64)
        //   reserved:  64  (8 * u64)
        //   total:    304
        // Must match what the running kernel reads.
        assert_eq!(size_of::<LoopConfig>(), 304);
    }

    #[test]
    fn loop_info64_zeroed_is_all_zero() {
        let info = LoopInfo64::zeroed();
        assert_eq!(info.lo_device, 0);
        assert_eq!(info.lo_inode, 0);
        assert_eq!(info.lo_rdevice, 0);
        assert_eq!(info.lo_offset, 0);
        assert_eq!(info.lo_sizelimit, 0);
        assert_eq!(info.lo_number, 0);
        assert_eq!(info.lo_encrypt_type, 0);
        assert_eq!(info.lo_encrypt_key_size, 0);
        assert_eq!(info.lo_flags, 0);
        assert!(info.lo_file_name.iter().all(|b| *b == 0));
        assert!(info.lo_crypt_name.iter().all(|b| *b == 0));
        assert!(info.lo_encrypt_key.iter().all(|b| *b == 0));
        assert_eq!(info.lo_init, [0, 0]);
    }

    #[test]
    fn loop_config_for_fd_only_sets_fd() {
        let cfg = LoopConfig::for_fd(7);
        assert_eq!(cfg.fd, 7);
        assert_eq!(cfg.block_size, 0);
        assert_eq!(cfg.info.lo_flags, 0);
        assert_eq!(cfg.info.lo_offset, 0);
        assert_eq!(cfg.info.lo_sizelimit, 0);
        assert_eq!(cfg.reserved, [0; 8]);
    }
}
