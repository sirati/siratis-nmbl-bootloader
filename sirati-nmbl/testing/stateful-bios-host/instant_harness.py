#!/usr/bin/env python3
"""Instant-boot VM test on the stateful BIOS/GRUB host.

boot.nmbl.instantBoot is on and the normal selector timeout is 30 s, so an
immediate boot and a menu are easy to tell apart.

1. No input: NMBL must never show the menu, and generation 3 must reach its
   success target (the first boot also proves the fresh state.bin counts as
   healthy).
2. After that successful boot, a key is typed on the serial console from the
   GRUB hand-off onward (the early boot window, before the selector exists):
   NMBL must show the menu and stay there past the 30 s timeout.
"""

import argparse
import os
import selectors
import signal
import subprocess
import time
from pathlib import Path

GRUB = "Booting `NMBL Bootloader'"
# The selector's key hint: present exactly when NMBL shows the generation
# menu. NMBL's own log lines are silenced on the console while its TUI owns it,
# so the test asserts on behaviour (menu shown or not, generation booted or
# not, and how fast) rather than on log text.
MENU = "Enter boot"
SUCCEEDED = "NMBL_STATEFUL_GEN3_SUCCEEDED"
# The normal selector timeout is 30 s; an instant boot must be much faster.
NORMAL_TIMEOUT_S = 30


def start(args):
    command = [
        args.qemu, "-machine", "pc,accel=kvm:tcg", "-cpu", "max",
        "-m", "2048", "-smp", "2",
        "-drive", f"file={args.disk},format=raw,if=virtio",
        "-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot",
        "-nic", "none",
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


def drive(proc, transcript, done, timeout, forbidden=(), type_after=None):
    """Read serial output until `done(text)`; optionally type a key every
    200 ms once `type_after` has been seen, until `done` holds."""
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    seen = bytearray()
    typing = False
    last_key = 0.0
    while time.monotonic() < deadline:
        for key, _ in selector.select(timeout=0.1):
            chunk = os.read(key.fd, 65536)
            if chunk:
                transcript.write(chunk)
                transcript.flush()
                seen.extend(chunk)
        text = seen.decode(errors="replace")
        for bad in forbidden:
            if bad in text:
                raise RuntimeError(f"unexpected {bad!r}:\n{text[-12000:]}")
        if done(text):
            return text
        if type_after is not None and type_after in text:
            typing = True
        if typing and time.monotonic() - last_key > 0.2:
            try:
                proc.stdin.write(b"x")
                proc.stdin.flush()
            except BrokenPipeError:
                pass
            last_key = time.monotonic()
        if proc.poll() is not None:
            raise RuntimeError(f"QEMU exited {proc.returncode}:\n{text[-12000:]}")
    raise RuntimeError(f"timed out:\n{seen[-12000:].decode(errors='replace')}")


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "disk", "transcript"):
        parser.add_argument(f"--{name}", required=True)
    args = parser.parse_args()
    with Path(args.transcript).open("wb") as transcript:
        print("instant boot: untouched boot", flush=True)
        proc = start(args)
        try:
            drive(proc, transcript, lambda t: GRUB in t, 180)
            t0 = time.monotonic()
            drive(proc, transcript, lambda t: SUCCEEDED in t, 420,
                  forbidden=(MENU, "_FAILING"))
            elapsed = time.monotonic() - t0
            print(f"instant boot: generation 3 reached success {elapsed:.1f}s after GRUB",
                  flush=True)
            proc.wait(timeout=90)
        finally:
            stop(proc)

        print("instant boot: keypress during early boot", flush=True)
        proc = start(args)
        try:
            drive(proc, transcript, lambda t: MENU in t, 420,
                  forbidden=(SUCCEEDED,), type_after=GRUB)
            # The menu has no countdown once a key was pressed; NMBL must stay
            # there. Watch for well over the 30 s timeout without typing.
            try:
                drive(proc, transcript, lambda t: SUCCEEDED in t,
                      NORMAL_TIMEOUT_S + 15)
                raise RuntimeError("generation booted despite the early keypress")
            except RuntimeError as err:
                if "timed out" not in str(err):
                    raise
        finally:
            stop(proc)
    print("instant boot VM test passed", flush=True)


if __name__ == "__main__":
    main()
