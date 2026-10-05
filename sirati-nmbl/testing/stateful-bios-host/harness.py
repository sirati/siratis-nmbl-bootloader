#!/usr/bin/env python3
"""Drive the stateful BIOS/GRUB host through its failure cascade.

Every generation powers itself off before multi-user.target. NMBL must boot
the active generation 3, then roll back through the stateful ring (2, then
1), and when the retry budget is exhausted `--expect` states the outcome:
`rescue` (boot.nmbl.rescue.automatic = true) or `menu` (false).
"""

import argparse
import json
import socket
import sys
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


def decode_state(data):
    """Decode the bounded CBOR primitives used by the production state slot."""
    if len(data) != 16384:
        raise RuntimeError("invalid state slot size")
    position = 0
    def item(depth=0):
        nonlocal position
        if depth > 32 or position >= len(data):
            raise RuntimeError("invalid state CBOR")
        head = data[position]; position += 1
        major, short = head >> 5, head & 31
        if major == 7:
            if short in (20, 21, 22): return {20: False, 21: True, 22: None}[short]
            raise RuntimeError("unsupported state primitive")
        if short < 24: value = short
        elif short in (24, 25, 26, 27):
            length = 1 << (short - 24)
            if position + length > len(data): raise RuntimeError("truncated state CBOR")
            value = int.from_bytes(data[position:position+length], "big"); position += length
        else: raise RuntimeError("unsupported state CBOR length")
        if major == 0: return value
        if major == 1: return -1-value
        if value > 16384: raise RuntimeError("oversized state CBOR")
        if major in (2, 3):
            end = position + value
            if end > len(data): raise RuntimeError("truncated state CBOR string")
            result = data[position:end]; position = end
            return result.decode() if major == 3 else result
        if major == 4: return [item(depth+1) for _ in range(value)]
        if major == 5:
            result = {}
            for _ in range(value):
                key = item(depth+1); result[key] = item(depth+1)
            return result
        raise RuntimeError("unsupported state CBOR type")
    result = item()
    if not isinstance(result, dict): raise RuntimeError("state slot is not a map")
    return result


def check_rescue_state(state, baseline=None, ready=True):
    if state.get("last_attempted_generation") != 1 or state.get("last_boot_succeeded") is not False:
        raise RuntimeError("rescue retry changed generation or manufactured health")
    if state.get("rescue_booted_generation") != (1 if ready else None):
        raise RuntimeError("rescue one-use authorization was not recorded/consumed")
    if state.get("rescue_exit_retry_in_progress") is not (not ready):
        raise RuntimeError("rescue retry progress state is wrong")
    if baseline is not None:
        for field in ("recovery_attempt", "known_good_generations"):
            if state.get(field) != baseline.get(field):
                raise RuntimeError(f"rescue retry changed {field}")
    return state


def offline_state(args):
    directory = Path(args.disk).parent
    esp = directory / "state-check-esp.raw"
    state = directory / "state-check.bin"
    subprocess.run([str(Path(args.qemu).with_name("qemu-img")), "dd", "--image-opts",
                    f"if=driver=raw,offset=2097152,size=268435456,file.driver=qcow2,file.file.driver=file,file.file.filename={args.disk.replace(chr(44), chr(44)*2)}",
                    f"of={esp}", "bs=512", "count=524288"], check=True,
                   stdout=subprocess.DEVNULL)
    subprocess.run(["mcopy", "-o", "-i", str(esp), "::/nmbl/state.bin", str(state)], check=True)
    result = decode_state(state.read_bytes())
    esp.unlink(); state.unlink()
    return result


def snapshot_failure(proc):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(30); sock.connect(proc.nmbl_qmp)
        with sock.makefile("rwb") as stream:
            json.loads(stream.readline())
            def command(name, arguments=None):
                stream.write((json.dumps({"execute":name,"arguments":arguments or {}})+"\n").encode()); stream.flush()
                while True:
                    response=json.loads(stream.readline())
                    if "error" in response: raise RuntimeError(response["error"])
                    if "return" in response: return response["return"]
            command("qmp_capabilities"); command("stop")
            result=command("human-monitor-command", {"command-line":"savevm rescue-failure"})
            if "error" in result.lower() or "failed" in result.lower(): raise RuntimeError(result)
    Path(proc.nmbl_disk).parent.joinpath(".nmbl-preserve-failure").write_text(proc.nmbl_disk+"\n")
    print("STATEFUL_FAILURE_SNAPSHOT_CONFIRMED", proc.nmbl_disk, flush=True)


