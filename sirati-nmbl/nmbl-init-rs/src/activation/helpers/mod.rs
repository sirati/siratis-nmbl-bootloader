//! Small utility helpers for the activation orchestrator.
//!
//! Split across submodules: required-/loaded-modules state
//! ([`modules_state`]), exit-code classification and error wrapping
//! ([`exit_codes`]), and passphrase/stdin collection ([`stdin`]). The
//! source-device readiness wait lives in the sibling `activation::source_wait`
//! module (the ops-threaded `<S: SysOps>` version the orchestrator uses). The
//! same `pub(crate)` API the rest of the crate consumed before the split is
//! re-exported here.

mod exit_codes;
mod modules_state;
mod stdin;

pub(crate) use exit_codes::{
    exit_code_error, is_activation_success, kind_label, wrap_runner_error,
};
pub(crate) use modules_state::{check_required_modules, loaded_modules};
pub(crate) use stdin::collect_stdin;
