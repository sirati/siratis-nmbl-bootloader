set -euo pipefail
# Runs the unmodified production nmbl-init as PID 1 in a rootless,
# capability-free podman container with its kernel-facing syscalls simulated
# (nmbl-simbox), and asserts the simulated kexec handover for two scenarios.
work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-simbox-test.XXXXXXXX")
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT

run() {
  local name=$1 scenario=$2
  echo "simbox: scenario $name"
  if ! @simbox@ run "$scenario" --headless --timeout 120 --json "$work/$name.json" \
      > "$work/$name.log" 2>&1; then
    sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$work/$name.log" | grep -av '^\s*$' >&2
    echo "simbox: scenario $name FAILED" >&2
    exit 1
  fi
}

run normal @normal@/scenario.toml
run luks @luks@/scenario.toml

# Graphical: the splash build draws on the simulated DRM card (shown in an
# X11 window on a headless Xvfb); Down+Enter typed INTO THAT WINDOW must
# reach NMBL as keyboard input and select generation 2.
echo "simbox: scenario splash (Xvfb, keys via the framebuffer window)"
display=:$((RANDOM % 200 + 300))
Xvfb "$display" -screen 0 1024x768x24 > "$work/xvfb.log" 2>&1 &
xvfb=$!
until xdpyinfo -display "$display" > /dev/null 2>&1; do :; done
DISPLAY=$display @simbox@ run @splash@/scenario.toml --graphical --timeout 120 \
  --frame-dump "$work/splash.ppm" --json "$work/splash.json" \
  < /dev/null > "$work/splash.log" 2>&1 &
sim=$!
until DISPLAY=$display xdotool search --name 'framebuffer \[frame ([3-9]|[1-9][0-9])' > /dev/null 2>&1 \
  || ! kill -0 "$sim" 2> /dev/null; do :; done
win=$(DISPLAY=$display xdotool search --name 'nmbl-simbox framebuffer' | head -n1 || true)
[[ -n "$win" ]] || { cat "$work/splash.log" >&2; echo "simbox: no framebuffer window" >&2; exit 1; }
DISPLAY=$display xdotool key --delay 200 --window "$win" Down Return
wait "$sim" || { cat "$work/splash.log" >&2; exit 1; }
kill "$xvfb"

python3 - "$work" <<'PY'
import json, sys
work = sys.argv[1]
def load(n): return json.load(open(f"{work}/{n}.json"))
init = "/nix/var/nix/profiles/system-3-link/init"
for name in ("normal", "luks"):
    r = load(name)
    assert r["outcome"] == "kexec", (name, r)
    assert r["init"] == init, (name, r["init"])
    assert r["cmdline"] == f"console=ttyS0 loglevel=4 simbox.gen=3 init={init}", (name, r["cmdline"])
    assert r["kernel_path"].endswith("nixos-system-simbox-gen3/kernel"), r["kernel_path"]
    assert r["kernel_len"] == len("SIMBOX-KERNEL-GEN3\n"), r["kernel_len"]
    assert not r["rollback_marker"]
    assert r["log_lines"] > 5, r["log_lines"]
    files = dict(r["appended_files"])
    assert "/nmbl-log/nmbl.log" in files, files
n, l = load("normal"), load("luks")
assert n["keyfiles"] == [], n["keyfiles"]
assert "/etc/nmbl-luks/cryptroot" not in dict(n["appended_files"])
# The typed test passphrase reaches stage 1 as the passToStage1 keyfile.
assert l["keyfiles"] == [["cryptroot", len("simbox-test-passphrase")]], l["keyfiles"]
assert dict(l["appended_files"])["/etc/nmbl-luks/cryptroot"] == len("simbox-test-passphrase")
s = load("splash")
assert s["init"] == "/nix/var/nix/profiles/system-2-link/init", s["init"]
assert "simbox.gen=2" in s["cmdline"], s["cmdline"]
# The dumped framebuffer is the real splash: not a blank buffer.
ppm = open(f"{work}/splash.ppm", "rb").read()
pixels = ppm.split(b"\n", 3)[3]
assert len(set(pixels[i:i+3] for i in range(0, len(pixels), 3000))) > 20, "blank framebuffer"
print("simbox: kexec handover assertions passed (normal boot, LUKS unlock, splash + X11 keys)")
PY
