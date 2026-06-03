pub mod activation;
pub mod boot;
pub mod config;
pub mod devices;
pub mod error;
pub mod generations;
#[cfg(feature = "remote-tui")]
pub mod ipc;
pub mod log;
#[cfg(feature = "mocking")]
pub mod mocking;
pub mod modules;
pub mod mount;
#[cfg(feature = "network-rescue")]
pub mod net;
pub mod panic;
pub mod rescue;
/// Single-source security defaults mirrored by `lib/security-consts.nix`.
/// Always compiled so the Nix↔Rust round-trip test runs in every build.
pub mod security_consts;
pub mod shell;
/// Signature-verification subsystem (secure/staged-boot). The module is
/// ALWAYS compiled because its `wire` leaf — the sidecar wire format shared
/// byte-for-byte with the host signer — must link in the default build
/// (FIX-25). Everything else inside (`alg`/`sidecar`/the verify facade) is
/// `#[cfg(feature = "secure-boot")]` and carries the frozen #12 API; the real
/// verify bodies land in #14/#15.
pub mod sig;
#[cfg(feature = "image-splash")]
pub mod splash;
#[cfg(feature = "stateful")]
pub mod state;
pub mod sys;
pub mod terminal;
/// TPM 2.0 cap/transport/presence core. ALWAYS compiled (no `secure-boot`
/// gate — FIX-09): the lock-on-rescue guard's PCR-cap and the deterministic
/// `/dev/tpmrm0` presence latch are needed even when `secure-boot` is OFF.
/// The cost is the non-optional, pure-Rust `tpm2-protocol` dep; the
/// `secure-boot`-only measure path (`tpm/measure.rs`) lands later, gated.
pub mod tpm;
pub mod ui;
/// Small shared primitives. `util::hex` is ungated; `util::hash` is gated
/// `any(network-rescue, secure-boot)` (it `use`s the optional `sha2` dep).
pub mod util;
pub mod validate;
