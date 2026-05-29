//! Thin wrappers around the two `/dev/loop-control` + `/dev/loopN`
//! ioctls NMBL needs to mount the rescue squashfs without dragging in
//! `losetup(8)` from util-linux.
//!
//! The flow at runtime is:
//!   1. [`allocate_loop_device`] opens `/dev/loop-control` and issues
//!      `LOOP_CTL_GET_FREE`, returning the index of a free loop node.
//!   2. The caller opens `/dev/loopN` (RW for read-write attachment,
//!      RO is fine when only mounting read-only backing files).
//!   3. [`configure_loop_device`] hands the backing-file fd + the
//!      `LO_FLAGS_READ_ONLY` bit to `LOOP_CONFIGURE` (Linux ≥ 5.8),
//!      which atomically binds the fd and sets the device parameters
//!      — replacing the old `LOOP_SET_FD` + `LOOP_SET_STATUS64`
//!      two-step.
//!   4. The caller mounts `/dev/loopN` as usual.
//!   5. [`detach_loop_device`] (`LOOP_CLR_FD`) is available for the
//!      cases where we want to release the binding, even though the
//!      rescue path normally never unwinds.
//!
//! Project rule: minimize unsafe. The opcodes are flat legacy numbers
//! (no `_IOR`/`_IOW` direction bits) so we drive `rustix::ioctl` with
//! `BadOpcode` — every unsafe block has a SAFETY comment per the
//! convention set in `sys::kexec`.

mod ops;
mod types;

// Re-export the public API at the original path so callers are unchanged.
pub use ops::{allocate_loop_device, configure_loop_device, detach_loop_device, open_loop_device};
pub use types::{
    LO_FLAGS_READ_ONLY, LO_KEY_SIZE, LO_NAME_SIZE, LOOP_CLR_FD, LOOP_CONFIGURE, LOOP_CONTROL_PATH,
    LOOP_CTL_GET_FREE, LoopConfig, LoopInfo64,
};
