# Single source of truth for NMBL's secure/staged-boot security defaults and
# for the ONE `secureBootActive` implication boolean (FIX-16 / R-cfg).
#
# This file is the Nix mirror of the Rust `pub const`s in
# `nmbl-init-rs/src/sig`, `…/src/tpm`, and `…/src/policy`. The values here are
# round-trip-tested against those consts (see `config/tests/security_consts.rs`,
# added alongside the Rust side) so the two can never silently drift.
#
# Why a single file:
#   * The `secureBootActive` boolean is the IMPLICATION "any per-table
#     security emit-gate ⇒ `secure-boot` ∈ nmblFeatures" (FIX-16). It must be
#     derived in ONE place and imported by BOTH the `nmblFeatures` derive
#     (lib/signing-build.nix) AND, later, the per-group `optionalAttrs` emit
#     gates (lib/config-toml.nix). Defining it once here keeps those callers
#     from drifting.
#   * The security defaults (refuse countdown, relock poison pre-image, lock
#     PCR index, sentinel path) are referenced by the per-slice modules
#     (#6/#7/#10) and the install/relock plumbing. Pinning them here — not at
#     each option's `default =` — means the Rust round-trip test guards ONE
#     definition.
#
# Usage:
#   let sc = import ./security-consts.nix { inherit lib; };
#   in  sc.mkSecureBootActive config        # -> bool
#       sc.defaults.refuseCountdownSeconds   # -> 30
#       sc.defaults.lockPcr                  # -> 11
#       sc.defaults.sentinelPath             # -> "/boot/nmbl/rescue"
#       sc.defaults.relockPoisonPreimage     # -> "nmbl:relock-poison:v1"

{ lib }:

let
  # ───────────────────────── security defaults ─────────────────────────
  #
  # Mirror of the Rust consts. Keep these in lockstep with:
  #   tpm/mod.rs       LOCK_PCR (= lockPcr), RELOCK_POISON pre-image
  #   secure_boot.rs   refuse_countdown_seconds default
  #   policy/sentinel  sentinel path default
  defaults = {
    # `boot.nmbl.secureBoot.refuseCountdownSeconds` default. The refuse
    # screen is a non-interactive countdown; 30 s before auto-reboot
    # (R-13 / FIX-39). The ONLY path — `policy.*` is superseded.
    refuseCountdownSeconds = 30;

    # PCR the measured-boot path caps to poison TPM-sealed secrets on a
    # refuse (R-2 / FIX-38). Mirrors `tpm::LOCK_PCR`.
    lockPcr = 11;

    # Domain-separated pre-image hashed into the relock poison value:
    # `sha256(b"nmbl:relock-poison:v1")` (FIX-38). The Rust side derives
    # `RELOCK_POISON` from this exact byte string and self-checks it; the
    # round-trip test asserts the two pre-images match.
    relockPoisonPreimage = "nmbl:relock-poison:v1";

    # Sentinel file whose presence forces a rescue boot and keeps the TPM
    # capped (FIX-38 / FIX-21). Lives on the writable boot FS. Mirrors the
    # Rust sentinel-path default in `policy/sentinel.rs`.
    sentinelPath = "/boot/nmbl/rescue";
  };

  # ──────────────────────── secureBootActive ──────────────────────────
  #
  # The ONE implication boolean. `mkSecureBootActive config` is true when
  # ANY security table is enabled. It is consumed by the `nmblFeatures`
  # derive (to OR in the `secure-boot` Cargo feature) and, later, by the
  # per-group emit gates — so all callers share ONE definition.
  #
  # SELF-REGISTRATION ANCHOR (FIX-60): the options these read
  # (`signing.enable`, `tpm.measure`, `secureBoot.enable`) are contributed
  # by the later slices #6/#7/#10 via `lib/modules/security/*.nix`. Until
  # those land, the `or false` fallbacks keep this file evaluable in the
  # skeleton (#5p) without a hard eval error. Each slice, when it adds its
  # option, ALSO appends one OR-term to `activeTerms` below at the marked
  # anchor — a one-line, conflict-free edit per slice (no shared boolean
  # expression is rewritten in multiple files).
  #
  # FIX-57 note: once #6/#7/#10 have landed their modules the `or false`
  # fallbacks become dead (the options always exist) and the F1 exit gate
  # tightens them to direct references. They are deliberate scaffolding for
  # the #5p→#6/#7/#10 window ONLY.
  mkSecureBootActive =
    config:
    let
      cfg = config.boot.nmbl;
      # ┌──────────────────── secureBootActive anchor ────────────────────┐
      # │ Each security slice appends ONE term here (no other file edit).  │
      # │   #6 signing  : (cfg.signing.enable    or false)                 │
      # │   #7 tpm      : (cfg.tpm.measure        or false)                │
      # │   #10 secureB : (cfg.secureBoot.enable  or false)                │
      # └─────────────────────────────────────────────────────────────────┘
      activeTerms = [
        (cfg.signing.enable or false)
        (cfg.tpm.measure or false)
        (cfg.secureBoot.enable or false)
      ];
    in
    lib.foldl' (a: b: a || b) false activeTerms;

  # ──────────────────────── stagedBootActive ──────────────────────────
  #
  # The ONE emit/feature gate for the staged-boot slice (#9). True exactly
  # when the operator enabled `boot.nmbl.staged.enable`. Consumed by:
  #   * the `nmblFeatures` derive (lib/signing-build.nix) — to OR in the
  #     `staged-boot` Cargo feature, and
  #   * BOTH staged emit gates (lib/config-toml.nix `[staged]` and
  #     lib/bootstrap-toml.nix `[bootstrap.staged]`),
  # so the Nix tables are emitted under the SAME boolean as the Rust
  # `#[cfg(feature = "staged-boot")]` — a feature-free binary never sees a
  # table it cannot parse (FIX-40).
  #
  # `staged-boot` structurally implies `secure-boot`, so a true value here
  # also makes `secureBootActive` true at the Cargo level via the feature
  # graph; the `staged.enable ⇒ secureBoot.enable` operator-config rule is
  # enforced as a `--validate-config` assertion in the staged module
  # (FIX-26), not here.
  #
  # The `or false` fallback keeps this evaluable in the #5p skeleton before
  # the staged module lands its `boot.nmbl.staged.enable` option (FIX-57
  # scaffolding for the same window as `mkSecureBootActive`).
  mkStagedBootActive = config: (config.boot.nmbl.staged.enable or false);
in
{
  inherit defaults mkSecureBootActive mkStagedBootActive;
}
