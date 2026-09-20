#!/usr/bin/env python3
"""Boot the real NMBL kernel and exercise persistent EROFS state."""

import argparse
import os
import selectors
import signal
import subprocess
import time
from pathlib import Path


def wait_for(proc, patterns, timeout, transcript):
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
        if all(pattern in text for pattern in patterns):
            return text
        if proc.poll() is not None:
            raise RuntimeError(f"QEMU exited {proc.returncode}:\n{text[-16000:]}")
    raise RuntimeError(f"timed out waiting for {patterns}:\n{seen[-16000:].decode(errors='replace')}")


def start(args, transcript):
    command_line = [
        args.qemu, "-machine", "q35,accel=tcg", "-cpu", "max",
        "-m", "3072", "-smp", "2", "-kernel", args.kernel,
        "-initrd", args.initrd,
        "-append", "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
        "-drive", f"file={args.boot},format=raw,if=virtio",
        "-drive", f"file={args.root},format=raw,if=virtio",
        "-drive", f"file={args.store},format=raw,if=virtio",
        "-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot",
    ]
    return subprocess.Popen(
        command_line, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
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


def normal_boot(args, marker, transcript):
    proc = start(args, transcript)
    wait_for(proc, ["NMBL_TARGET_READY", marker], 300, transcript)
    proc.wait(timeout=30)
    return proc


def happy_path(args):
    with Path(args.transcript).open("wb") as transcript:
        proc = normal_boot(args, "NMBL_FIRST_BLESSED", transcript)
        try:
            proc = normal_boot(args, "NMBL_SECOND_BLESSED", transcript)
            proc = normal_boot(args, "NMBL_PENDING_FAILED", transcript)
            proc = normal_boot(args, "NMBL_ROLLBACK_BLESSED", transcript)
            proc = normal_boot(args, "NMBL_TESTED_FAILED", transcript)

            proc = start(args, transcript)
            text = wait_for(proc, ["[nmbl] external rescue: mounting"], 240, transcript)
            if "NMBL_TARGET_READY" in text:
                raise RuntimeError("tested failure booted the target instead of rescue")
        finally:
            stop(proc)


def invalid_path(args, label):
    args.store = getattr(args, label)
    expected = "signature" if label == "tampered" else "incomplete bundle"
    with Path(f"{args.transcript}-{label}").open("wb") as transcript:
        proc = start(args, transcript)
        try:
            text = wait_for(proc, [expected], 240, transcript)
            if "NMBL_TARGET_READY" in text:
                raise RuntimeError(f"{label} generation reached the target system")
        finally:
            stop(proc)


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "kernel", "initrd", "boot", "store", "tampered", "unsigned", "root",
                 "first", "second", "third", "transcript"):
        parser.add_argument(f"--{name}", required=True)
    args = parser.parse_args()
    happy_path(args)
    invalid_path(args, "tampered")
    invalid_path(args, "unsigned")


if __name__ == "__main__":
    main()
