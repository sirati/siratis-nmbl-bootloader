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
      # Apps: legacy names first, then the three-axis matrix grafted
      # on. Legacy names keep working unchanged (operators may have
      # them in muscle memory). New matrix apps use the dotted
      # `<start>-<target>-<interaction>` naming.
      apps.${system} = testApps // {
        test-rescue-ssh = rescueVmTestApp;
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
