# NMBL Testing Infrastructure

This directory contains the modular testing infrastructure for NMBL bootloader.

## Quick Start

Run tests with a single command:

```bash
# Direct kernel boot (fast testing)
nix run .#test-test-mbr-serial-direct
nix run .#test-test-gpt-bios-direct
nix run .#test-test-gpt-uefi-direct

# UEFI boot (full boot chain)
nix run .#test-test-gpt-uefi-uefi
```

Everything is built and prepared automatically!

## What Happens

When you run `nix run .#test-test-mbr-serial-direct`:

1. **Builds kernel** - NMBL kernel with required modules
2. **Builds initramfs** - NMBL initramfs with init scripts
3. **Prepares disk** - Creates qcow2 disk image (2GB) if needed
4. **Starts VM** - Launches vm-serial-man with direct kernel boot
5. **Shows console** - Serial output appears in terminal

## Interaction

While VM is running, open another terminal:

```bash
# Send commands
vm-serial-man send 'ls -la /boot'
vm-serial-man send 'cat /proc/cmdline'
vm-serial-man send 'uname -a'

# Check status
vm-serial-man status

# Stop VM
vm-serial-man stop
```

Or press `Ctrl-C` in the VM terminal.

## Available Tests

| Command | Boot Mode | Configuration | Use Case |
|---------|-----------|---------------|----------|
| `test-test-mbr-serial-direct` | Direct Kernel | MBR | Fast init script testing |
| `test-test-gpt-bios-direct` | Direct Kernel | GPT-BIOS | Fast init script testing |
| `test-test-gpt-uefi-direct` | Direct Kernel | GPT-UEFI | Fast init script testing |
| `test-test-gpt-uefi-uefi` | UEFI/OVMF | GPT-UEFI | Full boot chain testing |

## File Structure

```
testing/
├── README.md                    # This file
├── vm-config.nix                # VM configuration builder (<100 lines)
├── test-runners.nix             # Test runner script builders (<110 lines)
└── build_configurations.nix     # Combines everything (<40 lines)
```

Each file has a single focus and is under 200 lines.

### vm-config.nix

Builds NixOS system configurations for testing:
- NMBL bootloader configuration
- Kernel modules
- Serial console setup
- Minimal system packages

### test-runners.nix

Creates executable scripts that:
- Build and link kernel/initramfs
- Create disk images
- Launch vm-serial-man with correct parameters

### build_configurations.nix

Combines VM configs and defines test configurations:
- test-mbr-serial
- test-gpt-bios
- test-gpt-uefi

## Workflow Examples

### Quick Init Script Testing

```bash
# 1. Make changes to init script
vim ../lib/config.nix

# 2. Test immediately (rebuilds kernel/initrd automatically)
nix run .#test-test-mbr-serial-direct

# 3. Observe output
# 4. Press Ctrl-C to stop
# 5. Repeat
```

### Full Boot Chain Testing

```bash
# 1. First test with direct boot to install system
nix run .#test-test-gpt-uefi-direct
# Wait for boot, then Ctrl-C

# 2. Now test UEFI boot
nix run .#test-test-gpt-uefi-uefi
# Observe OVMF → GRUB → NMBL → System boot chain
```

### Multi-terminal Testing

Terminal 1:
```bash
nix run .#test-test-mbr-serial-direct
```

Terminal 2:
```bash
# Wait for boot prompt
vm-serial-man send 'lsblk'
vm-serial-man send 'mount | grep /mnt'
vm-serial-man send 'cat /boot/nmbl-menu.txt'
```

## Disk Images

Test runs create persistent disk images in current directory:

```
test-mbr-serial.qcow2         # MBR test disk
test-gpt-bios.qcow2           # GPT-BIOS test disk
test-gpt-uefi.qcow2           # GPT-UEFI test disk
test-gpt-uefi_OVMF_VARS.fd    # UEFI variables
```

Delete these to start fresh:
```bash
rm -f *.qcow2 *_OVMF_VARS.fd
```

## Artifacts Location

Build artifacts are symlinked to:
```
.nmbl-test-<config>/
├── kernel -> /nix/store/.../bzImage
└── initrd -> /nix/store/.../initrd
```

## Troubleshooting

### "error: path does not exist"

Files not in git. Add them:
```bash
git add testing/*.nix
```

### VM doesn't boot

Check serial output for:
- Missing kernel modules
- Filesystem mount errors
- Device path mismatches

### Changes not appearing

Nix automatically rebuilds. If still not working:
```bash
# Clear nix build cache
nix-collect-garbage
# Delete disk and retry
rm -f test-*.qcow2
```

## Integration with vm-serial-man-rs

The test runners use `vm-serial-man-rs` from the parent directory:
```
../vm-serial-man-rs
```

It's automatically included via flake input:
```nix
inputs.vm-serial-man.url = "path:../vm-serial-man-rs";
```

## Adding New Tests

1. Edit `build_configurations.nix` to add new config:
```nix
configs = {
  test-my-config = {
    name = "test-my-config";
    bootMode = "mbr";  # or "gpt-bios", "gpt-uefi"
  };
};
```

2. Test appears automatically:
```bash
nix run .#test-test-my-config-direct
```

## Performance

Direct kernel boot is **5-10x faster** than traditional VM testing:
- Traditional: ~40-75 seconds per iteration
- Direct boot: ~7-13 seconds per iteration

Perfect for rapid development!