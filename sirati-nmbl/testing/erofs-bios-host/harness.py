#!/usr/bin/env python3
"""Boot the BIOS/GRUB EROFS host disk through its generation state machine.

Each boot starts QEMU with SeaBIOS on the whole disk (no -kernel), so GRUB,
the NMBL kernel/initrd on vfat /boot, the stage-1 persistent store and the
kexec into the selected EROFS generation are all real. The target system's
step service advances the state and powers off; the final boot must land in
the signed rescue with the signed network stage and serve recovery SSH.
"""

import argparse
import os
import selectors
import signal
import subprocess
import tempfile
import time
from pathlib import Path

GRUB = "Booting `NMBL Bootloader'"
CONFIG_LOAD = "loading full config from /mnt/boot/nmbl-generations/active/config.toml"


def wait_for(proc, patterns, timeout, transcript, forbidden=()):
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    seen = bytearray()
    while time.monotonic() < deadline:
        for key, _ in selector.select(timeout=1):
            chunk = os.read(key.fd, 65536)
            if chunk:
                transcript.write(chunk)
                transcript.flush()
                seen.extend(chunk)
                if os.environ.get("NMBL_VM_VERBOSE") == "1":
                    os.write(1, chunk)
        text = seen.decode(errors="replace")
        for bad in forbidden:
            if bad in text:
                raise RuntimeError(f"unexpected {bad!r}:\n{text[-16000:]}")
        if all(pattern in text for pattern in patterns):
            return text
        if proc.poll() is not None:
            raise RuntimeError(f"QEMU exited {proc.returncode} waiting for {patterns}:\n{text[-16000:]}")
    raise RuntimeError(f"timed out waiting for {patterns}:\n{seen[-16000:].decode(errors='replace')}")


def start(args, network_socket=None):
    command = [
        args.qemu, "-machine", "pc,accel=kvm:tcg", "-cpu", "max",
        "-m", "3072", "-smp", "2",
        "-drive", f"file={args.disk},format=raw,if=virtio",
        "-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot",
    ]
    if network_socket is None:
        command += ["-nic", "none"]
    else:
        command += [
            "-netdev", f"stream,id=net0,server=off,addr.type=unix,addr.path={network_socket}",
            "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
        ]
    return subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, start_new_session=True,
    )


def stop(proc):
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()


def target_step(args, transcript, marker):
    proc = start(args)
    try:
        wait_for(proc, [GRUB, CONFIG_LOAD, marker], 420, transcript,
                 forbidden=("NMBL_BIOS_STEP_FAILED", "external rescue: mounting"))
        proc.wait(timeout=60)
    finally:
        stop(proc)


def start_passt(args, network_dir):
    socket_path = network_dir / "qemu.sock"
    passt = subprocess.Popen(
        [
            args.passt, "-f", "-s", str(socket_path),
            "--runas", f"{os.getuid()}:{os.getgid()}",
            "-a", "10.0.2.15", "-n", "32", "-g", "10.0.2.2",
            "-M", "52:54:00:12:34:56", "-D", "10.0.2.3",
            "-t", f"127.0.0.1/{args.ssh_port}:22222",
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT,
    )
    deadline = time.monotonic() + 15
    while not socket_path.exists():
        if passt.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError("passt did not open its QEMU socket")
        time.sleep(0.1)
    return passt, socket_path


def remote_rescue(args, transcript, qemu):
    host_key = Path(args.ssh_host_key).read_text().split()
    known_hosts = Path(args.transcript).with_suffix(".known-hosts")
    known_hosts.write_text(f"[127.0.0.1]:{args.ssh_port} {host_key[0]} {host_key[1]}\n")
    command = [
        args.ssh, "-p", str(args.ssh_port), "-i", args.ssh_key,
        "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
        "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={known_hosts}",
        "-o", "ConnectTimeout=15", "root@127.0.0.1",
        "findmnt -n -o OPTIONS /nmbl-network | grep -qw ro && echo NMBL_BIOS_REMOTE_OK",
    ]
    deadline = time.monotonic() + 120
    last = ""
    while time.monotonic() < deadline:
        if qemu.poll() is not None:
            raise RuntimeError("rescue VM exited before SSH became reachable")
        result = subprocess.run(command, capture_output=True, text=True)
        transcript.write(f"\n[ssh rc={result.returncode}] {result.stdout}{result.stderr}\n".encode())
        if result.returncode == 0 and "NMBL_BIOS_REMOTE_OK" in result.stdout:
            return
        last = result.stderr
        time.sleep(1)
    raise RuntimeError(f"recovery SSH did not become ready: {last}")


def rescue_boot(args, transcript):
    network_dir = Path(tempfile.mkdtemp(prefix="nmbl-bios-passt-", dir="/tmp"))
    passt, socket_path = start_passt(args, network_dir)
    proc = start(args, socket_path)
    try:
        text = wait_for(proc, [GRUB, CONFIG_LOAD, "external rescue: mounting", "recovery system ready"],
                        420, transcript, forbidden=("network stage rejected",))
        if "NMBL_BIOS_" in text.split("external rescue: mounting", 1)[1]:
            raise RuntimeError("tested failure booted the target instead of rescue")
        checks = r'''
set -eux
findmnt -n -o OPTIONS /nmbl-network | grep -w ro | grep -w nosuid | grep -w noexec
grep -qx 'profile mac 52:54:00:12:34:56' /nmbl-network/etc/nmbl-network/network.conf
ip -4 addr show dev eth0 | grep -F '10.0.2.15/32'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'passwordauthentication no'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'authenticationmethods publickey'
sshd -T -f /etc/ssh/sshd_config | grep -Eqx 'permitrootlogin (without-password|prohibit-password)'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'allowusers root'
ss -tln | grep ':22222 '
echo NMBL_BIOS_RESCUE_"PASS"
'''
        proc.stdin.write(checks.encode())
        proc.stdin.flush()
        wait_for(proc, ["NMBL_BIOS_RESCUE_PASS"], 90, transcript)
        remote_rescue(args, transcript, proc)
    finally:
        stop(proc)
        passt.terminate()
        try:
            passt.wait(timeout=10)
        except subprocess.TimeoutExpired:
            passt.kill()
            passt.wait()
        socket_path.unlink(missing_ok=True)
        network_dir.rmdir()


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "passt", "ssh", "disk", "transcript", "ssh-key", "ssh-host-key"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--ssh-port", type=int, required=True)
    args = parser.parse_args()
    with Path(args.transcript).open("wb") as transcript:
        try:
            for marker in ("NMBL_BIOS_FIRST_BLESSED", "NMBL_BIOS_PENDING_FAILED",
                           "NMBL_BIOS_ROLLBACK_BLESSED", "NMBL_BIOS_TESTED_FAILED"):
                print(f"boot: expecting {marker}", flush=True)
                target_step(args, transcript, marker)
            print("boot: expecting signed rescue with network stage", flush=True)
            rescue_boot(args, transcript)
        except Exception:
            transcript.flush()
            print(f"transcript: {args.transcript}", flush=True)
            raise


if __name__ == "__main__":
    main()
