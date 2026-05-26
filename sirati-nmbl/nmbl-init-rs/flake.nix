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
          src = craneLib.cleanCargoSource ./.;
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

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        nmbl-init = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            pname = "nmbl-init";
          }
        );
      in
      {
        packages = {
          default = nmbl-init;
          nmbl-init = nmbl-init;
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
        };

        checks = {
          inherit nmbl-init;

          nmbl-init-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          nmbl-init-fmt = craneLib.cargoFmt {
            src = commonArgs.src;
          };

          # Enforce that the only sites that may exec a new process or
          # invoke std::process::Command are the three allow-listed
          # files: the emergency-shell exec site, the panic-recovery
          # re-exec site, and the activation-runner fork/exec helper.
          nmbl-init-no-exec = pkgs.runCommand "nmbl-init-no-exec" { } ''
            cd ${./.}
            if grep -RIn -E '\bCommand::|\bexecve\(' src/ \
                 | grep -v -E '^src/(shell\.rs|panic\.rs|sys/activation\.rs)'; then
              echo "ERROR: Command:: or execve() found outside allowlisted files"
              exit 1
            fi
            touch $out
          '';
        };
      }
    );
}
