{
  description = "nmbl-init — Rust /init for the NixOS Minimal BootLoader (static musl, -Oz)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      crane,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # The init must be a fully static binary so the initramfs has zero
        # runtime ELF dependencies. musl + +crt-static is the canonical recipe.
        rustTarget = "x86_64-unknown-linux-musl";

        # Pin a single stable toolchain version; production-grade builds want
        # reproducibility, not whatever stable happens to be on the day.
        rustToolchain = fenix.packages.${system}.combine [
          (fenix.packages.${system}.stable.withComponents [
            "cargo"
            "rustc"
            "rust-src"
            "rustfmt"
            "clippy"
          ])
          fenix.packages.${system}.targets.${rustTarget}.stable.rust-std
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Compiler/linker flags shared between buildDepsOnly and buildPackage.
        # -Oz, fat LTO, single codegen unit, strip — all for minimum image size.
        commonArgs = {
          # `cleanCargoSource` keeps only Rust/Cargo files, which drops the
          # binary blobs bundled via `include_bytes!`:
          #   * the compiled terminfo entry (`src/ui/console/data/xterm-256color`)
          #     — without `cup` from it termwiz transposes row/col on every
          #     incremental serial repaint, and
          #   * the embedded splash fallback font (`src/splash/data/`) used when
          #     the configured font fails to load.
          # Union them back in so the crate compiles.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              (craneLib.fileset.commonCargoSources ./.)
              ./src/ui/console/data/xterm-256color
              ./src/splash/data
            ];
          };
          strictDeps = true;

          CARGO_BUILD_TARGET = rustTarget;

          # +crt-static forces a fully-static binary linked against musl-libc.
          # No runtime DSO lookup → safe inside an initramfs with no /lib*.
          CARGO_BUILD_RUSTFLAGS = builtins.concatStringsSep " " [
            "-C target-feature=+crt-static"
            "-C link-arg=-s"
            "-C relocation-model=static"
            # rustix uses the linux_raw backend by default on linux-musl;
            # `linux_latest` skips its pre-5.x kernel fallbacks. NixOS
            # always runs a recent kernel so this is safe.
            "--cfg linux_latest"
            "--check-cfg cfg(linux_latest)"
          ];

          # We don't need anything from the host beyond the toolchain itself.
          # Anything we list here ends up in the initramfs only if config.nix
          # explicitly pulls it in — the binary itself stays self-contained.
          nativeBuildInputs = [ ];
          buildInputs = [ ];

          # No tests or doc-tests in initramfs builds; keep CI fast.
          doCheck = false;
        };

        # Generate the `src/sig/baked_keys.rs` trust anchor + the per-key
        # `baked_key_N.bin` blobs from a list of public keys, and union them
        # into the crane source (R-5/FIX-24). Each `publicKeys` element is
        #   { path = <store path to the raw encoded ML-DSA public key>;
        #     alg  = "MlDsa65" | "MlDsa87"; }
        # The committed `src/sig/baked_keys.rs` is the EMPTY default; this
        # overlays a generated copy + the blobs ONLY when keys are supplied,
        # so the default build stays cache-hot (FIX-63).
        #
        # `requireKeys` flips the generated `REQUIRE_KEYS` const: signature
        # ENFORCEMENT builds set it `true` (an empty set is then a Rust
        # compile error AND a Nix assert), while measure-only builds
        # (secure-boot feature but no signing enforcement) keep it `false` so
        # they legitimately build with no keys (FIX-24).
        mkBakedKeysSrc = { publicKeys, requireKeys }:
          let
            algByte = alg:
              if alg == "MlDsa65" then "AlgId::MlDsa65"
              else if alg == "MlDsa87" then "AlgId::MlDsa87"
              else throw "mkNmblInit: unknown baked-key algorithm '${alg}' (expected MlDsa65|MlDsa87)";

            # One `include_bytes!` entry per key, paired with its AlgId.
            entries = pkgs.lib.imap0 (i: k:
              "    (include_bytes!(\"baked_key_${toString i}.bin\"), ${algByte k.alg}),"
            ) publicKeys;

            bakedKeysRs = pkgs.writeText "baked_keys.rs" ''
              //! GENERATED by flake.nix `mkBakedKeysSrc` (R-5/FIX-24). Do not edit:
              //! the committed `src/sig/baked_keys.rs` is the EMPTY stub; this file
              //! overlays it when `boot.nmbl.signing.publicKeys` are configured.

              use super::alg::AlgId;

              /// Baked public-key trust anchor: `(raw encoded public-key bytes, alg)`.
              pub(crate) static BAKED_KEYS: &[(&[u8], AlgId)] = &[
              ${builtins.concatStringsSep "\n" entries}
              ];

              /// Whether at least one baked key is MANDATORY for this build (FIX-24).
              pub const REQUIRE_KEYS: bool = ${if requireKeys then "true" else "false"};
            '';

            # The per-key `.bin` blobs, named to match the `include_bytes!`
            # paths above, dropped into a directory crane can union in.
            blobsDir = pkgs.runCommand "nmbl-baked-key-blobs" { } (''
              mkdir -p $out
              cp ${bakedKeysRs} $out/baked_keys.rs
            '' + builtins.concatStringsSep "\n" (pkgs.lib.imap0 (i: k:
              "cp ${k.path} $out/baked_key_${toString i}.bin"
            ) publicKeys));
          in
          blobsDir;

        # Builder so callers (sirati-nmbl/flake.nix) can request a build
        # with optional Cargo features (e.g. `network-rescue`) and an optional
        # baked-key trust anchor. The default build below passes
        # `features = []` / `publicKeys = []`, so it stays byte-identical to
        # the feature-free build.
        mkNmblInit = { features ? [ ], publicKeys ? [ ], requireKeys ? null }:
          let
            hasSecureBoot = builtins.elem "secure-boot" features;
            # Default `requireKeys` to the literal "secure-boot ∈ features"
            # implication (FIX-24 task text); callers that distinguish
            # measure-only from enforcement override it explicitly.
            requireKeys' = if requireKeys == null then hasSecureBoot else requireKeys;

            # FIX-24: a secure-boot ENFORCEMENT build with no public keys is a
            # deterministic eval-time error — never a runtime allow-all.
            keysOk = pkgs.lib.assertMsg
              (!requireKeys' || publicKeys != [ ])
              "nmbl-init: secure-boot signature enforcement requires a non-empty boot.nmbl.signing.publicKeys (an empty baked-key set would be a runtime allow-all)";

            featureArgs =
              if features == [ ] then
                ""
              else
                "--features=" + builtins.concatStringsSep "," features;

            # Generated baked-keys source + blobs, materialised once.
            bakedBlobs = mkBakedKeysSrc { inherit publicKeys; requireKeys = requireKeys'; };

            # Source with the generated baked_keys.rs + per-key blobs overlaid
            # at their pinned `src/sig/` paths. crane's `cleanCargoSource`
            # drops `include_bytes!` blobs, so they MUST be unioned back in
            # (the terminfo/font precedent — FIX-24). Built as a derivation
            # that copies the cleaned source then overlays the generated files,
            # so the result is a clean store path crane can build from.
            srcWithKeys = pkgs.runCommand "nmbl-init-src-with-keys" { } ''
              cp -r --no-preserve=mode ${commonArgs.src} $out
              cp ${bakedBlobs}/baked_keys.rs $out/src/sig/baked_keys.rs
              ${builtins.concatStringsSep "\n" (pkgs.lib.imap0 (i: _:
                "cp ${bakedBlobs}/baked_key_${toString i}.bin $out/src/sig/baked_key_${toString i}.bin"
              ) publicKeys)}
            '';

            # `-p nmbl-init` scopes the build to the init package only: the
            # workspace now also holds `nmbl-host-tools` (the HOST signer), which
            # MUST NOT enter the musl initramfs build/closure (FIX-25). Feature
            # flags follow the package selector.
            featureArgs' =
              if featureArgs == "" then "-p nmbl-init" else "-p nmbl-init " + featureArgs;
            argsWithFeatures = commonArgs // {
              cargoExtraArgs = featureArgs';
            } // pkgs.lib.optionalAttrs (publicKeys != [ ]) {
              src = srcWithKeys;
            };
            # Deps-only build never needs the baked blobs (they are crate
            # source, not deps); keep it on the base src so its cache is shared
            # across keyed and keyless builds.
            artifacts = craneLib.buildDepsOnly (
              argsWithFeatures // { src = commonArgs.src; }
            );
          in
          assert keysOk;
          craneLib.buildPackage (
            argsWithFeatures
            // {
              cargoArtifacts = artifacts;
              pname = "nmbl-init";
            }
          );

        # `-p nmbl-init` keeps every initramfs build scoped to the init package;
        # the sibling `nmbl-host-tools` workspace member (the host signer) is
        # built separately below for the HOST target and never enters this
        # musl/initramfs closure (FIX-25).
        initArgs = commonArgs // {
          cargoExtraArgs = "-p nmbl-init";
        };

        cargoArtifacts = craneLib.buildDepsOnly initArgs;

        nmbl-init = craneLib.buildPackage (
          initArgs
          // {
            inherit cargoArtifacts;
            pname = "nmbl-init";
          }
        );

        # Same crate built with the optional `image-splash` cargo feature
        # (drm + png + fontdue + alacritty_terminal). Kept as a separate
        # package so users who don't want the splash never pull those deps.
        nmbl-init-splash = craneLib.buildPackage (
          initArgs
          // {
            inherit cargoArtifacts;
            pname = "nmbl-init-splash";
            cargoExtraArgs = "-p nmbl-init --features image-splash";
          }
        );

        # ---- Host-tools: the `nmbl-sign` signer (HOST target — FIX-25) --------
        #
        # A SEPARATE crane buildPackage for the host platform (NOT musl): it
        # produces the NMBLSIG1 sidecars the in-initramfs verifier checks, so it
        # must run on the operator's build host. It reuses `nmbl_init::sig`'s
        # frozen wire format via a path dep, but is built OUTSIDE the initramfs
        # `cargoArtifacts` and never appears in any nmbl-init/UKI/initramfs
        # closure. The host triple drops the `+crt-static`/musl rustflags; the
        # `.cargo/config.toml` musl default is overridden by `CARGO_BUILD_TARGET`.
        hostTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
        hostToolchain = fenix.packages.${system}.combine [
          (fenix.packages.${system}.stable.withComponents [
            "cargo"
            "rustc"
            "rustfmt"
            "clippy"
          ])
          fenix.packages.${system}.targets.${hostTarget}.stable.rust-std
        ];
        hostCraneLib = (crane.mkLib pkgs).overrideToolchain hostToolchain;
        hostCommonArgs = {
          src = commonArgs.src;
          strictDeps = true;
          cargoExtraArgs = "-p nmbl-host-tools";
          CARGO_BUILD_TARGET = hostTarget;
          # No musl/static rustflags here: this is a normal dynamic host binary.
          CARGO_BUILD_RUSTFLAGS = "";
          doCheck = false;
        };
        hostArtifacts = hostCraneLib.buildDepsOnly hostCommonArgs;
        nmbl-sign = hostCraneLib.buildPackage (
          hostCommonArgs
          // {
            cargoArtifacts = hostArtifacts;
            pname = "nmbl-sign";
          }
        );
      in
      {
        # Function form: callers wire Cargo features through this
        # (sirati-nmbl/flake.nix uses it to gate `network-rescue` on
        # `boot.nmbl.rescue.network`).
        legacyPackages.mkNmblInit = mkNmblInit;

        packages = {
          default = nmbl-init;
          nmbl-init = nmbl-init;
          nmbl-init-splash = nmbl-init-splash;
          # The host-platform ML-DSA image signer (FIX-25). A separate
          # buildPackage on the host target, outside the initramfs closure.
          nmbl-sign = nmbl-sign;
        };

        # Useful for hand-testing: just runs the binary in your shell. It will
        # refuse to do anything serious unless it sees a config file, so this
        # is only really for `nmbl-init --help` smoke checks.
        apps.default = {
          type = "app";
          program = "${nmbl-init}/bin/nmbl-init";
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ nmbl-init ];

          packages = with pkgs; [
            rust-analyzer
            cargo-edit
            cargo-watch
            cargo-nextest
            cargo-deny
            cargo-bloat
            # For sanity-checking the static linkage of the produced binary.
            file
            patchelf
          ];

          # Mirror the build target so `cargo check` / `clippy` give the same
          # diagnostics as the Nix build.
          env = {
            CARGO_BUILD_TARGET = rustTarget;
          };

          # Font path for splash::glyph_cache tests. Resolved at compile
          # time via `option_env!`; tests skip cleanly if the variable is
          # unset (e.g. when building outside the dev shell), so we never
          # need to vendor the font itself into the repo.
          shellHook = ''
            export NMBL_TEST_FONT="${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf"
          '';
        };

        checks = {
          inherit nmbl-init;

          # REQUIRED F2 gate (R-5/FIX-24): a secure-boot ENFORCEMENT build with
          # an empty `publicKeys` MUST be rejected at eval time, never produce a
          # runtime allow-all. `tryEval` captures the `assertMsg` failure so the
          # check passes precisely WHEN the empty-keys build is refused, and
          # fails loudly if the guard ever regresses to accepting it.
          zero-keys-rejected =
            let
              attempt = builtins.tryEval (
                mkNmblInit {
                  features = [ "secure-boot" ];
                  publicKeys = [ ];
                  requireKeys = true;
                }
              );
            in
            pkgs.runCommand "nmbl-init-zero-keys-rejected" { } ''
              ${if attempt.success then ''
                echo "FAIL: secure-boot enforcement build with empty publicKeys was ACCEPTED"
                echo "      (mkNmblInit must reject a zero-key enforcement build — FIX-24)"
                exit 1
              '' else ''
                echo "OK: empty-keys secure-boot enforcement build is rejected at eval (FIX-24)"
                touch $out
              ''}
            '';

          nmbl-init-clippy = craneLib.cargoClippy (
            initArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "-p nmbl-init --all-targets -- --deny warnings";
            }
          );

          # `cargo fmt --check` over the whole source tree (covers both the
          # init crate and the host-tools member).
          nmbl-init-fmt = craneLib.cargoFmt {
            src = commonArgs.src;
          };

          # ---- Host-tools (nmbl-sign) gate: build + clippy + test (FIX-25) ----
          # The signer is built/linted/tested under the HOST target (clippy
          # 1.95.0 -D warnings via the pinned fenix toolchain), separate from the
          # musl initramfs gate.
          inherit nmbl-sign;

          nmbl-sign-clippy = hostCraneLib.cargoClippy (
            hostCommonArgs
            // {
              cargoArtifacts = hostArtifacts;
              cargoClippyExtraArgs = "-p nmbl-host-tools --all-targets -- --deny warnings";
            }
          );

          # The cross-crate round-trip KAT (FIX-25/FIX-52): `nmbl-sign` signs and
          # `nmbl_init::sig::verify_digest` verifies on the same bytes, plus the
          # wrong-key / wrong-domain / truncated-sidecar negatives. Run on the
          # host target so both crates' code executes natively.
          nmbl-sign-test = hostCraneLib.cargoTest (
            hostCommonArgs
            // {
              cargoArtifacts = hostArtifacts;
              cargoExtraArgs = "-p nmbl-host-tools";
              # `hostCommonArgs` sets `doCheck = false` for the build/clippy
              # derivations; the test check MUST actually RUN the KATs, so flip
              # it back on here (else the test binary is built but never run).
              doCheck = true;
            }
          );

          # Replacing our own process (execve) or spawning one
          # (std::process::Command) is only sound at a handful of sites:
          # the single PID1 handoff, post-fork children, the panic
          # re-exec. Instead of an allow-list of whole files, require an
          # in-context justification on (or directly above) every real
          # use — a `// execve safety: <why>` comment, mirroring the
          # `// SAFETY:` convention for `unsafe`. Lines that are
          # themselves comments (e.g. doc-comments mentioning `execve(2)`)
          # are ignored, so prose about exec never trips the guard.
          nmbl-init-no-exec = pkgs.runCommand "nmbl-init-no-exec" {
            nativeBuildInputs = [ pkgs.gawk ];
          } ''
            cd ${./.}
            rc=0
            for f in $(find src -name '*.rs'); do
              awk -v file="$f" '
                ($0 ~ /\<execve\(/ || $0 ~ /\<Command::/) && $0 !~ /^[[:space:]]*\/\// {
                  if (prev !~ /execve safety:/ && $0 !~ /execve safety:/) {
                    printf "%s:%d:%s\n", file, FNR, $0
                    bad = 1
                  }
                }
                { prev = $0 }
                END { exit bad }
              ' "$f" || rc=1
            done
            if [ "$rc" -ne 0 ]; then
              echo "ERROR: every execve()/Command:: needs a '// execve safety: <why>' comment on or directly above it"
              exit 1
            fi
            touch $out
          '';

          # The single MOST security-critical invariant: no interactive
          # context (shell / PTY) is reachable while the lock PCR is
          # uncappable or a TPM-unsealed LUKS mapper is still live. Every
          # shell-fork waist (`spawn_shell` / `spawn_shell_on_tty`) MUST be
          # preceded — within its enclosing function — by the `Sealed`
          # witness that `policy::seal_secrets` mints (cap PCR + close
          # mappers), OR carry a `// seal-exempt: <why>` justification
          # (mirroring `// execve safety:`). A `Sealed` parameter on the
          # function, a `seal_secrets` call, or the witness binding all
          # satisfy the check; a bare spawn with neither fails CI. The
          # type system is the real guarantee (the shell-spawn helpers
          # require a `Sealed` by value); this is the machine-checked
          # belt-and-suspenders that catches a new unguarded waist (FIX-29
          # / re-audit C-1).
          nmbl-init-must-seal = pkgs.runCommand "nmbl-init-must-seal" {
            nativeBuildInputs = [ pkgs.gawk ];
          } ''
            cd ${./.}
            rc=0
            for f in $(find src -name '*.rs'); do
              awk -v file="$f" '
                # The witness must live in the SAME function body as the
                # fork: a `Sealed`/`seal_secrets` token in a doc-comment or
                # in an unrelated function 17 lines up must NOT satisfy a
                # spawn site. We reset the witness flag at every `fn`
                # definition, set it only on NON-comment witness lines
                # (`// seal-exempt:` is the one comment that counts), and
                # require it at each real spawn waist.
                function is_comment(line) {
                  return (line ~ /^[[:space:]]*\/\//)
                }
                # A new function definition starts a fresh body: anything
                # the previous fn proved no longer applies.
                /\<fn[[:space:]]+[A-Za-z_]/ { witness = 0 }
                # Record a witness ONLY from real code: a `Sealed` param /
                # binding or a `seal_secrets` call on a non-comment line.
                # An explicit `// seal-exempt:` justification is the single
                # comment form that also counts.
                (!is_comment($0) && ($0 ~ /seal_secrets/ || $0 ~ /Sealed/)) \
                  || ($0 ~ /seal-exempt:/) {
                  witness = 1
                }
                # A shell-fork waist: the two PTY shell-spawn primitives.
                # Skip comment lines (doc-comments that merely mention the
                # primitive) and their own definitions (`fn spawn_shell`).
                ($0 ~ /\<spawn_shell(_on_tty)?\(/) \
                  && !is_comment($0) \
                  && $0 !~ /\<fn[[:space:]]+spawn_shell/ {
                  if (!witness) {
                    printf "%s:%d:%s\n", file, FNR, $0
                    bad = 1
                  }
                }
                END { exit bad }
              ' "$f" || rc=1
            done
            if [ "$rc" -ne 0 ]; then
              echo "ERROR: every shell-fork (spawn_shell / spawn_shell_on_tty) must be preceded — within its enclosing function — by the Sealed witness from policy::seal_secrets (cap PCR + close TPM-unsealed mappers) or carry a '// seal-exempt: <why>' justification"
              exit 1
            fi
            touch $out
          '';

          # The no-bypass safety net (FIX-53/FIX-38): reaching recovery/rescue
          # must NEVER bypass the TPM cap, and a boot must NEVER proceed past a
          # signature gate without either a real verify or an EXPLICIT,
          # operator-opted, justified relaxation. There is a small, enumerable
          # set of legitimate sites where the cap is vacuous (no TPM to cap) or
          # the verify is intentionally skipped/downgraded (signing disabled,
          # or audit mode behind `allowAuditModeInsecure`). Each such site MUST
          # carry — on the line or directly above — a `// cap-exempt: <why>`
          # (a cap relaxation) or a `// signing safety: <why>` (a verify
          # skip/downgrade) justification, mirroring the `// seal-exempt:` /
          # `// execve safety:` conventions. A NEW bypass added without the
          # comment fails CI: the reviewer is forced to name a reason, and the
          # comment is the audit trail. Lines that are themselves comments
          # (prose mentioning these tokens) never trip the guard.
          #
          # The matched bypass shapes:
          #   * `if !config.signing.enable` / `if !config.secure_boot.enable`
          #       — the skip-verify short-circuit (signing/secure-boot off).
          #   * `VerifyPolicy::Audit =>`
          #       — the audit-mode downgrade arm (a failed verify proceeds).
          #   * `GateDecision::AuditProceed(` WITHOUT `=>` on the line
          #       — the priority-gate audit-proceed PRODUCERS (the `=>` match
          #         arm is the consumer, not a bypass; the enum decl has no
          #         `GateDecision::` prefix).
          #   * `CapOutcome::NoTpm =>` whose RHS is NOT another `CapOutcome::`
          #       — a NoTpm cap-degrade reaching `Ok`/the require-tpm branch
          #         (an identity remap `=> CapOutcome::NoTpm` is not a bypass).
          nmbl-init-no-cap-bypass = pkgs.runCommand "nmbl-init-no-cap-bypass" {
            nativeBuildInputs = [ pkgs.gawk ];
          } ''
            cd ${./.}
            rc=0
            for f in $(find src -name '*.rs'); do
              awk -v file="$f" '
                function is_comment(line) {
                  return (line ~ /^[[:space:]]*\/\//)
                }
                # `tok` is a STRING pattern (not a regex literal — passing /…/
                # to a function yields a boolean). A justification is in scope
                # when the token is on THIS line OR anywhere in the contiguous
                # comment block immediately above (accumulated into `just`).
                function need(tok, kind) {
                  if ($0 ~ tok || just ~ tok) { return 0 }
                  printf "%s:%d: %s without justification — %s\n", file, FNR, kind, $0
                  return 1
                }
                # Accumulate a contiguous run of comment lines so a multi-line
                # justification block above the anchor counts.
                { if (is_comment($0)) { just = just "\n" $0 } }
                # ── Verify skip / downgrade sites: need `// signing safety:` ──
                # signing/secure-boot disabled short-circuit.
                (!is_comment($0) \
                  && ($0 ~ /if[[:space:]]+!config\.signing\.enable/ \
                      || $0 ~ /if[[:space:]]+!config\.secure_boot\.enable/)) {
                  if (need("signing safety:", "verify-skip")) { bad = 1 }
                }
                # Audit-mode downgrade arm.
                (!is_comment($0) && $0 ~ /VerifyPolicy::Audit[[:space:]]*=>/) {
                  if (need("signing safety:", "audit-downgrade")) { bad = 1 }
                }
                # Priority-gate audit-proceed PRODUCER (construction, not the
                # `=>` consumer arm).
                (!is_comment($0) && $0 ~ /GateDecision::AuditProceed\(/ && $0 !~ /=>/) {
                  if (need("signing safety:", "audit-proceed")) { bad = 1 }
                }
                # ── Cap relaxation sites: need `// cap-exempt:` ──────────────
                # A NoTpm degrade arm reaching Ok / the require-tpm branch; an
                # identity remap (`=> CapOutcome::`) is not a relaxation.
                (!is_comment($0) && $0 ~ /CapOutcome::NoTpm[[:space:]]*=>/ \
                  && $0 !~ /=>[[:space:]]*CapOutcome::/) {
                  if (need("cap-exempt:", "cap-degrade")) { bad = 1 }
                }
                # Reset the comment context once a non-comment line passes the
                # rules above, so the next anchor only sees ITS own block.
                { if (!is_comment($0)) { just = "" } }
                END { exit bad }
              ' "$f" || rc=1
            done
            if [ "$rc" -ne 0 ]; then
              echo "ERROR: every TPM-cap relaxation or signature skip/downgrade must carry a '// cap-exempt: <why>' or '// signing safety: <why>' justification on or directly above it (the no-bypass checklist — FIX-53). Reaching recovery/rescue must never silently bypass the cap, and a boot must never proceed past a verify gate without an explicit, justified relaxation."
              exit 1
            fi
            touch $out
          '';

          # Pinned security-const defaults (FIX-38). The Rust `pub const`s in
          # `src/security_consts.rs` are the single source the Nix mirror
          # (`lib/security-consts.nix`) is round-trip-tested against by the
          # `security_consts::tests::nmbl_init_security_consts_match_nix` cargo
          # test (which reads the actual Nix file). That cargo test is skipped
          # inside `nix flake check` (which sets `doCheck = false`) AND in the
          # sandbox where the parent `lib/` is out of source, so this check is
          # the flake-check-time half: it greps the literals straight out of the
          # Rust source and asserts the pinned values byte-for-byte. A silent
          # change to a security default here fails the flake check even with
          # tests disabled; the cargo test then catches a Nix-side drift.
          nmbl-init-security-consts = pkgs.runCommand "nmbl-init-security-consts" {
            nativeBuildInputs = [ pkgs.gnugrep ];
          } ''
            cd ${./.}
            f=src/security_consts.rs
            fail() { echo "FAIL (FIX-38): $1"; exit 1; }

            grep -qF 'pub const LOCK_PCR: u32 = 11;' "$f" \
              || fail "LOCK_PCR must be pinned to 11 (security-consts.nix defaults.lockPcr)"
            grep -qF 'pub const RELOCK_POISON_PREIMAGE: &[u8] = b"nmbl:relock-poison:v1";' "$f" \
              || fail "RELOCK_POISON_PREIMAGE must be b\"nmbl:relock-poison:v1\" (defaults.relockPoisonPreimage)"
            grep -qF 'pub const REFUSE_COUNTDOWN_SECONDS: u32 = 30;' "$f" \
              || fail "REFUSE_COUNTDOWN_SECONDS must be 30 (defaults.refuseCountdownSeconds)"
            grep -qF 'pub const SENTINEL_PATH: &str = "/boot/nmbl/rescue";' "$f" \
              || fail "SENTINEL_PATH must be \"/boot/nmbl/rescue\" (defaults.sentinelPath)"

            echo "OK: security-const defaults are pinned (LOCK_PCR=11, poison preimage, countdown=30, sentinel /boot/nmbl/rescue)"
            touch $out
          '';
        };
      }
    );
}
