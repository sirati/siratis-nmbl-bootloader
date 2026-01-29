# NMBL Architecture Documentation

## System Overview

```
┌─────────────────────────────────────────────────────────────┐
│                         BIOS/UEFI                           │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                    NMBL Kernel (Linux)                      │
│                  (Minimal, Pinned Version)                  │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                   NMBL Initramfs                            │
│  ┌───────────────────────────────────────────────────────┐  │
│  │              Init Script (Busybox)                    │  │
│  │                                                       │  │
│  │  1. Mount & Load Modules                             │  │
│  │  2. Find NixOS Generations                           │  │
│  │  3. Interactive Selection UI                         │  │
│  │  4. Kexec to Selected Generation                     │  │
│  └───────────────────────────────────────────────────────┘  │
└──────────────────────┬──────────────────────────────────────┘
                       │ (kexec)
                       ▼
┌─────────────────────────────────────────────────────────────┐
│              Selected NixOS Generation                      │
│                 (Full System Kernel)                        │
└─────────────────────────────────────────────────────────────┘
```

## File Structure and Relationships

```
┌──────────────────────────────────────────────────────────────────┐
│                           flake.nix                              │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │  - Flake inputs (nixpkgs)                                  │  │
│  │  - Exports nixosModules.default                            │  │
│  │  - Exports example configuration                           │  │
│  └───────────┬─────────────────────┬──────────────────────────┘  │
└──────────────┼─────────────────────┼─────────────────────────────┘
               │                     │
               ▼                     ▼
    ┌──────────────────┐   ┌──────────────────┐
    │  lib/options.nix │   │  lib/config.nix  │
    └──────────────────┘   └─────────┬────────┘
           │                          │
           │                          │ imports
           │                          ▼
           │              ┌─────────────────────────┐
           │              │  scripts/script.nix     │
           │              └──────────┬──────────────┘
           │                         │ combines
           │                         │
           │         ┌───────────────┼───────────────┬───────────────┐
           │         │               │               │               │
           │         ▼               ▼               ▼               ▼
           │  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
           │  │mount-and-   │ │find-        │ │selection-   │ │kexec-boot   │
           │  │kernel.sh.nix│ │generations  │ │ui.sh.nix    │ │.sh.nix      │
           │  │             │ │.sh.nix      │ │             │ │             │
           │  └─────────────┘ └─────────────┘ └─────────────┘ └─────────────┘
           │         │               │               │               │
           │         └───────────────┴───────────────┴───────────────┘
           │                         │
           │                         ▼
           │              ┌─────────────────────────┐
           │              │  Final Init Script      │
           │              │  (Shell String)         │
           │              └─────────────────────────┘
           │
           └─────────────────────────────────────────────────────────┐
                                                                     │
                                                                     ▼
                                                           ┌──────────────────┐
                                                           │   User Config    │
                                                           │  (imports module)│
                                                           └──────────────────┘
```

## Boot Flow Sequence

```
┌─────────────┐
│  Power On   │
└──────┬──────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│ BIOS/UEFI loads NMBL kernel + initramfs                     │
└──────┬──────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│ Phase 1: Mount and Kernel Module Loading                    │
│                                                              │
│  • mount /proc, /sys, /dev                                  │
│  • modprobe configured kernel modules                       │
│  • mount root filesystem (read-only)                        │
│                                                              │
│  Success? ──No──> Drop to emergency shell                   │
│     │                                                        │
│    Yes                                                       │
└─────┴──────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│ Phase 2: Find NixOS Generations                             │
│                                                              │
│  • Scan /nix/var/nix/profiles/system-*-link                 │
│  • Read kernel, initrd, kernel-params from each             │
│  • Build arrays of available generations                    │
│                                                              │
│  Found? ──No──> Drop to emergency shell                     │
│     │                                                        │
│    Yes                                                       │
└─────┴──────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│ Phase 3: Interactive Selection UI                           │
│                                                              │
│  ┌────────────────────────────────────────────────────┐    │
│  │ === NixOS Linux Bootloader ===                     │    │
│  │                                                     │    │
│  │ Available Generations:                             │    │
│  │   [0] Generation current                           │    │
│  │   [1] Generation 123                               │    │
│  │   [2] Generation 122                               │    │
│  │                                                     │    │
│  │ [X] Passthrough kernel params                      │    │
│  │                                                     │    │
│  │ Commands: 0-9, p, e, s                             │    │
│  │ Select option (auto-boot 0 in 3s):                 │    │
│  └────────────────────────────────────────────────────┘    │
│                                                              │
│  User input ──or── Timeout after N seconds                  │
└──────┬───────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│ Phase 4: Kexec Boot Execution                               │
│                                                              │
│  • Load selected generation's kernel                        │
│  • Load selected generation's initrd                        │
│  • Construct final kernel command line:                     │
│    - Passthrough params (optional)                          │
│    - Generation-specific params                             │
│    - Custom params (if edited)                              │
│  • Unmount filesystems                                      │
│  • sync                                                      │
│  • kexec -e                                                 │
└──────┬──────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│ Selected NixOS Generation Boots                             │
│                                                              │
│  • New kernel starts                                        │
│  • New initrd runs                                          │
│  • Normal NixOS boot continues                              │
└─────────────────────────────────────────────────────────────┘
```

## Data Flow