def start(args, network_socket=None):
    command = [
        args.qemu, "-machine", "pc,accel=kvm:tcg", "-cpu", "max",
        "-m", "3072", "-smp", "2",
        "-drive", f"file={args.disk},format=qcow2,if=virtio",
        "-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot",
    ]
    if network_socket is None:
        command += ["-nic", "none"]
    else:
        command += [
            "-netdev", f"stream,id=net0,server=off,addr.type=unix,addr.path={network_socket}",
            "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
        ]
    socket_dir = Path(tempfile.mkdtemp(prefix="nmbl-stateful-qmp-", dir="/tmp"))
    qmp = socket_dir / "qmp.sock"
    command += ["-qmp", f"unix:{qmp},server=on,wait=off"]
    proc = subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, start_new_session=True,
    )
    proc.nmbl_qmp = str(qmp)
    proc.nmbl_disk = args.disk
    return proc


def stop(proc):
    if proc.poll() is None and sys.exc_info()[0] is not None:
        snapshot_preserved = False
        try:
            snapshot_failure(proc)
            snapshot_preserved = True
        except Exception as error:
            Path(proc.nmbl_disk).parent.joinpath(".snapshot-failed-live").write_text(f"{proc.pid} {proc.nmbl_qmp}\n")
            print(f"snapshot failed; live VM retained: {error}", file=sys.stderr, flush=True)
        # Snapshot success also retains the original runtime for diagnosis.
        Path(proc.nmbl_disk).parent.joinpath("failure-runtime.json").write_text(json.dumps({
            "qemu_pid":proc.pid,"qmp_socket":proc.nmbl_qmp,
            "serial_stdin":f"/proc/{proc.pid}/fd/0","overlay":proc.nmbl_disk,
            "snapshot":"rescue-failure" if snapshot_preserved else None}, indent=2)+"\n")
        with Path(proc.nmbl_disk).parent.joinpath("failure-serial-tail.log").open("ab") as tail, selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            while proc.poll() is None:
                for key, _ in selector.select(timeout=1):
                    chunk = os.read(key.fd,65536)
                    if chunk: tail.write(chunk); tail.flush()
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
    Path(proc.nmbl_qmp).unlink(missing_ok=True)
    Path(proc.nmbl_qmp).parent.rmdir()


def failing_boot(args, transcript, generation, marker_log):
    proc = start(args)
    try:
        wait_for(proc, [GRUB, marker_log, f"NMBL_STATEFUL_GEN{generation}_FAILING"],
                 420, transcript, forbidden=("_SUCCEEDED", "external rescue: mounting"))
        proc.wait(timeout=90)
    finally:
        stop(proc)


def rescue_boot(args, transcript, baseline=None):
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
            "cat /nmbl-root/mnt/boot-state/nmbl/state.bin",
        ]
        deadline = time.monotonic() + 180
        last = ""
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError("rescue VM exited before SSH became reachable")
            result = subprocess.run(command, capture_output=True)
            if result.returncode == 0:
                state = decode_state(result.stdout)
                if state.get("rescue_booted_generation") == 1:
                    state = check_rescue_state(state, baseline)
                    print("STATEFUL_RESCUE_READY_STATE", json.dumps(state, sort_keys=True), flush=True)
                    return state
            last = result.stderr.decode(errors="replace")
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
    original = args.disk
    args.disk = str(Path(original).with_suffix(".qcow2"))
    subprocess.run([str(Path(args.qemu).with_name("qemu-img")), "create", "-f", "qcow2", "-F", "raw", "-b", original, args.disk], check=True)
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
            baseline = rescue_boot(args, transcript)
            print("powercycle: exactly one failed-generation retry", flush=True)
            failing_boot(args, transcript, 1, "phase 4: scanning generations")
            consumed = check_rescue_state(offline_state(args), baseline, ready=False)
            print("STATEFUL_RETRY_CONSUMED_STATE", json.dumps(consumed, sort_keys=True), flush=True)
            rescue_boot(args, transcript, baseline)
            print("STATEFUL_RESCUE_POWERCYCLE_PASS", flush=True)
        else:
            menu_boot(args, transcript)


if __name__ == "__main__":
    main()
