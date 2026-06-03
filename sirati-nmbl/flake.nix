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
      # PUBLIC ML-DSA-87 key (the baked trust anchor) and `assertAbsentFromClosure`
      # — the build check that a signing PRIVATE key never enters a NMBL closure.
      # No private-key-importing signer: test artifacts are signed at INSTALL
      # RUNTIME (lib/install-{signing,gen-signing}.nix), never in a derivation.
      testKeys = import ./testing/keys.nix { inherit pkgs lib; };

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
      #
      # SIGNING MODEL (HARD principle): a signing PRIVATE key must NEVER be a Nix
      # derivation input. The test disk is therefore SIGNED AT INSTALL RUNTIME by
      # NMBL's normal install-time path-based signing — NOT by a build-time
      # derivation that store-imports the keys. The `test-secure-boot` config
      # already declares `signing.uki.{keyFile,certFile}` and `generationKeyFile`
      # as on-disk PATHs (`/var/lib/nmbl-test-keys/…`); the `sb-install-*`
      # orchestrator (testing/sb-install.nix) runs nixos-anywhere, stages the
      # committed test keys at those paths INSIDE the install chroot's root fs
      # (/var/lib, not /run — activation overmounts /run with a tmpfs), and
      # `installBootLoader` `sbsign`s the UKI + writes the ML-DSA generation
      # sidecars in place. The
      # booted disk is signed by the real install-time code and NO key is in any
      # derivation closure.
      secureBootConfig = testing.mkTestConfigurations."test-secure-boot";

      # Install variant of the real config: same config, but with the disko
      # build's `deferInstallSigning = mkDefault true` OVERRIDDEN to false so the
      # install-time signing ACTUALLY RUNS during the nixos-anywhere install
      # (where the staged key paths exist). Used only for its diskoScript +
      # toplevel store-paths — nixos-anywhere installs THIS and signs in place.
      secureBootInstallConfig = testing.mkTestVM (
        testing.configs."test-secure-boot" // {
          extraModules =
            (testing.configs."test-secure-boot".extraModules or [ ])
            ++ [ { boot.nmbl.signing.deferInstallSigning = lib.mkForce false; } ];
        }
      );

      # Runtime-install orchestrators (no signing key ever enters a derivation).
      sbInstall = import ./testing/sb-install.nix {
        inherit nixpkgs system nixos-anywhere;
        rescueArtifacts = (rescueVmTestFlake.outputs {
          self = rescueVmTestFlake;
          inherit nixpkgs;
        }).packages.${system};
      };

      secureBootInstaller = sbInstall.mkSbInstaller {
        name = "test-secure-boot";
        config = secureBootInstallConfig;
        port = "22201";
      };

      # CLOSURE GUARD: the install artifact (everything nixos-anywhere ships and
      # then signs in place) must reference NEITHER signing private key. Mirrors
      # `insecure-test-key-absent`, but for the SECURE-BOOT TEST install path and
      # for BOTH the ML-DSA generation key and the SB `db` private key. We assert
      # against a combined closure of the diskoScript + toplevel (the two
      # --store-paths the installer hands nixos-anywhere). Because the disk is
      # signed at INSTALL RUNTIME from a staged PATH, the keys must be absent.
      secureBootNoPrivateKey = testKeys.assertAbsentFromClosure {
        name = "test-secure-boot-no-private-key";
        # The two --store-paths the installer hands nixos-anywhere — together they
        # are everything that ships and is then signed in place at install time.
        rootPaths = [
          secureBootInstallConfig.config.system.build.diskoScript
          secureBootInstallConfig.config.system.build.toplevel
        ];
        # ALSO assert the Secure-Boot `db` private key (not just the ML-DSA one).
        extraKeyPaths = [ ./testing/keys/insecure-test-sb-db.key ];
      };

      secureBootRunner = testRunners.mkRunner {
        name = "test-secure-boot";
        config = secureBootConfig;
        inherit vmSerialMan;
        tpm = "tis";
        secureBoot = true;
        # PERSIST the swtpm state across the manager's lifetime so the TPM
        # seal/unseal ROUNDTRIP works: the enroll phase seals a token into the
        # SAME persisted swtpm this (unseal) phase then power-cycles into. A
        # fresh QEMU power-on still issues TPM2_Startup(CLEAR), so PCRs reset
        # and NMBL re-extends the same deterministic sequence (same generation
        # ⇒ same PCR-11, same firmware/db/UKI ⇒ same PCR-7). The non-roundtrip
        # scenarios (signed-gen, bad-sig) also run with persist on, which is
        # harmless: each is a single run and the assertion deletes the shared
        # state dir at cleanup.
        tpmPersist = true;
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

      # ── TPM-roundtrip ENROLL twin ──────────────────────────────────────────
      # The roundtrip's phase 1 needs a boot that REACHES the post-kexec system
      # so `nmbl-tpm-enroll` can seal the volume key to the live PCRs. NMBL's
      # `luks-tpm` activation runs `cryptsetup open --token-only` and cannot use
      # a passphrase, so a fresh (un-enrolled) disk would never reach the
      # system. This twin overrides ONLY the cryptroot unlock to `password`
      # (the install passphrase), so it boots the SAME generation — same
      # kernel/initrd/cmdline ⇒ the SAME PCR-11 event sequence the real config
      # extends — and reaches the system to enroll. PCR 7 is identical too (same
      # SB db / UKI signature). After it seals the token onto the on-disk LUKS2
      # header, the real tpm-unlock config power-cycles into the SAME swtpm and
      # auto-unseals. The twin is a SEPARATE signed disk (its config.toml/UKI
      # differ), built only when the roundtrip runs.
      secureBootEnrollConfig =
        let
          baseCfg = testing.configs."test-secure-boot";
          # Re-instantiate the test-secure-boot config through the SAME builder
          # but with one extra module that forces the cryptroot activation to a
          # passphrase unlock. Everything else (the generation: kernel, initrd,
          # cmdline, signing, SB posture) is identical, so the PCR-11 events and
          # PCR 7 match the real config — the seal the enroll phase makes is
          # exactly what the tpm-unlock phase unseals. `passToStage1` hands the
          # passphrase to the post-kexec system initrd.
          enrollOverride = { lib, ... }: {
            boot.nmbl.activation.luks = lib.mkForce [
              {
                name = "cryptroot";
                device = "/dev/vda3";
                unlock = "password";
                promptLabel = "Enter LUKS passphrase for cryptroot (enroll phase)";
                passToStage1 = "/etc/nmbl-luks/cryptroot";
              }
            ];
          };
          enrollModules =
            (testing.configs."test-secure-boot".extraModules or [ ]) ++ [ enrollOverride ];
        in
        testing.mkTestVM (baseCfg // {
          name = "test-secure-boot-enroll";
          extraModules = enrollModules;
        });

      # Install variant of the enroll twin (deferInstallSigning forced off so the
      # nixos-anywhere install signs its UKI + sidecars in place — same path-based
      # install-time signing as the real config, no key in any derivation).
      secureBootEnrollInstallConfig = testing.mkTestVM (
        testing.configs."test-secure-boot" // {
          name = "test-secure-boot-enroll";
          extraModules =
            (testing.configs."test-secure-boot".extraModules or [ ])
            ++ [
              (
                { lib, ... }:
                {
                  boot.nmbl.activation.luks = lib.mkForce [
                    {
                      name = "cryptroot";
                      device = "/dev/vda3";
                      unlock = "password";
                      promptLabel = "Enter LUKS passphrase for cryptroot (enroll phase)";
                      passToStage1 = "/etc/nmbl-luks/cryptroot";
                    }
                  ];
                  boot.nmbl.signing.deferInstallSigning = lib.mkForce false;
                }
              )
            ];
        }
      );

      secureBootEnrollInstaller = sbInstall.mkSbInstaller {
        name = "test-secure-boot-enroll";
        config = secureBootEnrollInstallConfig;
        port = "22202";
      };

      secureBootEnrollRunner = testRunners.mkRunner {
        name = "test-secure-boot-enroll";
        config = secureBootEnrollConfig;
        inherit vmSerialMan;
        tpm = "tis";
        secureBoot = true;
        # Persist so the token this phase seals survives into the unseal phase.
        tpmPersist = true;
        dbCert = ./testing/keys/insecure-test-sb-db.crt;
      };

      # ── Driver-image scenario (#1 / FEATURE-#1) ─────────────────────────────
      # The test-secure-boot-driver config wires the full secure-boot chain but
      # adds `boot.nmbl.driverImages` (a signed squashfs carrying `dummy`, a
      # module NOT in the base initrd) and opens cryptroot with the install
      # PASSPHRASE so the boot reaches the post-kexec system. Same install-runtime
      # signing model: the driver squashfs is signed in place by NMBL's
      # install-time code from the staged `imageKeyFile` PATH — no key in any
      # derivation.
      secureBootDriverConfig = testing.mkTestConfigurations."test-secure-boot-driver";

      # Install variant (deferInstallSigning forced off so the nixos-anywhere
      # install signs the UKI + generation sidecars AND the driver squashfs in
      # place from the staged key paths).
      secureBootDriverInstallConfig = testing.mkTestVM (
        testing.configs."test-secure-boot-driver" // {
          extraModules =
            (testing.configs."test-secure-boot-driver".extraModules or [ ])
            ++ [ { boot.nmbl.signing.deferInstallSigning = lib.mkForce false; } ];
        }
      );

      secureBootDriverInstaller = sbInstall.mkSbInstaller {
        name = "test-secure-boot-driver";
        config = secureBootDriverInstallConfig;
        port = "22203";
      };

      # CLOSURE GUARD: the driver-image install artifact must reference NO signing
      # private key (the ML-DSA generation/image key NOR the SB `db` key). The
      # driver squashfs is a PURE derivation; its signature is produced at install
      # runtime from a staged PATH, so neither key is in the closure.
      secureBootDriverNoPrivateKey = testKeys.assertAbsentFromClosure {
        name = "test-secure-boot-driver-no-private-key";
        rootPaths = [
          secureBootDriverInstallConfig.config.system.build.diskoScript
          secureBootDriverInstallConfig.config.system.build.toplevel
        ];
        extraKeyPaths = [ ./testing/keys/insecure-test-sb-db.key ];
      };

      secureBootDriverRunner = testRunners.mkRunner {
        name = "test-secure-boot-driver";
        config = secureBootDriverConfig;
        inherit vmSerialMan;
        tpm = "tis";
        secureBoot = true;
        tpmPersist = true;
        dbCert = ./testing/keys/insecure-test-sb-db.crt;
      };

      # ── Staged-boot scenario config (matrix row #2 / FEATURE #2) ───────────
      # A twin of test-secure-boot that proves STAGED BOOT: the priority volume
      # (the inside-LUKS cryptroot) carries a signed config fragment + staged
      # image NMBL loads as a second stage. After the post-unlock priority gate
      # attests the volume, `apply_staged_boot` verifies the image
      # (--domain driver-image) AND the fragment (--domain staged-fragment),
      # transactionally merges the fragment, and re-runs its effects — here the
      # fragment adds ONE extra explicit kernel module the base never loads, so
      # the staged re-run loading it is the observable proof the merge applied.
      #
      # Like the enroll twin it overrides cryptroot to a PASSWORD unlock so the
      # standalone scenario opens it without a TPM enrol (the SAME install-signed
      # generation otherwise — same measured/SB posture). It additionally turns
      # on the priority gate + staged boot and ships the `dummy` module in the
      # initrd (available, NOT loaded by the base) for the fragment to load.
      stagedOverride = { lib, ... }: {
        # Open cryptroot with the install passphrase (no TPM enrol needed).
        boot.nmbl.activation.luks = lib.mkForce [
          {
            name = "cryptroot";
            device = "/dev/vda3";
            unlock = "password";
            promptLabel = "Enter LUKS passphrase for cryptroot (staged phase)";
            passToStage1 = "/etc/nmbl-luks/cryptroot";
          }
        ];

        # Priority gate on the inside-LUKS cryptroot: only after the activation
        # opens /dev/mapper/cryptroot does the POST-UNLOCK gate fire, attest the
        # volume, and hand staged-boot the AttestedVolume witness. The signed
        # priority file + staged image + fragment all live on this volume,
        # staged + signed at install runtime (lib/staged-install.nix).
        boot.nmbl.secureBoot.priorityVolume = {
          device = "/dev/mapper/cryptroot";
          mountpoint = "/mnt/staged-priority";
          fstype = "ext4";
          insideLuks = true;
        };
        boot.nmbl.secureBoot.signedFilePath = "nmbl-staged/priority.signed";

        boot.nmbl.staged = {
          enable = true;
          image = "nmbl-staged/staged.sfs";
          fragment = "nmbl-staged/fragment.toml";
          sig = "nmbl-staged/fragment.toml.sig";
        };

        # Ship `dummy` (a tiny, standalone in-tree net driver) in the initrd
        # module tree WITHOUT loading it in the base config, so the staged
        # fragment's explicit list is what actually loads it — the observable.
        boot.initrd.availableKernelModules = [ "dummy" ];
      };

      secureBootStagedConfig = testing.mkTestVM (
        testing.configs."test-secure-boot" // {
          name = "test-secure-boot-staged";
          extraModules =
            (testing.configs."test-secure-boot".extraModules or [ ]) ++ [ stagedOverride ];
        }
      );

      # Install variant: deferInstallSigning forced off so the nixos-anywhere
      # install signs the UKI + generation sidecars AND the staged artifacts in
      # place — same path-based install-time signing, no key in any derivation.
      secureBootStagedInstallConfig = testing.mkTestVM (
        testing.configs."test-secure-boot" // {
          name = "test-secure-boot-staged";
          extraModules =
            (testing.configs."test-secure-boot".extraModules or [ ])
            ++ [
              stagedOverride
              { boot.nmbl.signing.deferInstallSigning = lib.mkForce false; }
            ];
        }
      );

      secureBootStagedInstaller = sbInstall.mkSbInstaller {
        name = "test-secure-boot-staged";
        config = secureBootStagedInstallConfig;
        # Port 22210 (clear of the 22201-22203 the real/enroll installers use) so
        # this scenario's nixos-anywhere bootstrap never collides with a
        # concurrent secure-boot install on the same host.
        port = "22210";
      };

      secureBootStagedRunner = testRunners.mkRunner {
        name = "test-secure-boot-staged";
        config = secureBootStagedConfig;
        inherit vmSerialMan;
        tpm = "tis";
        secureBoot = true;
        tpmPersist = true;
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

      # Build one scenario check app from its assertion script.
      #
      # SIGNED-AT-INSTALL-RUNTIME: each scenario first runs the `sb-install-*`
      # orchestrator (nixos-anywhere), which produces the SIGNED disk under
      # `$INSTALL_WORK/disk1.qcow2`. The disk is signed by NMBL's install-time
      # path-based code — no signing key is ever a derivation input. We then
      # export `NMBL_DISK_IMAGE` so the scenario runner (mkRunner) boots THAT
      # signed disk. `extraEnv` lets the bad-sig scenario point at the same disk.
      mkSbScenarioCheck =
        {
          scenario,
          script,
          extraInputs ? [ ],
          extraEnv ? "",
          # The install orchestrator + runner this scenario boots. Default to the
          # real `test-secure-boot` config (tpm-unlock cryptroot). A scenario that
          # only needs to prove NMBL verifies+measures+kexecs a validly-signed
          # generation — but whose cryptroot must actually OPEN for the boot to
          # reach that point — overrides these to the passphrase-unlock twin (the
          # SAME signed generation, only the LUKS unlock differs).
          installer ? secureBootInstaller,
          installBin ? "sb-install-test-secure-boot",
          installSubdir ? "real",
          runner ? secureBootRunner,
        }:
        pkgs.writeShellApplication {
          name = "test-secure-boot-${scenario}";
          runtimeInputs = sbScenarioInputs ++ extraInputs ++ [ pkgs.qemu-utils ];
          text = ''
            export NMBL_RUNNER=${runner}

            # Produce the install-runtime-SIGNED disk unless the caller already
            # staged one (NMBL_SB_SIGNED_DISK). The orchestrator reads the test
            # signing keys from a runtime PATH and signs the UKI + sidecars in
            # place during the nixos-anywhere install.
            INSTALL_WORK="''${NMBL_SB_INSTALL_WORK:-$PWD/.sb-install-work}"
            if [ -z "''${NMBL_SB_SIGNED_DISK:-}" ]; then
              mkdir -p "$INSTALL_WORK"
              echo "=== signing the test disk at install runtime (nixos-anywhere) ==="
              ${installer}/bin/${installBin} \
                --work-dir "$INSTALL_WORK/${installSubdir}"
              NMBL_SB_SIGNED_DISK="$INSTALL_WORK/${installSubdir}/disk1.qcow2"
            fi
            export NMBL_DISK_IMAGE="$NMBL_SB_SIGNED_DISK"
            export NMBL_SB_DISK="$NMBL_SB_SIGNED_DISK"

            ${extraEnv}
            assertions=${./testing/assertions}
            timeout "''${NMBL_WALL_TIMEOUT:-1800}" \
              bash "$assertions/${script}"
          '';
        };

      checkSbTpmRoundtrip = pkgs.writeShellApplication {
        name = "test-secure-boot-tpm-roundtrip";
        runtimeInputs = sbScenarioInputs ++ [ pkgs.libguestfs-with-appliance pkgs.qemu-utils ];
        # The roundtrip drives TWO runners against one persisted swtpm: the
        # passphrase-unlock enroll twin (phase 1, seals the token onto vda3) and
        # the real tpm-unlock config (phase 2, auto-unseals after the
        # power-cycle). Both disks are SIGNED AT INSTALL RUNTIME by the
        # orchestrators. The two phases share ONE disk (the enrolled qcow2);
        # before phase 2 the script swaps that disk's ESP UKI for the real
        # tpm-unlock config's installed-and-signed UKI (NMBL_SB_TPM_UKI,
        # extracted from the real disk's ESP) so the token on vda3 stays intact
        # while the NMBL stage switches to token unlock — needs libguestfs.
        text = ''
          export NMBL_RUNNER=${secureBootRunner}
          export NMBL_ENROLL_RUNNER=${secureBootEnrollRunner}
          export LIBGUESTFS_BACKEND=direct

          INSTALL_WORK="''${NMBL_SB_INSTALL_WORK:-$PWD/.sb-install-work}"
          mkdir -p "$INSTALL_WORK"

          # Phase 1 boots the ENROLL twin (passphrase cryptroot) so it can reach
          # the system and run nmbl-tpm-enroll. Install + sign that disk at
          # runtime; the enroll runner boots it via NMBL_DISK_IMAGE.
          if [ -z "''${NMBL_SB_ENROLL_DISK:-}" ]; then
            echo "=== install+sign the ENROLL-twin disk at runtime (nixos-anywhere) ==="
            ${secureBootEnrollInstaller}/bin/sb-install-test-secure-boot-enroll \
              --work-dir "$INSTALL_WORK/enroll"
            NMBL_SB_ENROLL_DISK="$INSTALL_WORK/enroll/disk1.qcow2"
          fi
          export NMBL_DISK_IMAGE="$NMBL_SB_ENROLL_DISK"

          # The real (tpm-unlock) config: install + sign at runtime so the script
          # can swap its INSTALL-SIGNED UKI onto the shared disk's ESP for phase 2
          # (the token on vda3 stays intact). We only need its signed UKI, which
          # we extract from the installed disk's ESP — no host-side sbsign drv.
          if [ -z "''${NMBL_SB_SIGNED_DISK:-}" ]; then
            echo "=== install+sign the tpm-unlock disk at runtime (for its UKI) ==="
            ${secureBootInstaller}/bin/sb-install-test-secure-boot \
              --work-dir "$INSTALL_WORK/real"
            NMBL_SB_SIGNED_DISK="$INSTALL_WORK/real/disk1.qcow2"
          fi
          NMBL_SB_TPM_UKI="$INSTALL_WORK/real-tpm-uki/BOOTX64.EFI"
          mkdir -p "$INSTALL_WORK/real-tpm-uki"
          guestfish --ro -a "$NMBL_SB_SIGNED_DISK" <<GF
          run
          mount /dev/sda2 /
          download /EFI/BOOT/BOOTX64.EFI $INSTALL_WORK/real-tpm-uki/BOOTX64.EFI
          umount /
          GF
          export NMBL_SB_TPM_UKI

          assertions=${./testing/assertions}
          timeout "''${NMBL_WALL_TIMEOUT:-3000}" \
            bash "$assertions/sb-tpm-roundtrip.sh"
        '';
      };
      # The positive control proves NMBL verifies+measures+kexecs a validly-signed
      # generation and boots it. Reaching that proof requires the cryptroot to
      # OPEN first (storage activation runs before generation verify+kexec). The
      # real config's tpm-unlock cryptroot only opens against a TPM-sealed token,
      # which this standalone scenario never enrols, so it would wedge on the
      # emergency menu. Boot the passphrase-unlock twin instead: it ships the SAME
      # install-signed generation (same kernel/initrd/cmdline/UKI signature) and
      # opens cryptroot with the install passphrase, so the boot reaches NMBL's
      # verify+measure+kexec and proves the signed generation actually runs.
      checkSbSignedGenHappy = mkSbScenarioCheck {
        scenario = "signed-gen-happy";
        script = "sb-signed-gen-happy.sh";
        installer = secureBootEnrollInstaller;
        installBin = "sb-install-test-secure-boot-enroll";
        installSubdir = "enroll";
        runner = secureBootEnrollRunner;
      };
      # The clean NEGATIVE twin of signed-gen-happy. It boots the SAME
      # passphrase-unlock ENROLL twin (same install-signed generation, cryptroot
      # opens with the install passphrase) so the boot actually REACHES NMBL's
      # generation verify — the only difference from signed-gen-happy is that the
      # generation's signature is TAMPERED on the staged disk, so verify FAILS and
      # NMBL refuses → reboot-into-rescue. Pointing at the real tpm-unlock config
      # here would never enrol a token in this standalone scenario, so cryptroot
      # would never open and verify would never run (the bad gen would not be
      # refused — it would simply never be reached).
      checkSbBadSigRefused = mkSbScenarioCheck {
        scenario = "bad-sig-refused";
        script = "sb-bad-sig-refused.sh";
        installer = secureBootEnrollInstaller;
        installBin = "sb-install-test-secure-boot-enroll";
        installSubdir = "enroll";
        runner = secureBootEnrollRunner;
        # The bad-sig scenario tampers a staged disk copy (corrupts a sidecar on
        # the FAT32 boot partition), so it needs libguestfs. NMBL_SB_DISK is set
        # to the install-runtime-signed ENROLL disk by mkSbScenarioCheck above.
        extraInputs = [ pkgs.libguestfs-with-appliance ];
      };

      # POSITIVE driver-image scenario (#1 / FEATURE-#1). Boots the
      # test-secure-boot-driver config: NMBL verifies the signed driver squashfs,
      # loop-mounts it, and `finit_module`s `dummy` (a module absent from the base
      # initrd) BEFORE the generation kexec. cryptroot opens with the install
      # passphrase so the boot reaches the post-kexec shell; the assertion proves
      # the driver-image load ran+loaded the module from the IMAGE via the
      # nmbl-init journal marker.
      checkSbDriverImage = mkSbScenarioCheck {
        scenario = "driver-image";
        script = "sb-driver-image.sh";
        installer = secureBootDriverInstaller;
        installBin = "sb-install-test-secure-boot-driver";
        installSubdir = "driver";
        runner = secureBootDriverRunner;
      };

      # NEGATIVE driver-image scenario (#1 NEG). The SAME config + signed disk,
      # but the driver squashfs is CORRUPTED on the FAT32 boot partition before
      # boot, so the single-fd verify FAILS. Per the IMPLEMENTED enforce-mode
      # behaviour (imageload/verify.rs → policy::refuse_unsigned, R-1) NMBL
      # REFUSES and reboots into rescue — it never mounts the bad image. We assert
      # the refuse, that no driver-image-loaded marker reaches the system, and
      # that no emergency shell is offered. Needs libguestfs to tamper the disk.
      checkSbDriverImageBadRefused = mkSbScenarioCheck {
        scenario = "driver-image-bad-refused";
        script = "sb-driver-image-bad-refused.sh";
        installer = secureBootDriverInstaller;
        installBin = "sb-install-test-secure-boot-driver";
        installSubdir = "driver";
        runner = secureBootDriverRunner;
        extraInputs = [ pkgs.libguestfs-with-appliance ];
      };

      # STAGED BOOT (matrix row #2 / FEATURE #2). Boots the staged twin: the
      # priority volume (inside-LUKS cryptroot) carries a signed fragment +
      # staged image installed+signed AT RUNTIME by lib/staged-install.nix.
      # cryptroot opens with the install passphrase, the post-unlock priority
      # gate attests the volume, staged-boot verifies+merges the fragment (which
      # adds an extra explicit module), then the boot reaches the root shell.
      # The proof markers (priority-gate VALID, fragment applied, the staged
      # module loaded) are read from the post-kexec nmbl-init journal.
      checkSbStaged = mkSbScenarioCheck {
        scenario = "staged";
        script = "sb-staged.sh";
        installer = secureBootStagedInstaller;
        installBin = "sb-install-test-secure-boot-staged";
        installSubdir = "staged";
        runner = secureBootStagedRunner;
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
        # The runtime SB-install orchestrators (#57 F6b). Each installs the
        # test-secure-boot NMBL config via nixos-anywhere and signs the UKI +
        # generation sidecars AT INSTALL RUNTIME from the staged key paths — no
        # signing key ever enters a derivation. The booted disk lands in the
        # orchestrator's work dir. `nix build .#sb-install-test-secure-boot`.
        sb-install-test-secure-boot = secureBootInstaller;
        sb-install-test-secure-boot-enroll = secureBootEnrollInstaller;
        sb-install-test-secure-boot-driver = secureBootDriverInstaller;
        sb-install-test-secure-boot-staged = secureBootStagedInstaller;
        # CLOSURE GUARD (#57 F6b): the secure-boot test INSTALL artifact's closure
        # (diskoScript + toplevel — everything nixos-anywhere ships) must contain
        # NEITHER the ML-DSA generation key NOR the SB `db` private key. Proves
        # the install signs from a runtime PATH, never a derivation input.
        # `nix build .#test-secure-boot-no-private-key`.
        test-secure-boot-no-private-key = secureBootNoPrivateKey;
        # Same closure guard for the driver-image install artifact (#1): the
        # driver squashfs is pure, signed at install runtime from a staged PATH,
        # so no signing key is in its closure.
        test-secure-boot-driver-no-private-key = secureBootDriverNoPrivateKey;
      };

      # Build-only validation gates surfaced for CI / `nix flake check`-style
      # consumption: the insecure-test-key prod-absence guard (#56) and the
      # secure-boot-install private-key-absence guard (#57 F6b — the signed test
      # disk is signed at install runtime, so no signing key is in its closure).
      checks.${system} = {
        insecure-test-key-absent = insecureKeyAbsentFromProd;
        test-secure-boot-no-private-key = secureBootNoPrivateKey;
        test-secure-boot-driver-no-private-key = secureBootDriverNoPrivateKey;
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
        # Driver-image scenarios (#1 / FEATURE-#1): a signed driver squashfs
        # carrying a module absent from the base initrd is verified, loop-mounted
        # and loaded pre-kexec (positive), or refused when corrupted (negative).
        # `nix run .#test-secure-boot-driver-image`.
        test-secure-boot-driver-image = {
          type = "app";
          program = "${checkSbDriverImage}/bin/test-secure-boot-driver-image";
        };
        test-secure-boot-driver-image-bad-refused = {
          type = "app";
          program = "${checkSbDriverImageBadRefused}/bin/test-secure-boot-driver-image-bad-refused";
        };
        # Staged-boot apply (matrix row #2 / FEATURE #2). The priority volume
        # carries a signed fragment + staged image NMBL loads as a second stage.
        # `nix run .#test-secure-boot-staged`.
        test-secure-boot-staged = {
          type = "app";
          program = "${checkSbStaged}/bin/test-secure-boot-staged";
        };
        # Standalone runtime SB-install orchestrators (#57 F6b). Run one to
        # produce the install-runtime-SIGNED disk on its own (the scenario apps
        # run it automatically, but #57 can pre-stage it). The disk is signed by
        # NMBL's install-time path-based code — no signing key in any derivation.
        # `nix run .#sb-install-test-secure-boot -- --ssh-key <key> [--keys-dir <dir>]`.
        sb-install-test-secure-boot = {
          type = "app";
          program = "${secureBootInstaller}/bin/sb-install-test-secure-boot";
        };
        sb-install-test-secure-boot-enroll = {
          type = "app";
          program = "${secureBootEnrollInstaller}/bin/sb-install-test-secure-boot-enroll";
        };
        sb-install-test-secure-boot-driver = {
          type = "app";
          program = "${secureBootDriverInstaller}/bin/sb-install-test-secure-boot-driver";
        };
        sb-install-test-secure-boot-staged = {
          type = "app";
          program = "${secureBootStagedInstaller}/bin/sb-install-test-secure-boot-staged";
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
        # INSECURE TEST-ONLY signing keypair glue (#56): the baked PUBLIC key and
        # `assertAbsentFromClosure` (the build check that a signing PRIVATE key is
        # never a derivation input). No private-key-importing signer — test
        # artifacts are signed at INSTALL RUNTIME, never in a derivation.
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
