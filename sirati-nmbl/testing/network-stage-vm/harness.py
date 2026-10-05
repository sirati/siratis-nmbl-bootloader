#!/usr/bin/env python3
"""Drive NMBL's direct-kernel rescue VM and scan binary artifacts."""

import argparse
import contextlib
import json
import os
import selectors
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
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
    secrets = [Path(key).read_bytes() for key in args.key]
    needles = [secret for value in secrets for secret in (value, value[9:])]
    needles.append(args.marker.encode())
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


def console_image(args):
    root = Path(args.initrd)
    bootstraps = list(root.rglob("*-nmbl-bootstrap.toml"))
    if len(bootstraps) != 1:
        raise RuntimeError("complete initramfs must contain one bootstrap profile")
    profile = tomllib.loads(bootstraps[0].read_text())
    baseline = {"i8042", "atkbd", "usbhid", "hid_generic", "xhci_pci", "ehci_pci"}
    if not baseline <= set(profile["bootstrap"]["kernel_modules"]["explicit"]):
        raise RuntimeError("trusted bootstrap omits console input preload")
    builtin = set()
    for path in root.rglob("modules.builtin"):
        builtin.update(Path(line).name.removesuffix(".ko").replace("-", "_")
                       for line in path.read_text().splitlines())
    included = {path.name.split(".ko", 1)[0].replace("-", "_")
                for path in root.rglob("*.ko*") if path.is_file()}
    if not baseline <= included | builtin:
        raise RuntimeError("complete initramfs omits console input modules or builtin evidence")
    if args.identity_config:
        config = tomllib.loads(Path(args.identity_config).read_text())
        identity = config["rescue"]["identity_volume"]
        required = set(identity.get("required_modules", []))
        if identity.get("fstype") != "btrfs" or "crc32c-cryptoapi" not in required:
            raise RuntimeError("native Btrfs identity config omits its exact CRC32C crypto provider")
        if not {name.replace("-", "_") for name in required} <= included:
            raise RuntimeError("native identity required modules are absent from the actual initramfs")
        print("native Btrfs identity CRC32C config and actual module bytes verified", flush=True)
    print("complete initramfs console preload and module bytes verified", flush=True)


def wait_for(proc, patterns, timeout, transcript, extra_proc=None):
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ, True)
    if extra_proc is not None:
        selector.register(extra_proc.stdout, selectors.EVENT_READ, False)
    deadline = time.monotonic() + timeout
    seen = bytearray()
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"process exited {proc.returncode}:\n{seen[-12000:].decode(errors='replace')}")
        if extra_proc is not None and extra_proc.poll() is not None:
            raise RuntimeError(f"QEMU exited {extra_proc.returncode} during SSH")
        for key, _ in selector.select(timeout=1):
            chunk = os.read(key.fd, 65536)
            if not chunk:
                continue
            transcript.write(chunk)
            transcript.flush()
            if key.data:
                seen.extend(chunk)
            if os.environ.get("NMBL_VM_VERBOSE") == "1":
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
        text = seen.decode(errors="replace")
        if all(pattern in text for pattern in patterns):
            return text
    raise RuntimeError(f"timed out waiting for {patterns}:\n{seen[-12000:].decode(errors='replace')}")


def qemu_command(args, disk, network_socket, qmp_socket):
    command = [
        args.qemu,
        "-machine", "q35,accel=tcg",
        "-cpu", "max",
        "-m", "3072",
        "-smp", "2",
        "-kernel", args.kernel,
        "-initrd", args.initrd,
        "-append", "console=tty0 console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
        "-drive", f"file={disk},format=qcow2,if=virtio",
        "-netdev", f"stream,id=net0,server=off,addr.type=unix,addr.path={network_socket}",
        "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
        "-display", "none",
        "-serial", "stdio",
        "-monitor", "none",
        "-qmp", f"unix:{qmp_socket},server=on,wait=off",
        "-no-reboot",
    ]
    if args.identity_disk:
        command.extend(["-drive", f"file={args.identity_disk},format=qcow2,if=virtio"])
    return command


