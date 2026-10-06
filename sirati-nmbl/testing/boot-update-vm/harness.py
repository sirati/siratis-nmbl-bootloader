import argparse
import os
import selectors
import subprocess
import sys
import time


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--qemu", required=True)
    parser.add_argument("--ovmf-code", required=True)
    parser.add_argument("--ovmf-vars", required=True)
    parser.add_argument("--esp", required=True)
    parser.add_argument("--boot", required=True)
    parser.add_argument("--root", required=True)
    parser.add_argument("--slot", choices=("A", "B"), required=True)
    parser.add_argument("--expect", choices=("boot", "refuse"), default="boot")
    parser.add_argument("--log", required=True)
    args = parser.parse_args()
    command = [
        args.qemu, "-machine", "q35,accel=tcg", "-cpu", "max", "-m", "1536",
        "-nodefaults", "-display", "none", "-serial", "stdio", "-no-reboot",
        "-drive", f"if=pflash,format=raw,readonly=on,file={args.ovmf_code}",
        "-drive", f"if=pflash,format=raw,file={args.ovmf_vars}",
        "-drive", f"if=virtio,format=raw,readonly=on,file={args.esp}",
        "-drive", f"if=virtio,format=raw,readonly=on,file={args.boot}",
        # NMBL mounts the root read-write; keep the shared image pristine.
        "-drive", f"if=virtio,format=raw,snapshot=on,file={args.root}",
    ]
    config = f"/mnt/boot/nmbl-boot-sets/{args.slot}/config"
    loading = f"phase 0.5: loading full config from {config}"
    # Every step after the slot config's own signature check, in order: the
    # config verifies, NMBL kexecs into the generation (whose signature the
    # enforcing config checked first), and the generation's initrd runs.
    generation = "<<< NixOS Stage 1 >>>"
    if args.expect == "boot":
        steps = [loading, f"phase 0.5: boot config {config} verified",
                 "kexec: handing off to new kernel", generation]
        forbidden = [f"boot config {config} rejected"]
    else:
        steps = [loading, f"phase 0.5: boot config {config} rejected"]
        forbidden = [f"boot config {config} verified", "phase 3b: mount system filesystems",
                     "phase 4: scan generations", "kexec: handing off to new kernel",
                     generation]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + 300
    output = bytearray()
    settled = None
    try:
        while time.monotonic() < deadline:
            for key, _ in selector.select(timeout=1):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if chunk:
                    output.extend(chunk)
            text = output.decode(errors="replace")
            after_load = text.split(loading, 1)[1] if loading in text else ""
            if any(bad in after_load for bad in forbidden):
                break
            position = 0
            for step in steps:
                found = text.find(step, position)
                if found < 0:
                    break
                position = found + len(step)
            else:
                if args.expect == "boot":
                    return 0
                # A refused config must stay refused: keep watching past the
                # selector timeout for any sign of the boot going on.
                settled = settled or time.monotonic() + 30
                if time.monotonic() >= settled:
                    return 0
            if process.poll() is not None:
                if settled is not None:
                    return 0
                break
        sys.stderr.write(output.decode(errors="replace"))
        return 1
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
        with open(args.log, "wb") as handle:
            handle.write(output)


if __name__ == "__main__":
    raise SystemExit(main())
