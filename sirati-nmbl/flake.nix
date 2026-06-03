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

      # The disko disk-image build installs NMBL with signing DEFERRED (the
      # impure keys are unreadable inside the sealed build VM — see
      # boot.nmbl.signing.deferInstallSigning). We finish the install HERE, on
      # the host, where the committed INSECURE-TEST keys are available: sign
      # every generation's kernel/initrd (ML-DSA, the baked trust anchor's
      # private half) and `sbsign` the NMBL UKI with the test `db` key, then
      # write the artifacts onto the disk's (unencrypted) ESP. This is the
      # RUNTIME-install equivalent for a config the scenarios boot as a
      # prebuilt disk; the store-imported keys are fine here because this is a
      # TEST disk, NOT a production NMBL closure (the prod closure-leak guard
      # `insecure-test-key-absent` still holds for production configs).
      secureBootSignedDisk =
        let
          unsignedDisk = secureBootConfig.config.system.build.vmDiskImage;
          toplevel = secureBootConfig.config.system.build.toplevel;
          uki = secureBootConfig.config.system.build.nmblUki;
          mlDsaKey = testKeys.privateKey;
          sbKey = ./testing/keys/insecure-test-sb-db.key;
          sbCert = ./testing/keys/insecure-test-sb-db.crt;
          sigSuffix = secureBootConfig.config.boot.nmbl.signing.sigPathSuffix;
        in
        pkgs.runCommand "test-secure-boot-signed-disk-image"
          {
            nativeBuildInputs = [
              pkgs.qemu-utils
              pkgs.libguestfs-with-appliance
              pkgs.sbsigntool
              pkgs.coreutils
            ] ++ lib.optional (nmblSign != null) nmblSign;
          }
          ''
            mkdir -p "$out" stage/sigs
            # Writable copy of the deferred (unsigned) image.
            qemu-img convert -f qcow2 -O qcow2 \
              ${unsignedDisk}/nixos.qcow2 "$out/nixos.qcow2"
            chmod u+w "$out/nixos.qcow2"

            # gen-id = file_name(canonicalize(toplevel)) — the SAME content-
            # addressed store basename nmbl-init computes at boot (gen_id.rs /
            # `nmbl-init --print-gen-id`), so the signer and the runtime
            # verifier agree on the sidecar directory.
            gen_id="$(basename "$(readlink -f ${toplevel})")"
            echo "Signing generation $gen_id for the NMBL boot guard (host-side)..."

            # Per-generation ML-DSA sidecars, per-role domains, exactly where
            # src/sig/scan.rs::resolve_sig_sidecar looks:
            #   /nmbl/sigs/<gen-id>/{kernel,initrd}<sigPathSuffix>
            mkdir -p "stage/sigs/$gen_id"
            ${if nmblSign == null then ''
              echo "ERROR: nmbl-sign signer unavailable; cannot sign the test disk." >&2
              exit 1
            '' else ''
              nmbl-sign sign --key ${mlDsaKey} --domain gen-kernel \
                --out "stage/sigs/$gen_id/kernel${sigSuffix}" ${toplevel}/kernel
              nmbl-sign sign --key ${mlDsaKey} --domain gen-initrd \
                --out "stage/sigs/$gen_id/initrd${sigSuffix}" ${toplevel}/initrd
            ''}

            # sbsign the NMBL UKI with the INSECURE-TEST db key so the
            # enforcing SB firmware (whose db we enrolled this cert into)
            # launches it (audit F1).
            echo "Signing the NMBL UKI for Secure Boot (sbsign, host-side)..."
            sbsign --key ${sbKey} --cert ${sbCert} \
              --output stage/BOOTX64.EFI ${uki}
            sbverify --cert ${sbCert} stage/BOOTX64.EFI

            # Write the signed UKI + sidecars onto the (unencrypted) ESP
            # (disk-main-ESP = partition 2 in the disko layout).
            echo "Writing signed artifacts onto the ESP..."
            export LIBGUESTFS_BACKEND=direct
            guestfish --rw -a "$out/nixos.qcow2" <<EOF
            run
            mount /dev/sda2 /
            mkdir-p /EFI/BOOT
            upload stage/BOOTX64.EFI /EFI/BOOT/BOOTX64.EFI
            mkdir-p /nmbl/sigs/$gen_id
            upload stage/sigs/$gen_id/kernel${sigSuffix} /nmbl/sigs/$gen_id/kernel${sigSuffix}
            upload stage/sigs/$gen_id/initrd${sigSuffix} /nmbl/sigs/$gen_id/initrd${sigSuffix}
            umount /
            EOF
            echo "✓ test-secure-boot disk signed: UKI sbsign'd, generation $gen_id ML-DSA sidecars staged."
          '';

      # The real (tpm-unlock) config's host-signed NMBL UKI, as a standalone
      # PE. The TPM-roundtrip needs it to power-cycle the SAME disk (with the
      # token the enroll phase sealed onto vda3) into the tpm-unlock NMBL stage:
      # the enroll twin and the real config differ ONLY in their embedded NMBL
      # initrd (password vs token unlock), and for an efi-stub loader that
      # config lives inside the UKI. Swapping just `/EFI/BOOT/BOOTX64.EFI` on the
      # shared disk therefore switches phase 2 to tpm-unlock while keeping the
      # enrolled LUKS2 token on vda3 intact.
      secureBootSignedUki =
        let
          uki = secureBootConfig.config.system.build.nmblUki;
          sbKey = ./testing/keys/insecure-test-sb-db.key;
          sbCert = ./testing/keys/insecure-test-sb-db.crt;
        in
        pkgs.runCommand "test-secure-boot-signed-uki"
          {
            nativeBuildInputs = [ pkgs.sbsigntool pkgs.coreutils ];
          }
          ''
            mkdir -p "$out"
            sbsign --key ${sbKey} --cert ${sbCert} \
              --output "$out/BOOTX64.EFI" ${uki}
            sbverify --cert ${sbCert} "$out/BOOTX64.EFI"
          '';

      # The config the scenarios boot/tamper: identical to secureBootConfig but
      # with vmDiskImage pointing at the HOST-SIGNED disk, so the runner copies
      # the signed qcow2 and the bad-sig scenario tampers a signed image.
      secureBootSignedConfig = secureBootConfig // {
        config = secureBootConfig.config // {
          system = secureBootConfig.config.system // {
            build = secureBootConfig.config.system.build // {
              vmDiskImage = secureBootSignedDisk;
            };
          };
        };
      };

      secureBootRunner = testRunners.mkRunner {
        name = "test-secure-boot";
        config = secureBootSignedConfig;
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
        in
        testing.mkTestVM (baseCfg // {
          name = "test-secure-boot-enroll";
          extraModules = (baseCfg.extraModules or [ ]) ++ [ enrollOverride ];
        });

      # Host-sign the enroll twin's disk the same way the real config's disk is
      # signed (UKI sbsign + per-generation ML-DSA sidecars on the ESP), so the
      # enforcing firmware launches its UKI and NMBL's verify guard passes.
      secureBootEnrollSignedDisk =
        let
          unsignedDisk = secureBootEnrollConfig.config.system.build.vmDiskImage;
          toplevel = secureBootEnrollConfig.config.system.build.toplevel;
          uki = secureBootEnrollConfig.config.system.build.nmblUki;
          mlDsaKey = testKeys.privateKey;
          sbKey = ./testing/keys/insecure-test-sb-db.key;
          sbCert = ./testing/keys/insecure-test-sb-db.crt;
          sigSuffix = secureBootEnrollConfig.config.boot.nmbl.signing.sigPathSuffix;
        in
        pkgs.runCommand "test-secure-boot-enroll-signed-disk-image"
          {
            nativeBuildInputs = [
              pkgs.qemu-utils
              pkgs.libguestfs-with-appliance
              pkgs.sbsigntool
              pkgs.coreutils
            ] ++ lib.optional (nmblSign != null) nmblSign;
          }
          ''
            mkdir -p "$out" stage/sigs
            qemu-img convert -f qcow2 -O qcow2 \
              ${unsignedDisk}/nixos.qcow2 "$out/nixos.qcow2"
            chmod u+w "$out/nixos.qcow2"

            gen_id="$(basename "$(readlink -f ${toplevel})")"
            echo "Signing enroll-twin generation $gen_id (host-side)..."
            mkdir -p "stage/sigs/$gen_id"
            ${if nmblSign == null then ''
              echo "ERROR: nmbl-sign signer unavailable; cannot sign the enroll disk." >&2
              exit 1
            '' else ''
              nmbl-sign sign --key ${mlDsaKey} --domain gen-kernel \
                --out "stage/sigs/$gen_id/kernel${sigSuffix}" ${toplevel}/kernel
              nmbl-sign sign --key ${mlDsaKey} --domain gen-initrd \
                --out "stage/sigs/$gen_id/initrd${sigSuffix}" ${toplevel}/initrd
            ''}

            echo "sbsign'ing the enroll-twin UKI for Secure Boot..."
            sbsign --key ${sbKey} --cert ${sbCert} \
              --output stage/BOOTX64.EFI ${uki}
            sbverify --cert ${sbCert} stage/BOOTX64.EFI

            export LIBGUESTFS_BACKEND=direct
            guestfish --rw -a "$out/nixos.qcow2" <<EOF
            run
            mount /dev/sda2 /
            mkdir-p /EFI/BOOT
            upload stage/BOOTX64.EFI /EFI/BOOT/BOOTX64.EFI
            mkdir-p /nmbl/sigs/$gen_id
            upload stage/sigs/$gen_id/kernel${sigSuffix} /nmbl/sigs/$gen_id/kernel${sigSuffix}
            upload stage/sigs/$gen_id/initrd${sigSuffix} /nmbl/sigs/$gen_id/initrd${sigSuffix}
            umount /
            EOF
            echo "✓ enroll-twin disk signed."
          '';

      secureBootEnrollSignedConfig = secureBootEnrollConfig // {
        config = secureBootEnrollConfig.config // {
          system = secureBootEnrollConfig.config.system // {
            build = secureBootEnrollConfig.config.system.build // {
              vmDiskImage = secureBootEnrollSignedDisk;
            };
          };
        };
      };

      secureBootEnrollRunner = testRunners.mkRunner {
        name = "test-secure-boot-enroll";
        config = secureBootEnrollSignedConfig;
        inherit vmSerialMan;
        tpm = "tis";
        secureBoot = true;
        # Persist so the token this phase seals survives into the unseal phase.
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
        # The roundtrip drives TWO runners against one persisted swtpm: the
        # passphrase-unlock enroll twin (phase 1, seals the token onto vda3) and
        # the real tpm-unlock config (phase 2, auto-unseals after the
        # power-cycle). The two phases share ONE disk (the enrolled qcow2);
        # before phase 2 the script swaps that disk's ESP UKI for the real
        # tpm-unlock UKI (NMBL_SB_TPM_UKI) so the token on vda3 stays intact
        # while the NMBL stage switches to token unlock — needs libguestfs.
        extraInputs = [ pkgs.libguestfs-with-appliance ];
        extraEnv = ''
          export NMBL_ENROLL_RUNNER=${secureBootEnrollRunner}
          export NMBL_SB_TPM_UKI=${secureBootSignedUki}/BOOTX64.EFI
        '';
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
          export NMBL_SB_DISK=${secureBootSignedConfig.config.system.build.vmDiskImage}/nixos.qcow2
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
        # The fully-signed test-secure-boot disk the chain scenarios boot: the
        # disko image (NMBL installed, signing deferred) with the UKI sbsign'd
        # and the generation ML-DSA sidecars written onto the ESP host-side.
        # `nix build .#test-secure-boot-disk`.
        test-secure-boot-disk = secureBootSignedDisk;
        # The TPM-roundtrip ENROLL twin's signed disk: same generation as
        # test-secure-boot but with a passphrase cryptroot so phase 1 of the
        # roundtrip can boot into the system and run nmbl-tpm-enroll.
        # `nix build .#test-secure-boot-enroll-disk`.
        test-secure-boot-enroll-disk = secureBootEnrollSignedDisk;
        # The real (tpm-unlock) config's host-signed NMBL UKI, swapped onto the
        # shared disk's ESP for the roundtrip's phase 2.
        # `nix build .#test-secure-boot-signed-uki`.
        test-secure-boot-signed-uki = secureBootSignedUki;
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
