//! Network primitives used by the rescue HTTP fallback.
//! Gated behind the `network-rescue` Cargo feature; zero bytes
//! ship when the feature is off.
pub mod dhcp;
pub mod http;
pub mod iface;