def console_proof(proc, qmp_socket, transcript):
    proc.stdin.write(b"test -f /run/nmbl-console-inherited-state-fixture && echo NMBL_CONSOLE_\"STIMULUS_PASS\"\n")
    proc.stdin.flush()
    wait_for(proc, ["NMBL_CONSOLE_STIMULUS_PASS"], 30, transcript)
    # An echoed command is never the receipt: quoted marker fragments only
    # concatenate in the shell after the physical terminal input was read.
    proc.stdin.write(b"test $(tty) = /dev/ttyS0 && test -r /dev/tty && "
                     b"test $(ps -o pgid= -p $$) = $(ps -o tpgid= -p $$) "
                     b"&& echo NMBL_SERIAL_\"CTTY_PASS\"\n")
    proc.stdin.flush()
    wait_for(proc, ["NMBL_SERIAL_CTTY_PASS"], 30, transcript)
    with socket.socket(socket.AF_UNIX) as qmp, contextlib.ExitStack() as resources:
        qmp.settimeout(10)
        qmp.connect(str(qmp_socket))
        stream = resources.enter_context(qmp.makefile("rwb"))
        json.loads(stream.readline())

        def command(name, arguments=None):
            stream.write((json.dumps({"execute": name, "arguments": arguments or {}}) + "\n").encode())
            stream.flush()
            while True:
                reply = json.loads(stream.readline())
                if "error" in reply:
                    raise RuntimeError(f"QMP console input failed: {reply['error']}")
                if "return" in reply:
                    return

        command("qmp_capabilities")
        # The production launcher must select its own VT. Never repair it
        # with Ctrl-Alt-F1 before checking physical keyboard input.
        for char in "tty >/run/vga-tty\n":
            key = {" ": "spc", "/": "slash", "-": "minus", "\n": "ret", ">": "dot"}.get(char, char)
            keys = ([{"type": "qcode", "data": "shift"}] if char == ">" else [])
            keys.append({"type": "qcode", "data": key})
            command("send-key", {"keys": keys, "hold-time": 25})
            time.sleep(0.04)
    # QMP returns after queuing key events, before the guest shell has
    # necessarily executed the command. Observe its actual side effect.
    proc.stdin.write(b"for n in $(seq 1 50); do if test -f /run/vga-tty && test $(cat /run/vga-tty) = /dev/tty1; then echo NMBL_VGA_\"INPUT_PASS\"; break; fi; sleep .1; done\n")
    proc.stdin.flush()
    wait_for(proc, ["NMBL_VGA_INPUT_PASS"], 30, transcript)
    proc.stdin.write(b"sleep 60\n")
    proc.stdin.flush()
    time.sleep(0.5)
    proc.stdin.write(b"\x03")
    proc.stdin.flush()
    time.sleep(0.3)
    proc.stdin.write(b"echo NMBL_JOB_\"CONTROL_PASS\"\n")
    proc.stdin.flush()
    wait_for(proc, ["NMBL_JOB_CONTROL_PASS"], 15, transcript)
    proc.stdin.write(b"exit\n")
    proc.stdin.flush()
    time.sleep(2)
    proc.stdin.write(b"test $(cat /proc/1/comm) = init && echo NMBL_RESCUE_\"RESPAWN_PASS\"\n")
    proc.stdin.flush()
    wait_for(proc, ["NMBL_RESCUE_RESPAWN_PASS"], 30, transcript)


def remote_tui(args, transcript, qemu):
    host = "::1" if args.mode == "baked-slaac" else "127.0.0.1"
    host_key = Path(args.ssh_host_key).read_text().split()
    known_hosts = Path(args.transcript).with_suffix(".known-hosts")
    known_hosts.write_text(
        f"[{host}]:{args.ssh_port} {host_key[0]} {host_key[1]}\n"
    )
    command = [
        args.ssh,
        "-tt",
        "-p", str(args.ssh_port),
        "-i", args.ssh_key,
        "-o", "BatchMode=yes",
        "-o", "IdentitiesOnly=yes",
        "-o", "StrictHostKeyChecking=yes",
        "-o", f"UserKnownHostsFile={known_hosts}",
        "-o", "ConnectTimeout=15",
        f"root@{host}",
        "stty rows 30 cols 100; exec nmbl",
    ]
    environment = os.environ.copy()
    environment["TERM"] = "xterm-256color"
    deadline = time.monotonic() + 120
    last_error = None
    while time.monotonic() < deadline:
        proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=environment,
        )
        try:
            wait_for(
                proc,
                ["boot failed", "Reboot", "Raw Shell", "Retry boot from config"],
                60,
                transcript,
                extra_proc=qemu,
            )
            proc.stdin.write(b"\x05")
            proc.stdin.flush()
            if proc.wait(timeout=30) != 0:
                raise RuntimeError("remote nmbl TUI SSH session failed")
            return
        except RuntimeError as error:
            last_error = error
            time.sleep(1)
        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait(timeout=10)
    raise RuntimeError(f"rescue SSH/TUI did not become ready: {last_error}")


