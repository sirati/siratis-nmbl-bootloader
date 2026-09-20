#!/usr/bin/env python3
import os
import stat
import sys


def files_under(path):
    try:
        mode = os.lstat(path).st_mode
    except FileNotFoundError:
        return
    if stat.S_ISREG(mode):
        yield path
    elif stat.S_ISDIR(mode):
        for root, dirs, files in os.walk(path, followlinks=False):
            dirs[:] = [d for d in dirs if not os.path.islink(os.path.join(root, d))]
            for name in files:
                candidate = os.path.join(root, name)
                if os.path.isfile(candidate) and not os.path.islink(candidate):
                    yield candidate


def contains(path, needles):
    overlap = max(map(len, needles)) - 1
    tail = b""
    try:
        with open(path, "rb") as stream:
            while chunk := stream.read(4 * 1024 * 1024):
                data = tail + chunk
                if any(needle in data for needle in needles):
                    return True
                tail = data[-overlap:] if overlap else b""
    except (OSError, PermissionError):
        pass
    return False


key_path, marker_path, *roots = sys.argv[1:]
needles = [open(key_path, "rb").read(), open(marker_path, "rb").read()]
if not all(needles):
    raise SystemExit("empty private-key scan needle")

os.unlink(key_path)
os.unlink(marker_path)
leaks = [path for root in roots for path in files_under(root) if contains(path, needles)]
if leaks:
    print("private key bytes or marker leaked:", *leaks, sep="\n", file=sys.stderr)
    raise SystemExit(1)
print(f"private-key absence scan passed across {len(roots)} roots")
