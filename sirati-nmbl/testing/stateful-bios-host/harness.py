#!/usr/bin/env python3
"""Drive the stateful BIOS/GRUB host through its failure cascade.

Every generation powers itself off before multi-user.target. NMBL must boot
the active generation 3, then roll back through the stateful ring (2, then
1), and when the retry budget is exhausted `--expect` states the outcome:
`rescue` (boot.nmbl.rescue.automatic = true) or `menu` (false).
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


def failing_boot(args, transcript, generation, marker_log):
    proc = start(args)
    try:
        wait_for(proc, [GRUB, marker_log, f"NMBL_STATEFUL_GEN{generation}_FAILING"],
                 420, transcript, forbidden=("_SUCCEEDED", "external rescue: mounting"))
        proc.wait(timeout=90)
    finally:
        stop(proc)


def rescue_boot(args, transcript):
    network_dir = Path(tempfile.mkdtemp(prefix="nmbl-stateful-passt-", dir="/tmp"))
    socket_path = network_dir / "qemu.sock"
    passt = subprocess.Popen(
        [args.passt, "-f", "-s", str(socket_path), "--runas", f"{os.getuid()}:{os.getgid()}",
         "-t", f"127.0.0.1/{args.ssh_port}:22222"],
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT,
    )
    deadline = time.monotonic() + 15
    while not socket_path.exists():
        if passt.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError("passt did not open its QEMU socket")
        time.sleep(0.1)
    proc = start(args, socket_path)
    try:
        text = wait_for(proc, [GRUB, "max recovery attempts exceeded",
                               "entering automatic rescue", "external rescue: mounting",
                               "recovery system ready"],
                        420, transcript, forbidden=("NMBL_STATEFUL_GEN",))
        if "Boot failed. The chain of errors" in text:
            raise RuntimeError("automatic rescue showed the emergency menu first")
        command = [
            args.ssh, "-p", str(args.ssh_port), "-i", args.ssh_key,
            "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
            "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=15", "root@127.0.0.1",
            "test -d /nmbl-root && echo NMBL_STATEFUL_REMOTE_OK",
        ]
        deadline = time.monotonic() + 180
        last = ""
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError("rescue VM exited before SSH became reachable")
            result = subprocess.run(command, capture_output=True, text=True)
            if result.returncode == 0 and "NMBL_STATEFUL_REMOTE_OK" in result.stdout:
                return
            last = result.stderr
            time.sleep(2)
        raise RuntimeError(f"recovery SSH did not become ready: {last}")
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


def menu_boot(args, transcript):
    proc = start(args)
    try:
        wait_for(proc, [GRUB, "max recovery attempts exceeded", "Boot failed. The chain of errors",
                        "[Reboot]"],
                 420, transcript,
                 forbidden=("NMBL_STATEFUL_GEN", "external rescue: mounting",
                            "entering automatic rescue"))
    finally:
        stop(proc)


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "passt", "ssh", "disk", "transcript", "ssh-key"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--ssh-port", type=int, required=True)
    parser.add_argument("--expect", choices=("rescue", "menu"), required=True)
    args = parser.parse_args()
    with Path(args.transcript).open("wb") as transcript:
        steps = [
            (3, "selector skipped"),
            (2, "stateful rollback forced generation 2"),
            (1, "stateful rollback forced generation 1"),
        ]
        for generation, marker in steps:
            print(f"boot [{args.expect}]: generation {generation} fails", flush=True)
            failing_boot(args, transcript, generation, "phase 4: scanning generations")
        print(f"boot [{args.expect}]: retries exhausted", flush=True)
        if args.expect == "rescue":
            rescue_boot(args, transcript)
        else:
            menu_boot(args, transcript)


if __name__ == "__main__":
    main()