def hold_failed_vm(proc, transcript):
    """Keep the failed runtime and drain serial until its operator ends it."""
    with selectors.DefaultSelector() as selector:
        selector.register(proc.stdout, selectors.EVENT_READ)
        while proc.poll() is None:
            for key, _ in selector.select(timeout=1):
                chunk = os.read(key.fd, 65536)
                if chunk:
                    transcript.write(chunk)
                    transcript.flush()


def boot(args):
    print(f"booting {args.mode} VM from {args.disk}", flush=True)
    transcript_path = Path(args.transcript)
    with transcript_path.open("wb") as transcript:
        network_dir = Path(tempfile.mkdtemp(prefix="nmbl-passt-", dir="/tmp"))
        network_socket = network_dir / "qemu.sock"
        qmp_socket = network_dir / "qmp.sock"
        overlay = transcript_path.with_suffix(".qcow2")
        subprocess.run([str(Path(args.qemu).with_name("qemu-img")), "create", "-f", "qcow2", "-F", "raw",
                        "-b", str(Path(args.disk).resolve()), str(overlay)], check=True)
        passt_log = transcript_path.with_suffix(".passt.log")
        passt_output = passt_log.open("wb")
        passt_command = [
                args.passt,
                "-f", "-s", str(network_socket),
                "--runas", f"{os.getuid()}:{os.getgid()}",
                "-a", "88.99.80.66" if args.mode in ("baked-static", "native-identity", "missing-identity") else "10.0.2.15", "-n", "32",
                "-g", "172.31.1.1" if args.mode in ("baked-static", "native-identity", "missing-identity") else "10.0.2.2", "-M", "52:54:00:12:34:56",
                "-D", "10.0.2.3",
                "-t", f"127.0.0.1/{args.ssh_port}:22222",
            ]
        if args.mode == "baked-slaac":
            passt_command = [args.passt, "-f", "-s", str(network_socket),
                             "--runas", f"{os.getuid()}:{os.getgid()}", "-6",
                             "-a", "2001:db8::15", "-g", "fe80::2", "-D", "2001:db8::3",
                             "-M", "52:54:00:12:34:56", "--no-dhcp", "--no-dhcpv6",
                             "-t", f"::1/{args.ssh_port}:22222"]
        passt = subprocess.Popen(
            passt_command,
            stdout=passt_output,
            stderr=subprocess.STDOUT,
        )
        passt_output.close()
        deadline = time.monotonic() + 15
        while not network_socket.exists():
            if passt.poll() is not None:
                details = passt_log.read_text(errors="replace") if passt_log.exists() else ""
                raise RuntimeError(
                    f"passt exited {passt.returncode} before opening its socket: {details}"
                )
            if time.monotonic() >= deadline:
                raise RuntimeError("passt did not open its QEMU socket")
            time.sleep(0.1)
        proc = subprocess.Popen(
            qemu_command(args, overlay, network_socket, qmp_socket),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            if args.mode in ("good", "baked-static", "baked-slaac", "native-identity", "missing-identity"):
                wait_for(proc, ["recovery system ready"], 240, transcript)
                console_proof(proc, qmp_socket, transcript)
                commands = r'''
set -eux
findmnt -n -o OPTIONS /nmbl-network | grep -w ro | grep -w nodev | grep -w nosuid | grep -w noexec
test -d /nmbl-network/lib/modules/$(uname -r)
test -d /nmbl-network/lib/firmware
grep -qx 'version 2' /nmbl-network/etc/nmbl-network/network.conf
grep -qx 'profile mac 52:54:00:12:34:56' /nmbl-network/etc/nmbl-network/network.conf
grep -qx 'dns 10.0.2.3' /nmbl-network/etc/nmbl-network/network.conf
grep -qx 'dns fec0::3' /nmbl-network/etc/nmbl-network/network.conf
lsmod | grep '^dummy '
test "$(sha256sum /etc/ssh/ssh_host_ed25519_key | cut -d' ' -f1)" = "$(sha256sum /nmbl-root/mnt/boot/rescue-host-ed25519 | cut -d' ' -f1)"
test "$(stat -c '%u:%g:%a' /nmbl-root/mnt/boot/rescue-host-ed25519)" = 0:0:600
sshd -T -f /etc/ssh/sshd_config | grep -qx 'passwordauthentication no'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'kbdinteractiveauthentication no'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'authenticationmethods publickey'
sshd -T -f /etc/ssh/sshd_config | grep -qx 'disableforwarding yes'
ss -tln | grep ':22222 '
ip -4 addr show dev eth0 | grep -F '10.0.2.15/32'
ip -6 addr show dev eth0 | grep -F 'fec0::15/64'
ip -4 route show | grep -F 'default via 10.0.2.2 dev eth0 onlink'
ip -6 route show | grep -E 'default via fe80::2 dev eth0 .*onlink'
ip -4 route show | grep -F '198.51.100.0/24 via 10.0.2.2 dev eth0 onlink'
ip -6 route show | grep -E '2001:db8:1::/64 via fe80::2 dev eth0 .*onlink'
grep -qx 'nameserver 10.0.2.3' /etc/resolv.conf
grep -qx 'nameserver fec0::3' /etc/resolv.conf
! pgrep -x dhcpcd
echo NMBL_NETWORK_STAGE_VM_"PASS"
'''
                if args.mode in ("baked-static", "native-identity", "missing-identity"):
                    commands = r'''
test ! -d /nmbl-network/etc/nmbl-network &&
grep -qx 'version 2' /nix/store/*-nmbl-baked-network.conf &&
ip -4 addr show dev eth0 | grep -F '88.99.80.66/32' &&
ip -4 route show | grep -F 'default via 172.31.1.1 dev eth0 onlink' &&
! pgrep -x dhcpcd &&
ss -tln | grep ':22222 ' &&
echo NMBL_NETWORK_STAGE_VM_"PASS"
'''
                if args.mode == "missing-identity":
                    commands = commands.replace("ss -tln | grep ':22222 ' &&", "! pgrep -x sshd &&")
                if args.mode == "baked-slaac":
                    commands = r'''
test ! -d /nmbl-network/etc/nmbl-network &&
grep -qx 'version 1' /nix/store/*-nmbl-baked-network.conf &&
grep -qx 'address-family ipv6-only' /nix/store/*-nmbl-baked-network.conf &&
test -z "$(ip -o -4 addr show scope global)" &&
ip -6 addr show dev eth0 scope global | grep '2001:db8::5054:ff:fe12:3456/64' &&
ip -6 route show default | grep 'default via ' &&
ss -tln | grep ':22222 ' &&
echo NMBL_NETWORK_STAGE_VM_"PASS"
'''
                if args.mode == "native-identity":
                    commands = commands.replace('echo NMBL_NETWORK_STAGE_VM_', 'findmnt -n -o FSTYPE /nmbl-root/nmbl-identity | grep -qx btrfs && findmnt -n -o OPTIONS /nmbl-root/nmbl-identity | grep -w ro | grep -w nodev | grep -w nosuid | grep -w noexec && test ! -e /dev/intentional-missing-generation-root && echo NMBL_NETWORK_STAGE_VM_')
                proc.stdin.write(commands.encode())
                proc.stdin.flush()
                text = wait_for(proc, ["NMBL_NETWORK_STAGE_VM_PASS"], 90, transcript)
                if "network-stage signature" in text.lower() and "failed" in text.lower():
                    raise RuntimeError("positive VM reported a network-stage signature failure")
                if args.mode == "missing-identity":
                    proc.stdin.write(b"test ! -e /etc/ssh/ssh_host_ed25519_key && ! pgrep -x sshd && echo NMBL_IDENTITY_\"FAILCLOSED_PASS\"\n")
                    proc.stdin.flush()
                    wait_for(proc, ["NMBL_IDENTITY_FAILCLOSED_PASS"], 30, transcript)
                    denied = subprocess.run([args.ssh, "-p", str(args.ssh_port), "-o", "BatchMode=yes", "-o", "ConnectTimeout=3", "root@127.0.0.1", "true"], capture_output=True, timeout=10)
                    if denied.returncode == 0: raise RuntimeError("missing persistent identity exposed SSH")
                else:
                    remote_tui(args, transcript, proc)
            else:
                text = wait_for(
                    proc,
                    ["network stage rejected", "local console only"],
                    180,
                    transcript,
                )
                if "recovery system ready" in text:
                    raise RuntimeError("invalid network stage reached the rescue system")
                console_proof(proc, qmp_socket, transcript)
        except BaseException:
            # Preserve original disk plus RAM/CPU/device state before any
            # operator diagnosis. This is an ordinary test failure snapshot,
            # never an in-guest repair or a successful test receipt.
            transcript_path.parent.joinpath(".nmbl-preserve-failure").write_text(str(overlay) + "\n")
            snapshot_preserved = False
            try:
                with socket.socket(socket.AF_UNIX) as qmp, contextlib.ExitStack() as resources:
                    qmp.settimeout(120)
                    qmp.connect(str(qmp_socket))
                    stream = resources.enter_context(qmp.makefile("rwb"))
                    json.loads(stream.readline())
                    for request in ({"execute": "qmp_capabilities"}, {"execute": "stop"},
                                    {"execute": "human-monitor-command", "arguments": {"command-line": "savevm rescue-failure"}}):
                        stream.write((json.dumps(request) + "\n").encode())
                        stream.flush()
                        while True:
                            reply = json.loads(stream.readline())
                            if "error" in reply:
                                raise RuntimeError(f"failed to preserve rescue snapshot: {reply['error']}")
                            if "return" in reply:
                                if isinstance(reply["return"], str) and reply["return"].strip():
                                    raise RuntimeError(f"snapshot manager reported: {reply['return']}")
                                break
                snapshot_preserved = True
                print(f"failure snapshot preserved in {overlay} (rescue-failure)", flush=True)
            except Exception as snapshot_error:
                print(f"SNAPSHOT FAILURE: {snapshot_error}; retaining live QEMU and passt; no manual diagnosis", file=sys.stderr, flush=True)
                transcript_path.parent.joinpath(".snapshot-failed-live").write_text(f"qemu={proc.pid}\npasst={passt.pid}\nqmp={qmp_socket}\n")
            # A failure snapshot is a diagnosis checkpoint, not permission
            # to terminate the original VM. The operator owns its next action.
            receipt = {
                "qemu_pid": proc.pid, "passt_pid": passt.pid,
                "qmp_socket": str(qmp_socket),
                "serial_stdin": f"/proc/{proc.pid}/fd/0",
                "qemu_argv": qemu_command(args, overlay, network_socket, qmp_socket),
                "passt_argv": passt_command,
                "overlay": str(overlay), "identity_overlay": args.identity_disk,
                "snapshot": "rescue-failure" if snapshot_preserved else None,
            }
            receipt_path = transcript_path.with_suffix(".failure-runtime.json")
            receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
            receipt_path.chmod(0o600)
            print(f"FAILED VM RETAINED for manual diagnosis: {receipt_path}", file=sys.stderr, flush=True)
            # Continue draining serial output so manual shell commands cannot
            # block on a full pipe. Explicitly ending this VM ends ownership.
            hold_failed_vm(proc, transcript)
            raise
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
            passt.terminate()
            try:
                passt.wait(timeout=10)
            except subprocess.TimeoutExpired:
                passt.kill()
                passt.wait()
            network_socket.unlink(missing_ok=True)
            qmp_socket.unlink(missing_ok=True)
            network_dir.rmdir()


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    scan_parser = sub.add_parser("scan")
    scan_parser.add_argument("--key", action="append", required=True)
    scan_parser.add_argument("--marker", required=True)
    scan_parser.add_argument("--target-list")
    scan_parser.add_argument("targets", nargs="*")
    image_parser = sub.add_parser("console-image")
    image_parser.add_argument("--initrd", required=True)
    image_parser.add_argument("--identity-config")
    boot_parser = sub.add_parser("boot")
    boot_parser.add_argument("--qemu", required=True)
    boot_parser.add_argument("--passt", required=True)
    boot_parser.add_argument("--kernel", required=True)
    boot_parser.add_argument("--initrd", required=True)
    boot_parser.add_argument("--disk", required=True)
    boot_parser.add_argument("--transcript", required=True)
    boot_parser.add_argument("--ssh", required=True)
    boot_parser.add_argument("--ssh-port", type=int, required=True)
    boot_parser.add_argument("--ssh-key", required=True)
    boot_parser.add_argument("--ssh-host-key", required=True)
    boot_parser.add_argument("--identity-disk")
    boot_parser.add_argument("--mode", choices=["good", "invalid", "baked-static", "baked-slaac", "native-identity", "missing-identity"], required=True)
    args = parser.parse_args()
    if args.command == "scan":
        scan(args)
    elif args.command == "console-image":
        console_image(args)
    else:
        boot(args)


if __name__ == "__main__":
    main()
