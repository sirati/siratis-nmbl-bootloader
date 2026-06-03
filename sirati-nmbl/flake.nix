{
  description = "Linux-as-bootloader (NMBL-style) for NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    nmbl-init-rs = {
      url = "path:./nmbl-init-rs";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    nixos-anywhere = {
      url = "github:nix-community/nixos-anywhere";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.disko.follows = "disko";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nmbl-init-rs,
      disko,
      nixos-anywhere,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # Import testing utilities
      testing = import ./testing/build_configurations.nix { inherit self nixpkgs disko; };

      # Import test runners
      testRunners = import ./testing/test-runners.nix { inherit nixpkgs system; };

      # Common test-artefact value type plus helpers that convert NMBL
      # VM configs into the uniform artefact shape every renderer
      # (screen-based vm-serial-man, tmux-serial, future SDL/VNC, …)
      # consumes. Keeps the "what to test" and "how to display it"
      # axes orthogonal so each one can evolve without N×M duplication.
      testArtefact = import ./testing/test-artefact.nix { inherit nixpkgs system; };

      # tmux-serial renderer. Reads a testArtefact, returns a
      # writeShellApplication that hosts QEMU directly in a named tmux
      # pane (no socat / pty broker — QEMU's `-serial mon:stdio` IS the
      # pane's pty). Used to verify the unified ratatui TUI renders
      # the LUKS passphrase modal cleanly over serial.
      tmuxSerial = import ./testing/serial-tmux-harness.nix { inherit nixpkgs system; };

      # Import vm-serial-man package directly
      vmSerialManFlake = import ../vm-serial-man-rs/flake.nix;
      vmSerialMan =
        (vmSerialManFlake.outputs {
          self = vmSerialManFlake;
          inherit nixpkgs;
        }).packages.${system}.default;

      # The Rust /init binary that replaces the bash script bundle.
      # Built by the sibling flake at ./nmbl-init-rs.
      nmblInit = nmbl-init-rs.packages.${system}.default;

      # Same binary with the `image-splash` cargo feature compiled in.
      # `lib/config.nix` selects between the two based on
      # `boot.nmbl.splash.enable`; this attribute is injected
      # unconditionally so the option can be toggled at evaluation time.
      nmblInitSplash = nmbl-init-rs.packages.${system}.nmbl-init-splash;

      # Builder form: lib/config.nix uses this to build a /init with
      # extra Cargo features (currently `network-rescue` when
      # `boot.nmbl.rescue.network = true`). Falls back to the default
      # binary if the sibling flake hasn't been updated yet.
      mkNmblInit =
        nmbl-init-rs.legacyPackages.${system}.mkNmblInit
          or (_: nmblInit);

      # The host-platform `nmbl-sign` ML-DSA image signer. Used by
      # lib/modules/driver-image.nix to sign each driver-image squashfs at
      # install time (`--domain driver-image`). `null` on an older sibling
      # flake that predates the signer; driver-image.nix turns that into a
      # hard eval error only WHEN driver images are enabled.
      nmblSign =
        nmbl-init-rs.packages.${system}.nmbl-sign or null;

      # The host / install-time LUKS-to-TPM seal helper (`nmbl-tpm-enroll`). It
      # reuses `systemd-cryptenroll` to write a LUKS2 systemd-tpm2 token that
      # NMBL's boot-time `cryptsetup open --token-only` unlock consumes (no Rust
      # TPM2_Unseal — master-plan §A SEALING-REUSE). Exposed as a package so the
      # installer / operator can run it once after first boot; it is asserted
      # ABSENT from the initramfs closure by lib/config.nix.
      nmblTpmEnroll = import ./lib/tpm-enroll.nix { inherit pkgs lib; };
      lib = nixpkgs.lib;

      # Import rescue-vm-test app directly
      rescueVmTestFlake = import ../rescue-vm-test/flake.nix;
      rescueVmTestApp =
        (rescueVmTestFlake.outputs {
          self = rescueVmTestFlake;
          inherit nixpkgs;
        }).apps.${system}.default;

      # Import nixos-anywhere-test directly. Synthesise a `self` whose outPath
      # points to the sub-tree inside this flake's whole-tree source copy so
      # that `import ../rescue-vm-test/flake.nix` inside the imported flake can
      # still see its siblings.
      nixosAnywhereTestFlake = import ../nixos-anywhere-test/flake.nix;
      nixosAnywhereTest = nixosAnywhereTestFlake.outputs {
        self = nixosAnywhereTestFlake // {
          # self.outPath for a ?dir=sirati-nmbl flake already includes the
          # /sirati-nmbl suffix. Use builtins.dirOf to get the repo root, then
          # append /nixos-anywhere-test (a sibling of sirati-nmbl).
          outPath = builtins.dirOf self.outPath + "/nixos-anywhere-test";
        };
        inherit
          nixpkgs
          disko
          nixos-anywhere
          nmbl-init-rs
          ;
      };
      nixosAnywhereTestApps = {
        install-test-gpt-bios = nixosAnywhereTest.apps.${system}.install-test-gpt-bios;
        install-test-gpt-uefi-grub = nixosAnywhereTest.apps.${system}.install-test-gpt-uefi-grub;
        install-test-gpt-uefi-systemd = nixosAnywhereTest.apps.${system}.install-test-gpt-uefi-systemd;
        install-test-gpt-bios-raid1 = nixosAnywhereTest.apps.${system}.install-test-gpt-bios-raid1;
        install-test-gpt-uefi-grub-raid1 = nixosAnywhereTest.apps.${system}.install-test-gpt-uefi-grub-raid1;
        install-test-gpt-uefi-systemd-raid1 = nixosAnywhereTest.apps.${system}.install-test-gpt-uefi-systemd-raid1;
        install-test-gpt-bios-btrfs-raid1 = nixosAnywhereTest.apps.${system}.install-test-gpt-bios-btrfs-raid1;
        install-test-gpt-uefi-grub-btrfs-raid1 = nixosAnywhereTest.apps.${system}.install-test-gpt-uefi-grub-btrfs-raid1;
        install-test-gpt-uefi-systemd-btrfs-raid1 = nixosAnywhereTest.apps.${system}.install-test-gpt-uefi-systemd-btrfs-raid1;
      };

      # Aliases that surface the install orchestrators under the
      # three-axis matrix's `nixos-anywhere-install-<target>-<interaction>`
      # namespace. The `nixos-anywhere-install` start mode is special
      # — its renderer IS the orchestrator (rescue VM + nixos-anywhere
      # + stage-3 verify), so the matrix combos directly re-export the
      # existing legacy apps rather than running through the generic
      # interaction renderers.
      #
      # `-screen` is the operator-friendly alias for `-qemu-serial-rs`
      # (matches the historical "GNU screen + vm-serial-man" path that
      # the legacy `install-test-*` apps used).
      nixosAnywhereInstallAliases =
        let
          a = nixosAnywhereTest.apps.${system};
          # vnc-demo variant for the splash-luks path; only some
          # configs have a noVNC bridge wired up.
          alias = legacyName: a.${legacyName};
        in
        {
          nixos-anywhere-install-plain-ext4-qemu-serial-rs = alias "install-test-gpt-uefi-grub";
          nixos-anywhere-install-plain-ext4-screen = alias "install-test-gpt-uefi-grub";
          nixos-anywhere-install-mdraid-qemu-serial-rs = alias "install-test-gpt-uefi-grub-raid1";
          nixos-anywhere-install-mdraid-screen = alias "install-test-gpt-uefi-grub-raid1";
          nixos-anywhere-install-btrfs-raid1-qemu-serial-rs = alias "install-test-gpt-uefi-grub-btrfs-raid1";
          nixos-anywhere-install-btrfs-raid1-screen = alias "install-test-gpt-uefi-grub-btrfs-raid1";
          # LUKS install: the existing splash-luks-serial-demo
          # orchestrator does a full install onto the luks-password
          # disko layout and exits cleanly, leaving disks at
          # $WORK_DIR/disk1.qcow2 + $WORK_DIR/disk2.qcow2 — exactly
          # what kvm-kexec-installed-luks-password-tmux needs as input.
          nixos-anywhere-install-luks-password-qemu-serial-rs = alias "install-test-splash-luks-serial-demo";
          nixos-anywhere-install-luks-password-screen = alias "install-test-splash-luks-serial-demo";
        };

      # Build test runner apps for each configuration. Each entry
      # gets two apps:
      #   - `<name>`               → vm-serial-man + GNU screen (legacy)
      #   - `tmux-serial-<name>`   → tmux pane hosting QEMU directly,
      #                              ratatui TUI renders over the
      #                              pane's pty.
      # The tmux-serial variant exists because the ratatui passphrase
      # modal renders 1:1 over a serial UART, so a vt100-aware tmux
      # pane is now a first-class display for any NMBL boot test.
      testApps = builtins.listToAttrs (
        builtins.concatLists (
          builtins.attrValues (
            builtins.mapAttrs (
              name: cfg:
              let
                config = testing.mkTestConfigurations.${name};
                artefact = testArtefact.artefactFromVmConfig {
                  inherit name config;
                };
                legacyApp = {
                  name = "${name}";
                  value = {
                    type = "app";
                    program = "${testRunners.mkRunner {
                      inherit name config vmSerialMan;
                      bootMode = null; # Will be derived from config.bootstrapper
                    }}";
                  };
                };
                tmuxApp = {
                  name = "tmux-serial-${name}";
                  value = {
                    type = "app";
                    program =
                      "${tmuxSerial.mkTmuxSerialRunner { inherit artefact; }}/bin/tmux-serial-${name}";
                  };
                };
              in
              [ legacyApp tmuxApp ]
            ) testing.configs
          )
        )
      );

      # Three-axis test matrix: start-mode × target × interaction.
      # Generates apps named `<start>-<target>-<interaction>` (e.g.
      # `kvm-kexec-installed-luks-password-tmux`). Adding a target,
      # start mode, or interaction is one new file in
      # testing/{targets,start-modes,interactions}/ + one line in
      # the corresponding default.nix.
      testMatrix = import ./testing/compose.nix {
        inherit
          self
          nixpkgs
          disko
          nixos-anywhere
          vmSerialMan
          system
          ;
      };

      # Automated log-import test. Reuses the existing direct-kernel runner
      # for the qemu_kernel_invoke config (fastest path: real NMBL init +
      # kexec into a booted NixOS, no install) and drives it with the
      # assertion scripts under testing/assertions. Asserts the pre-kexec
      # NMBL log lands in the booted journal under tag `nmbl-init`.
      logImportRunner = testRunners.mkRunner {
        name = "test-gpt-qemu-kernel-invoke";
        config = testing.mkTestConfigurations."test-gpt-qemu-kernel-invoke";
        inherit vmSerialMan;
        bootMode = null;
      };

      checkLogImport = pkgs.writeShellApplication {
        name = "check-log-import";
        runtimeInputs = [
          vmSerialMan
          pkgs.screen
          pkgs.coreutils
          pkgs.gnugrep
          pkgs.gnused
          pkgs.qemu_kvm
        ];
        text = ''
          export NMBL_RUNNER=${logImportRunner}
          # Import the whole assertions dir so log-import.sh finds its
          # sibling lib.sh at runtime (it sources ./lib.sh by BASH_SOURCE).
          assertions=${./testing/assertions}
          # Hard wall-clock cap so a wedged VM can never hang CI forever.
          timeout "''${NMBL_WALL_TIMEOUT:-600}" \
            bash "$assertions/log-import.sh"
        '';
      };

      # INSECURE TEST-ONLY signing keypair glue (#56). Exposes the committed
      # fixed ML-DSA-87 keypair to the test harness, `signTestArtifact` to sign
      # test artifacts with it, and `assertAbsentFromClosure` — the build check
      # that the test PRIVATE key never enters a PRODUCTION NMBL closure.
      testKeys = import ./testing/keys.nix { inherit pkgs lib nmblSign; };

      # The PRODUCTION-closure absence guard (#56 / FIX-61): the insecure-test
      # private key must NOT appear in a production NMBL initramfs closure.
      # Built against test-gpt-uefi-grub's nmblInitramfs (a config that never
      # imports testing/keys), so this PASSES; it FIRES if the key were ever
      # baked into a production artifact. Mirrors the nmbl-tpm-enroll absence
      # assert in lib/config.nix.
      insecureKeyAbsentFromProd = testKeys.assertAbsentFromClosure {
        name = "insecure-test-key-absent-from-prod-initramfs";
        closurePath =
          testing.mkTestConfigurations."test-gpt-uefi-grub".config.system.build.nmblInitramfs;
      };

      # Secure-Boot enforcement smoke test (#55 / R-10). Boots an UNSIGNED UKI
      # under a Secure-Boot-ENFORCING OVMFFull and asserts the firmware refuses
      # it (the #29 precondition). BUILT here; #57 runs the VM.
      sbSmoke = import ./testing/sb-smoke.nix {
        inherit
          nixpkgs
          system
          testRunners
          vmSerialMan
          ;
        config = testing.mkTestConfigurations."test-gpt-uefi-grub";
      };

      checkSbUnsignedUki = pkgs.writeShellApplication {
        name = "check-sb-unsigned-uki";
        runtimeInputs = [
          vmSerialMan
          pkgs.screen
          pkgs.coreutils
          pkgs.gnugrep
          pkgs.gnused
          pkgs.qemu_kvm
          pkgs.OVMFFull
          pkgs.swtpm
        ];
        text = ''
          export NMBL_RUNNER=${sbSmoke.runner}
          assertions=${./testing/assertions}
          timeout "''${NMBL_WALL_TIMEOUT:-600}" \
            bash "$assertions/sb-unsigned-uki.sh"
        '';
      };

      # Secure-boot CHAIN VM scenarios (#57 F6b). One runner wires the whole
      # measured/signed chain under the swtpm "tis" + SB-OVMF (smm=on) seam;
      # each scenario is its own `.#test-secure-boot-<scenario>` app the #57
      # runner executes. BUILT here; #57 runs the VMs.
      secureBootConfig = testing.mkTestConfigurations."test-secure-boot";
      secureBootRunner = testRunners.mkRunner {
        name = "test-secure-boot";
        config = secureBootConfig;
        inherit vmSerialMan;
        tpm = "tis";
        secureBoot = true;
        # ENFORCING SB firmware that ALSO trusts the test UKI (F1): the test
        # db CERT is enrolled into the MS VARS' `db` so the firmware accepts
        # the NMBL UKI sbsign'd with the matching key at install time, while
        # still refusing anything unsigned. PUBLICLY-KNOWN test cert (path
        # literal ⇒ store import is fine; only the install-time signing
        # key/cert must stay out of the store, which they do — they are impure
        # string paths). The unsigned-UKI smoke (sbSmoke.runner) leaves dbCert
        # unset so it KEEPS the MS-only db and still refuses the unsigned UKI.
        dbCert = ./testing/keys/insecure-test-sb-db.crt;
      };

      # The runtime inputs every secure-boot scenario needs on PATH.
      sbScenarioInputs = [
        vmSerialMan
        pkgs.screen
        pkgs.coreutils
        pkgs.gnugrep
        pkgs.gnused
        pkgs.qemu_kvm
        pkgs.OVMFFull
        pkgs.swtpm
      ];

      # Build one scenario check app from its assertion script. `extraEnv` lets
      # the bad-sig scenario export the pristine disk path it tampers.
      mkSbScenarioCheck =
        {
          scenario,
          script,
          extraInputs ? [ ],
          extraEnv ? "",
        }:
        pkgs.writeShellApplication {
          name = "test-secure-boot-${scenario}";
          runtimeInputs = sbScenarioInputs ++ extraInputs;
          text = ''
            export NMBL_RUNNER=${secureBootRunner}
            ${extraEnv}
            assertions=${./testing/assertions}
            timeout "''${NMBL_WALL_TIMEOUT:-900}" \
              bash "$assertions/${script}"
          '';
        };

      checkSbTpmRoundtrip = mkSbScenarioCheck {
        scenario = "tpm-roundtrip";
        script = "sb-tpm-roundtrip.sh";
      };
      checkSbSignedGenHappy = mkSbScenarioCheck {
        scenario = "signed-gen-happy";
        script = "sb-signed-gen-happy.sh";
      };
      checkSbBadSigRefused = mkSbScenarioCheck {
        scenario = "bad-sig-refused";
        script = "sb-bad-sig-refused.sh";
        # The bad-sig scenario tampers a pristine disk copy (removes a sidecar
        # off the FAT32 boot partition), so it needs libguestfs + the disk path.
        extraInputs = [ pkgs.libguestfs-with-appliance ];
        extraEnv = ''
          export NMBL_SB_DISK=${secureBootConfig.config.system.build.vmDiskImage}/nixos.qcow2
        '';
      };
    in
    {
      # The main NixOS module
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        {
          imports = [
            # ./lib/options.nix already imports ./lib/modules/activation.nix,
            # so listing it here too would be redundant (NixOS dedups by
            # path but keeping a single import site avoids confusion).
            ./lib/options.nix
            ./lib/config.nix
          ];

          # Make the Rust /init binaries available to lib/config.nix
          # without forcing every caller to pass them explicitly. Both
          # are injected unconditionally; lib/config.nix picks one based
          # on `boot.nmbl.splash.enable`.
          _module.args.nmblInit = nmblInit;
          _module.args.nmblInitSplash = nmblInitSplash;
          # Builder used by lib/config.nix when the rescue-network path
          # is enabled — produces a /init with the `network-rescue`
          # Cargo feature compiled in.
          _module.args.mkNmblInit = mkNmblInit;
          # The `nmbl-sign` ML-DSA image signer, used by the driver-image
          # build to sign each squashfs at install time.
          _module.args.nmblSign = nmblSign;
        };

      # Installer-available host tools. `nmbl-tpm-enroll` seals a LUKS volume
      # key to the TPM bound to NMBL's measured-boot PCRs (11+7); run it once,
      # after the box has first-booted the installed system, against the LUKS
      # header. It is asserted ABSENT from the initramfs closure (it never ships
      # in NMBL's boot environment). Build with `nix build .#nmbl-tpm-enroll`.
      packages.${system} = {
        nmbl-tpm-enroll = nmblTpmEnroll;
        # Build check (#56): the insecure-test signing key must be ABSENT from
        # a production NMBL closure. `nix build .#insecure-test-key-absent`.
        insecure-test-key-absent = insecureKeyAbsentFromProd;
        # The unsigned-UKI Secure-Boot smoke-test disk (#55): a GPT/ESP image
        # whose BOOTX64.EFI is an UNSIGNED UKI, for the SB-refusal harness.
        sb-unsigned-uki-disk = sbSmoke.unsignedUkiDisk;
      };

      # Build-only validation gates surfaced for CI / `nix flake check`-style
      # consumption: the insecure-test-key prod-absence guard (#56).
      checks.${system} = {
        insecure-test-key-absent = insecureKeyAbsentFromProd;
      };

      # Test configurations
      nixosConfigurations = testing.mkTestConfigurations;

      # Debug info for each test configuration
      # Access with: nix build .#debugInfo.test-gpt-bios
      # Or view with: nix eval .#debugInfo.test-gpt-bios --raw
      debugInfo = builtins.mapAttrs (
        name: config: config.config.system.build.nmblDebugInfo
      ) testing.mkTestConfigurations;

      # Test runner apps
      # Run with: nix run .#test-gpt-bios
      # Run with: nix run .#test-gpt-uefi-grub
      # Run with: nix run .#test-gpt-uefi-systemd
      # Run with: nix run .#test-gpt-qemu-kernel-invoke
      # Run with: nix run .#test-gpt-qemu-kernel-invoke -- --debug-shell  (drops to emergency shell)
      # Run with: nix run .#test-rescue-ssh -- [--pubkey-file PATH] [--port N]
      # Run with: nix run .#tmux-serial-test-gpt-uefi-grub-luks-password
      # Run with: nix run .#check-log-import  (asserts NMBL pre-kexec log in journal)
      # Apps: legacy names first, then the three-axis matrix grafted
      # on. Legacy names keep working unchanged (operators may have
      # them in muscle memory). New matrix apps use the dotted
      # `<start>-<target>-<interaction>` naming.
      apps.${system} = testApps // {
        test-rescue-ssh = rescueVmTestApp;
        check-log-import = {
          type = "app";
          program = "${checkLogImport}/bin/check-log-import";
        };
        # Secure-Boot enforcement smoke test (#55 / R-10): asserts the firmware
        # refuses an unsigned UKI. Run by #57. `nix run .#check-sb-unsigned-uki`.
        check-sb-unsigned-uki = {
          type = "app";
          program = "${checkSbUnsignedUki}/bin/check-sb-unsigned-uki";
        };
        # Secure-boot CHAIN scenarios (#57 F6b). Each boots the test-secure-boot
        # config (swtpm "tis" + SB-OVMF) and drives one assertion script.
        # `nix run .#test-secure-boot-<scenario>`.
        test-secure-boot-tpm-roundtrip = {
          type = "app";
          program = "${checkSbTpmRoundtrip}/bin/test-secure-boot-tpm-roundtrip";
        };
        test-secure-boot-signed-gen-happy = {
          type = "app";
          program = "${checkSbSignedGenHappy}/bin/test-secure-boot-signed-gen-happy";
        };
        test-secure-boot-bad-sig-refused = {
          type = "app";
          program = "${checkSbBadSigRefused}/bin/test-secure-boot-bad-sig-refused";
        };
      } // nixosAnywhereTestApps // testMatrix.apps // nixosAnywhereInstallAliases;

      # Reusable library: external flakes (the LUKS install orchestrator,
      # rescue-vm-test, future variants) consume `testArtefact` to
      # describe their VM-under-test and `tmuxSerial.mkTmuxSerialRunner`
      # to render it into a tmux session. Exposed as legacyPackages so
      # callers can `nmbl.legacyPackages.${system}.testArtefact.mkArtefact ...`
      # without re-importing the testing/ files by relative path.
      #
      # `tmuxSerialRunners` is a precomputed attrset of every per-config
      # runner derivation, keyed by the same name as `apps.<...>`. Useful
      # when an external flake wants to `nix build` a runner directly
      # (e.g. as a release artefact) rather than `nix run` it.
      legacyPackages.${system} = {
        inherit testArtefact tmuxSerial;
        # INSECURE TEST-ONLY signing keypair glue (#56): `signTestArtifact`,
        # `assertAbsentFromClosure`, `bakedPublicKey`, and the raw key paths.
        inherit testKeys;
        # The Secure-Boot unsigned-UKI smoke harness (#55): the runner, the
        # unsigned UKI, and the ESP disk image.
        inherit sbSmoke;
        tmuxSerialRunners = builtins.mapAttrs (
          name: _cfg:
          let
            config = testing.mkTestConfigurations.${name};
            artefact = testArtefact.artefactFromVmConfig { inherit name config; };
          in
          tmuxSerial.mkTmuxSerialRunner { inherit artefact; }
        ) testing.configs;
        # Three-axis test matrix surface: callers can reach the
        # individual axis registries (targets / start-modes /
        # interactions) and the precomputed cross product via
        # legacyPackages.${system}.testMatrix.
        testMatrix = testMatrix;
      };
    };
}
