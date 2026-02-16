# NMBL - NixOS Minimal BootLoader

A Linux-as-bootloader implementation for NixOS that uses kexec to boot into different NixOS generations.

## Project Structure

This project is organized into modular components for easy maintenance and understanding:

```
sirati-nmbl/
├── flake.nix              # Main flake entry point
├── lib/
│   ├── options.nix        # NixOS module options definitions
│   └── config.nix         # NixOS module configuration implementation
├── scripts/
│   ├── script.nix         # Main script builder (combines all parts)
│   ├── mount-and-kernel.sh.nix    # Part 1: Filesystem mounting & kernel modules
│   ├── find-generations.sh.nix    # Part 2: NixOS generation discovery
│   ├── selection-ui.sh.nix        # Part 3: Interactive selection menu
│   └── kexec-boot.sh.nix          # Part 4: Kexec execution
└── example.nix            # Example configuration
```

### Architecture

The bootloader works in four stages:

1. **Mount and Kernel Module Loading** (`mount-and-kernel.sh.nix`)
   - Mounts essential filesystems (proc, sys, dev)
   - Loads required kernel modules for storage access
   - Mounts configured filesystems (typically the root partition)

2. **Find NixOS Generations** (`find-generations.sh.nix`)
   - Discovers all available NixOS system generations
   - Reads kernel parameters for each generation
   - Validates that generations have required files (kernel, initrd)

3. **Interactive Selection UI** (`selection-ui.sh.nix`)
   - Presents a menu of available generations
   - Allows toggling kernel parameter passthrough
   - Supports custom kernel parameter editing
   - Auto-boots default after timeout

4. **Kexec Boot Execution** (`kexec-boot.sh.nix`)
   - Loads selected kernel and initrd
   - Constructs final kernel command line
   - Unmounts filesystems cleanly
   - Performs kexec into selected generation

### Script Structure

Each `.sh.nix` file is a Nix expression that returns a shell script string:

```nix
# Example structure
{
  lib,
  pkgs,
  cfg,
}:

''
  # Shell script code here
  echo "This is part of the init script"
  ${lib.concatStringsSep "\n" cfg.someList}
''
```

The `scripts/script.nix` file imports all parts and combines them into a single init script with proper shebang and error handling.

## Usage

### In a Flake

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nmbl.url = "path:./sirati-nmbl";  # or your preferred source
  };

  outputs = { self, nixpkgs, nmbl }: {
    nixosConfigurations.mySystem = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        nmbl.nixosModules.default
        {
          boot.nmbl = {
            enable = true;
            bootMode = "gpt-uefi";
            
            kernelModules = [
              "ext4"
              "nvme"
            ];
            
            fileSystems."/mnt-root" = {
              device = "/dev/nvme0n1p2";
              fsType = "ext4";
              options = [ "ro" ];
            };
          };
          
          # Your other configuration...
        }
      ];
    };
  };
}
```

### Configuration Options

See `lib/options.nix` for all available options:

- `boot.nmbl.enable` - Enable the bootloader
- `boot.nmbl.bootMode` - Boot mode: "mbr", "gpt-bios", or "gpt-uefi"
- `boot.nmbl.kernelPackage` - Pinned kernel for the bootloader
- `boot.nmbl.kernelModules` - Kernel modules to load in initramfs
- `boot.nmbl.fileSystems` - Filesystems to mount
- `boot.nmbl.kernelParams` - Bootloader-specific kernel parameters
- `boot.nmbl.timeoutSeconds` - Auto-boot timeout
- `boot.nmbl.serialConsole` - Serial console configuration

## Testing

### Quick Testing with `nix run` (Recommended)

Everything is automated - just run:

```bash
# Direct kernel boot (fast testing - 5-10x faster)
nix run .#test-test-mbr-serial-direct
nix run .#test-test-gpt-bios-direct
nix run .#test-test-gpt-uefi-direct

# UEFI boot (full boot chain testing)
nix run .#test-test-gpt-uefi-uefi
```

**What happens automatically:**
1. Builds NMBL kernel and initramfs
2. Creates disk image (if needed)
3. Starts VM with vm-serial-man
4. Shows serial console output

**Interact with running VM:**
```bash
# In another terminal
vm-serial-man send 'ls -la /boot'
vm-serial-man send 'cat /proc/cmdline'
vm-serial-man status
vm-serial-man stop
```

See [testing/README.md](./testing/README.md) for complete testing guide.

### Alternative: Manual Testing Scripts

For more control, use the shell scripts:

```bash
# Quick demo
./demo-direct-kernel.sh

# Full testing with specific config
./test-direct-kernel.sh test-mbr-serial
./test-uefi-boot.sh test-gpt-uefi
```

See [TESTING-WITH-VM-SERIAL-MAN.md](./TESTING-WITH-VM-SERIAL-MAN.md) for comprehensive guide.

### Traditional VM Testing

Build and run test VMs (old method):

```bash
# Build VM
nix build .#nixosConfigurations.test-mbr-serial.config.system.build.vm

# Run VM
./result/bin/run-test-mbr-serial-vm

# SSH into VM (from another terminal)
ssh -p 2222 root@localhost  # password: test
```

See [debug.md](./debug.md) for more details.

## Development

### Adding New Script Components

To add a new script component:

1. Create a new `.sh.nix` file in `scripts/` that returns a string
2. Import it in `scripts/script.nix`
3. Add it to the concatenation in the appropriate location

Example:

```nix
# scripts/my-feature.sh.nix
{ lib, pkgs, cfg }:

''
  # My feature code
  echo "Doing something..."
''
```

Then in `scripts/script.nix`:

```nix
let
  myFeatureScript = import ./my-feature.sh.nix { inherit lib pkgs cfg; };
in
pkgs.writeScript "init" ''
  #!${pkgs.busybox}/bin/sh
  set -e
  
  ${mountAndKernelScript}
  ${myFeatureScript}  # Add your component
  ${findGenerationsScript}
  # ...
''
```

### Building Individual Components

Build specific parts:

```bash
# Build NMBL kernel
nix build .#nixosConfigurations.test-mbr-serial.config.system.build.nmblKernel

# Build NMBL initramfs
nix build .#nixosConfigurations.test-mbr-serial.config.system.build.nmblInitramfs

# Build bootloader installation script
nix build .#nixosConfigurations.test-mbr-serial.config.system.build.installBootLoader

# Check the generated init script
nix eval .#nixosConfigurations.test-mbr-serial.config.system.build.nmblInitramfs --apply 'x: x.contents'
```

## Design Decisions


### Why busybox?

The bootloader needs to be minimal and fast. Busybox provides:
- Small footprint
- All essential Unix utilities
- Single binary for easy initramfs inclusion

## License

MIT License (the scripts here, the generated content has various licenses)