```
┌───────────────────────────────────────────────────────────────┐
│                   Configuration Input                         │
│                                                               │
│  boot.nmbl = {                                                │
│    enable = true;                                             │
│    kernelModules = [ "ext4" "nvme" ];                         │
│    fileSystems."/mnt-root" = { ... };                         │
│    kernelParams = [ "console=ttyS0" ];                        │
│    timeoutSeconds = 3;                                        │
│  };                                                           │
└───────────────────┬───────────────────────────────────────────┘
                    │
                    ▼
┌───────────────────────────────────────────────────────────────┐
│                   Nix Evaluation                              │
│                                                               │
│  1. lib/options.nix validates options                        │
│  2. lib/config.nix processes configuration                   │
│  3. scripts/script.nix generates init script                 │
│                                                               │
│  Parameters flow:                                             │
│    cfg.kernelModules ──> mount-and-kernel.sh.nix             │
│    cfg.fileSystems   ──> mount-and-kernel.sh.nix             │
│    cfg.kernelParams  ──> selection-ui.sh.nix                 │
│    cfg.timeoutSeconds ──> selection-ui.sh.nix                │
│    cfg.fileSystems   ──> kexec-boot.sh.nix                   │
└───────────────────┬───────────────────────────────────────────┘
                    │
                    ▼
┌───────────────────────────────────────────────────────────────┐
│                   Build Outputs                               │
│                                                               │
│  • system.build.nmblKernel      (kernel package)             │
│  • system.build.nmblInitramfs   (initrd with init script)    │
│  • system.build.nmblBootConfig  (configuration info)         │
│  • system.build.installNmbl     (installation script)        │
└───────────────────┬───────────────────────────────────────────┘
                    │
                    ▼
┌───────────────────────────────────────────────────────────────┐
│                   Installation                                │
│                                                               │
│  install-nmbl /dev/sda                                        │
│    ├─> Installs bootloader (GRUB/syslinux)                   │
│    ├─> Copies kernel to /boot/nmbl-kernel                    │
│    └─> Copies initrd to /boot/nmbl-initrd                    │
└───────────────────────────────────────────────────────────────┘
```

## Module System Integration

```
┌────────────────────────────────────────────────────────────┐
│                    User's configuration.nix                 │
│                                                             │
│  imports = [ nmbl.nixosModules.default ];                  │
│                                                             │
│  boot.nmbl.enable = true;                                  │
│  # ... other NMBL config ...                               │
│                                                             │
│  # Regular NixOS config still works:                       │
│  boot.kernelPackages = pkgs.linuxPackages_latest;          │
│  fileSystems."/" = { device = "/dev/sda1"; };              │
└──────────────────────┬─────────────────────────────────────┘
                       │
                       ▼
┌────────────────────────────────────────────────────────────┐
│                NixOS Module System                          │
│                                                             │
│  • Merges all module options                               │
│  • Validates configuration                                 │
│  • Evaluates config blocks with lib.mkIf                   │
└──────────────────────┬─────────────────────────────────────┘
                       │
                       ▼
┌────────────────────────────────────────────────────────────┐
│              NMBL Module (lib/config.nix)                   │
│                                                             │
│  config = lib.mkIf cfg.enable {                            │
│    # Disables standard bootloaders                         │
│    boot.loader.grub.enable = false;                        │
│    boot.loader.systemd-boot.enable = false;                │
│                                                             │
│    # Builds NMBL components                                │
│    system.build.nmblKernel = ...;                          │
│    system.build.nmblInitramfs = ...;                       │
│                                                             │
│    # Adds tools to system                                  │
│    environment.systemPackages = [ ... ];                   │
│  };                                                         │
└────────────────────────────────────────────────────────────┘
```

## Script Component Architecture

Each `.sh.nix` file follows this pattern:

```nix
# scripts/component-name.sh.nix

{
  lib,      # Nix standard library (for string manipulation, etc)
  pkgs,     # Nixpkgs package set (for paths to binaries)
  cfg,      # User's boot.nmbl configuration
}:

''
  # Shell script code goes here
  # Can use Nix string interpolation:
  
  echo "Configured timeout: ${toString cfg.timeoutSeconds}"
  
  # Can iterate over Nix lists/attrs:
  ${lib.concatMapStringsSep "\n" (mod: 
    "modprobe ${mod}"
  ) cfg.kernelModules}
  
  # Can access package paths:
  ${pkgs.bash}/bin/bash --version
''
```

The script is **not executed** during Nix evaluation. It's a pure string that gets written to the Nix store and becomes part of the initramfs.

## Key Design Principles

1. **Separation of Concerns**: Each component has one job
2. **Build-time Composition**: All string interpolation happens during build
3. **No Runtime Dependencies**: Final script is self-contained
4. **Linear Execution**: No complex function calls or exports
5. **Fail-Safe**: Errors drop to emergency shell for recovery
6. **Minimal Footprint**: Only essential tools in initramfs

## Extension Points

To extend NMBL functionality:

1. **Add a new script phase**: Create `scripts/my-feature.sh.nix`
2. **Modify options**: Edit `lib/options.nix` to add new config
3. **Change build logic**: Edit `lib/config.nix` to modify builds
4. **Alternative UI**: Replace `selection-ui.sh.nix` completely
5. **Custom hooks**: Add script phases between existing ones

Example of adding a pre-boot hook:

```nix
# scripts/pre-boot-hook.sh.nix
{ lib, pkgs, cfg }:

''
  # Run before kexec
  ${lib.optionalString (cfg.preBootHook != null) cfg.preBootHook}
''
```

Then in `scripts/script.nix`:

```nix
${selectionUIScript}
${preBootHookScript}  # <-- Add here
${kexecBootScript}
```

## Performance Characteristics

- **Boot time**: ~2-5 seconds (depends on filesystem scan)
- **Memory usage**: ~50MB for initramfs
- **Disk space**: ~100MB (kernel + initramfs)
- **Generation scan**: O(n) where n = number of generations

The bootloader is designed to be fast and lightweight, suitable for:
- Servers (with serial console support)
- Virtual machines
- Embedded systems
- Development machines