//! Inter-process transport for the remote TUI.
//!
//! Gated behind the `remote-tui` feature. A non-PID-1 invocation of the
//! binary connects to PID 1's Unix socket, passes its controlling
//! terminal across via `SCM_RIGHTS`, and goes quiescent while PID 1
//! drives the operator's terminal directly. PID 1 owns the accept loop
//! (Phase 3); this module only provides the transport building blocks
//! (`tui_socket::bind_listener`, `tui_socket::authenticate_and_receive`)
//! and the synchronous client (`tui_socket::connect_and_serve`).

#[cfg(feature = "remote-tui")]
pub mod tui_socket;
