#!/usr/bin/env python3
"""Build a BIOS-bootable GPT disk without root privileges.

Layout mirrors the consumer DNS-VPS disko: p1 BIOS boot (GRUB core.img),
p2 vfat `disk-main-boot`, p3 ext4 `disk-main-persistent`. GRUB is embedded
the way grub-bios-setup does it: boot.img in the MBR code area pointing at
core.img in the BIOS boot partition, whose blocklist covers the rest of it.
"""

import argparse
import os
import struct
import subprocess
from pathlib import Path

SECTOR = 512
BIOS_START = 2048          # 1 MiB
BIOS_SECTORS = 2048        # 1 MiB
BOOT_START = 4096          # 2 MiB
BOOT_SECTORS = 128 * 2048  # 128 MiB


def run(*argv, **kw):
    subprocess.run(argv, check=True, **kw)


def copy_into(part, out, start):
    with part.open("rb") as src, out.open("r+b") as dst:
        dst.seek(start * SECTOR)
        while chunk := src.read(1 << 24):
            dst.write(chunk)
    part.unlink()


def tree_mib(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            total += os.lstat(os.path.join(root, name)).st_size
    return total // (1024 * 1024) + 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--grub", required=True, help="grub2 package (bios)")
    parser.add_argument("--out", required=True)
    parser.add_argument("--boot", required=True)
    parser.add_argument("--persistent", required=True)
    args = parser.parse_args()
    out = Path(args.out)
    work = out.parent

    persist_mib = 2 * tree_mib(args.persistent) + 512
    persist_start = BOOT_START + BOOT_SECTORS
    persist_sectors = persist_mib * 2048
    total_sectors = persist_start + persist_sectors + 2048
    with out.open("wb") as f:
        f.truncate(total_sectors * SECTOR)
    run("sgdisk",
        f"-n1:{BIOS_START}:{BIOS_START + BIOS_SECTORS - 1}", "-t1:EF02", "-c1:disk-main-bios",
        f"-n2:{BOOT_START}:{BOOT_START + BOOT_SECTORS - 1}", "-t2:EF00", "-c2:disk-main-boot",
        f"-n3:{persist_start}:{persist_start + persist_sectors - 1}", "-t3:8300", "-c3:disk-main-persistent",
        str(out), stdout=subprocess.DEVNULL)

    # vfat /boot (FAT32 like the consumer's ESP; one-sector clusters keep a
    # 128 MiB partition above the FAT32 cluster minimum), populated with mtools.
    boot_part = work / "boot.vfat"
    with boot_part.open("wb") as f:
        f.truncate(BOOT_SECTORS * SECTOR)
    run("mkfs.vfat", "-F", "32", "-s", "1", "-n", "NMBLBOOT", str(boot_part),
        stdout=subprocess.DEVNULL)
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
    run("mcopy", "-s", "-i", str(boot_part),
        *[str(p) for p in Path(args.boot).iterdir()], "::/", env=env)
    copy_into(boot_part, out, BOOT_START)

    # ext4 /persistent from the prepared tree; the rescue SSH host key must be
    # root-owned, which mkfs.ext4 -d cannot express for an unprivileged build.
    part = work / "persistent.ext4"
    with part.open("wb") as f:
        f.truncate(persist_sectors * SECTOR)
    run("mkfs.ext4", "-q", "-F", "-L", "NMBLSTORE", "-d", args.persistent, str(part))
    for node in ("/", "/rescue-host-ed25519", "/rescue-host-ed25519.pub"):
        for field in ("uid", "gid"):
            run("debugfs", "-w", "-R", f"set_inode_field {node} {field} 0", str(part),
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    run("e2fsck", "-fn", str(part), stdout=subprocess.DEVNULL)
    copy_into(part, out, persist_start)

    # GRUB: core.img into the BIOS boot partition, boot.img into the MBR.
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
    print(f"built {out}: GRUB core {sectors} sectors, /persistent {persist_mib} MiB")


if __name__ == "__main__":
    main()
