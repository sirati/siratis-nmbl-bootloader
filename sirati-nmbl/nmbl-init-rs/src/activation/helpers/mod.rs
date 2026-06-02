//! Small utility helpers for the activation orchestrator.
//!
//! Split across submodules: source-device readiness waits
//! ([`source_wait`]), required-/loaded-modules state ([`modules_state`]),
//! exit-code classification and error wrapping ([`exit_codes`]), and
//! passphrase/stdin collection ([`stdin`]). The same `pub(crate)` API the
//! rest of the crate consumed before the split is re-exported here.

mod exit_codes;
mod modules_state;
mod source_wait;
mod stdin;

pub(crate) use exit_codes::{
    exit_code_error, is_activation_success, kind_label, wrap_runner_error,
};
pub(crate) use modules_state::{check_required_modules, loaded_modules};
pub(crate) use source_wait::wait_for_source_device;
pub(crate) use stdin::collect_stdin;
