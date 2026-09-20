#!/usr/bin/env python3
"""Drive NMBL's direct-kernel rescue VM and scan binary artifacts."""

import argparse
import os
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path


def files_below(target: Path):
    if target.is_file():
        yield target
        return
    for root, _, names in os.walk(target):
        for name in names:
            path = Path(root, name)
            if path.is_file() and not path.is_symlink():
                yield path


def contains(path, needles):
    overlap = max(map(len, needles)) - 1
    previous = b""
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            data = previous + chunk
            if any(needle in data for needle in needles):
                return True
            previous = data[-overlap:] if overlap else b""
    return False


def scan(args):
    secret = Path(args.key).read_bytes()
    needles = [secret, secret[9:], args.marker.encode()]
    targets = list(args.targets)
    if args.target_list:
        targets.extend(Path(args.target_list).read_text().splitlines())
    for target_name in targets:
        for path in files_below(Path(target_name)):
            try:
                leaked = contains(path, needles)
            except OSError:
                continue
            if leaked:
                raise SystemExit(f"private signing key escaped into {path}")


def wait_for(proc, patterns, timeout, transcript):
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    seen = bytearray()
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"QEMU exited {proc.returncode}:\n{seen[-12000:].decode(errors='replace')}")
        for key, _ in selector.select(timeout=1):
            chunk = os.read(key.fd, 65536)
            if not chunk:
                continue
            transcript.write(chunk)
            transcript.flush()
            seen.extend(chunk)
            if os.environ.get("NMBL_VM_VERBOSE") == "1":
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
        text = seen.decode(errors="replace")
        if all(pattern in text for pattern in patterns):
            return text
    raise RuntimeError(f"timed out waiting for {patterns}:\n{seen[-12000:].decode(errors='replace')}")


def qemu_command(args, disk):
    return [
        args.qemu,
        "-machine", "q35,accel=tcg",
        "-cpu", "max",
        "-m", "3072",
        "-smp", "2",
        "-kernel", args.kernel,
        "-initrd", args.initrd,
        "-append", "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
        "-drive", f"file={disk},format=raw,if=virtio,readonly=on",
        "-netdev", "user,id=net0,ipv6=on",
        "-device", "virtio-net-pci,netdev=net0",
        "-display", "none",
        "-serial", "stdio",
        "-monitor", "none",
        "-no-reboot",
    ]


def boot(args):
    print(f"booting {args.mode} VM from {args.disk}", flush=True)
    transcript_path = Path(args.transcript)
    with transcript_path.open("wb") as transcript:
        proc = subprocess.Popen(
            qemu_command(args, args.disk),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            if args.mode == "good":
                wait_for(proc, ["recovery system ready"], 240, transcript)
                commands = r'''
set -eux
findmnt -n -o OPTIONS /nmbl-network | grep -w ro | grep -w nodev | grep -w nosuid | grep -w noexec
test -d /nmbl-network/lib/modules/$(uname -r)
test -d /nmbl-network/lib/firmware
grep -qx 'address-family dual-stack' /nmbl-network/etc/nmbl-network/network.conf
lsmod | grep '^dummy '
cmp /etc/ssh/ssh_host_ed25519_key /nmbl-root/mnt/boot/rescue-host-ed25519
test "$(stat -c '%u:%g:%a' /nmbl-root/mnt/boot/rescue-host-ed25519)" = 0:0:600
sshd -T -f /etc/ssh/sshd_config | grep -qx 'passwordauthentication no'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'kbdinteractiveauthentication no'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'authenticationmethods publickey'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'disableforwarding yes'
ss -tln | grep ':22222 '
ip -4 addr show dev eth0 | grep 'inet '
ip -6 addr show dev eth0 | grep 'inet6 '
echo NMBL_NETWORK_STAGE_VM_PASS
'''
                proc.stdin.write(commands.encode())
                proc.stdin.flush()
                text = wait_for(proc, ["NMBL_NETWORK_STAGE_VM_PASS"], 90, transcript)
                if "network-stage signature" in text.lower() and "failed" in text.lower():
                    raise RuntimeError("positive VM reported a network-stage signature failure")
            else:
                text = wait_for(
                    proc,
                    ["network stage rejected", "local console only"],
                    180,
                    transcript,
                )
                if "recovery system ready" in text:
                    raise RuntimeError("invalid network stage reached the rescue system")
                proc.stdin.write(b"echo NMBL_LOCAL_CONSOLE_PASS\n")
                proc.stdin.flush()
                wait_for(proc, ["NMBL_LOCAL_CONSOLE_PASS"], 30, transcript)
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    scan_parser = sub.add_parser("scan")
    scan_parser.add_argument("--key", required=True)
    scan_parser.add_argument("--marker", required=True)
    scan_parser.add_argument("--target-list")
    scan_parser.add_argument("targets", nargs="*")
    boot_parser = sub.add_parser("boot")
    boot_parser.add_argument("--qemu", required=True)
    boot_parser.add_argument("--kernel", required=True)
    boot_parser.add_argument("--initrd", required=True)
    boot_parser.add_argument("--disk", required=True)
    boot_parser.add_argument("--transcript", required=True)
    boot_parser.add_argument("--mode", choices=["good", "invalid"], required=True)
    args = parser.parse_args()
    scan(args) if args.command == "scan" else boot(args)


if __name__ == "__main__":
    main()
