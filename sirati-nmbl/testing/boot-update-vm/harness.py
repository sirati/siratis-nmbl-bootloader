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
    parser.add_argument("--slot", choices=("A", "B"), required=True)
    parser.add_argument("--log", required=True)
    args = parser.parse_args()
    command = [
        args.qemu, "-machine", "q35,accel=tcg", "-cpu", "max", "-m", "1536",
        "-nodefaults", "-display", "none", "-serial", "stdio", "-no-reboot",
        "-drive", f"if=pflash,format=raw,readonly=on,file={args.ovmf_code}",
        "-drive", f"if=pflash,format=raw,file={args.ovmf_vars}",
        "-drive", f"if=virtio,format=raw,readonly=on,file={args.esp}",
        "-drive", f"if=virtio,format=raw,readonly=on,file={args.boot}",
    ]
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    expected = f"/mnt/boot/nmbl-boot-sets/{args.slot}/config".encode()
    deadline = time.monotonic() + 90
    output = bytearray()
    try:
        while time.monotonic() < deadline:
            for key, _ in selector.select(timeout=1):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    break
                output.extend(chunk)
                if expected in output:
                    return 0
            if process.poll() is not None:
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
