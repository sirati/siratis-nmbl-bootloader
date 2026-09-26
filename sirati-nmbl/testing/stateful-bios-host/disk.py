#!/usr/bin/env python3
"""Build the Hetzner-shape stateful test disk without root privileges.

p1 BIOS boot (GRUB core.img), p2 vfat `disk-main-ESP` (/boot: GRUB, the NMBL
kernel/initrd, external config, rescue image, state.bin), p3 Btrfs
`disk-main-root` with subvolumes `@root` (/) and `@nix` (/nix: the store,
the Nix database and the system-N-link profiles).
"""

import argparse
import os
import struct
import subprocess
from pathlib import Path

SECTOR = 512
BIOS_START, BIOS_SECTORS = 2048, 2048
ESP_START, ESP_SECTORS = 4096, 256 * 2048


def run(*argv, **kw):
    subprocess.run(argv, check=True, **kw)


def copy_into(part, out, start):
    with part.open("rb") as src, out.open("r+b") as dst:
        dst.seek(start * SECTOR)
        while chunk := src.read(1 << 24):
            dst.write(chunk)
    part.unlink()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--grub", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--boot", required=True)
    parser.add_argument("--root", required=True, help="tree with @root and @nix subdirectories")
    args = parser.parse_args()
    out = Path(args.out)
    work = out.parent

    # Btrfs root: --rootdir copies the tree, --subvol turns the two top-level
    # directories into subvolumes, fakeroot makes every file root-owned.
    root_part = work / "root.btrfs"
    root_part.touch()
    run("fakeroot", "mkfs.btrfs", "-q", "-L", "NMBLROOT", "--rootdir", args.root,
        "--subvol", "rw:@root", "--subvol", "rw:@nix", "--shrink", str(root_part))
    extra = 768 * 1024 * 1024
    with root_part.open("r+b") as f:
        f.truncate(os.path.getsize(root_part) + extra)
    root_sectors = os.path.getsize(root_part) // SECTOR
    root_start = ESP_START + ESP_SECTORS
    total = root_start + root_sectors + 2048
    with out.open("wb") as f:
        f.truncate(total * SECTOR)
    run("sgdisk",
        f"-n1:{BIOS_START}:{BIOS_START + BIOS_SECTORS - 1}", "-t1:EF02", "-c1:disk-main-bios",
        f"-n2:{ESP_START}:{ESP_START + ESP_SECTORS - 1}", "-t2:EF00", "-c2:disk-main-ESP",
        f"-n3:{root_start}:{root_start + root_sectors - 1}", "-t3:8300", "-c3:disk-main-root",
        str(out), stdout=subprocess.DEVNULL)
    copy_into(root_part, out, root_start)

    esp = work / "esp.vfat"
    with esp.open("wb") as f:
        f.truncate(ESP_SECTORS * SECTOR)
    run("mkfs.vfat", "-F", "32", "-s", "1", "-n", "NMBLESP", str(esp), stdout=subprocess.DEVNULL)
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
    run("mcopy", "-s", "-i", str(esp), *[str(p) for p in Path(args.boot).iterdir()], "::/", env=env)
    copy_into(esp, out, ESP_START)

    platform = Path(args.grub) / "lib/grub/i386-pc"
    core = work / "core.img"
    run(str(Path(args.grub) / "bin/grub-mkimage"), "-O", "i386-pc", "-d", str(platform),
        "-o", str(core), "-p", "(hd0,gpt2)/grub",
        "biosdisk", "part_gpt", "fat", "normal", "linux", "echo", "serial",
        "terminal", "test", "search", "configfile")
    core_bytes = bytearray(core.read_bytes())
    sectors = (len(core_bytes) + SECTOR - 1) // SECTOR
    if sectors > BIOS_SECTORS:
        raise SystemExit("core.img does not fit the BIOS boot partition")
    _start, _length, segment = struct.unpack_from("<QHH", core_bytes, SECTOR - 12)
    struct.pack_into("<QHH", core_bytes, SECTOR - 12, BIOS_START + 1, sectors - 1, segment)
    boot_img = (platform / "boot.img").read_bytes()
    with out.open("r+b") as f:
        mbr = bytearray(f.read(SECTOR))
        mbr[0:440] = boot_img[0:440]
        struct.pack_into("<Q", mbr, 0x5C, BIOS_START)
        f.seek(0)
        f.write(mbr)
        f.seek(BIOS_START * SECTOR)
        f.write(core_bytes)
    core.unlink()
    print(f"built {out}: Btrfs root {root_sectors // 2048} MiB")


if __name__ == "__main__":
    main()
