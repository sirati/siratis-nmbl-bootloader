# NMBL install-test workflow — notes & gotchas

Captures hard-won knowledge from getting `nix run .#install-test-gpt-*` working end-to-end. Read this first if you're picking up `nixos-anywhere-test/` and wondering why something is the way it is — the answer is usually "because that was the only way".

## Status

| Config | Single-disk | RAID1 |
|---|---|---|
| `gpt-bios` | ✅ PASS | ❌ blocked on mdadm/kernel-6.18 superblock regression |
| `gpt-uefi-grub` | ✅ PASS | ❌ same |
| `gpt-uefi-systemd` | ✅ PASS | ❌ same |

Boot chain in all PASS cases: `firmware → bootstrapper → NMBL kernel → NMBL init script → kexec → installed NixOS`.

## What the orchestrator does (4 stages)

1. Boot SystemRescue 13.00 in QEMU under user-mode networking with `hostfwd tcp::PORT-:22`. The user's SSH pubkey is injected via a small FAT32 aux disk that SystemRescue's `autorun` mechanism picks up.
2. `nixos-anywhere --store-paths $DISKO_SCRIPT $NIXOS_SYSTEM -i $KEY --target-host root@localhost --ssh-port $PORT --post-kexec-ssh-port $PORT --extra-files extra-files --no-reboot`. The installer kexecs SystemRescue into a noninteractive NixOS installer, runs disko + nixos-install + NMBL's `system.build.installBootLoader`, then halts.
3. Power-off rescue VM, relaunch QEMU with the proper firmware (SeaBIOS for bios, OVMF for uefi) and no CD/no -kernel. Disk1's bootloader chains into NMBL → kexec into installed NixOS.
4. SSH in, sanity-check `hostname`, `lsblk`, `/boot` contents. Print `===== PASS: install-gpt-X =====`.

## Lessons that bit us

### `nixos-anywhere`'s `-i` is *not* the same as `ssh`'s `-i`

`ssh -i pubkey.pub` works (it looks up the matching agent identity). `nixos-anywhere -i pubkey.pub` doesn't — its `uploadSshKey` (around `src/nixos-anywhere.sh:540`) copies the file verbatim to `$tempDir/nixos-anywhere`, then runs `ssh-keygen -y -f` on it to derive the pubkey. With a `.pub` input, that pipeline produces a garbage temp key, `ssh-copy-id` installs the wrong pubkey, and **every reconnect after kexec fails with `Permission denied (publickey)`** silently — without ever telling you the input was wrong.

**Fix:** the orchestrator now requires `--ssh-key PATH` (private key) or `SSH_PRIVATE_KEY=<contents>` env. Pubkey is derived via `ssh-keygen -y -f`.

### `--post-kexec-ssh-port` defaults to 22

After kexec, nixos-anywhere reconnects to the host's original `--ssh-port` value but resets it to 22 unless `--post-kexec-ssh-port` is passed. With QEMU usermode `hostfwd tcp::22001-:22`, the post-kexec ssh hits **`localhost:22` on the host** (whatever sshd or nothing is listening there), not the kexec'd installer in the guest. Symptom: identical "Permission denied (publickey)" to the `-i` bug above.

**Fix:** pass `--post-kexec-ssh-port "$PORT"` explicitly.

### Don't `--flake` if your flake imports siblings

`nixos-anywhere --flake path:/nix/store/HASH-nixos-anywhere-test#install-gpt-bios` evaluates the flake at that store path. The path is a **subtree-only copy**; `import ../sirati-nmbl/flake.nix` inside it resolves to `/nix/store/sirati-nmbl` (one level up = `/nix/store/`), pure-eval refuses with `access to absolute path '/nix/store/sirati-nmbl' is forbidden`.

**Fix:** switch to `--store-paths $DISKO_SCRIPT $NIXOS_SYSTEM`. Pre-build both at orchestrator-build time — sibling imports happen at that point, where `self.outPath` includes the whole repo tree thanks to a synthesised `self` in `sirati-nmbl/flake.nix` (see "Cross-flake sibling access" below).

### Cross-flake sibling access via synthesised `self`

When `sirati-nmbl/flake.nix` does `import ../nixos-anywhere-test/flake.nix` and calls its `outputs`, the natural choice is `self = nixosAnywhereTestFlake`. But that `self` has no `outPath`, and the imported flake's orchestrator embeds `"${self}"` (the source path) into a shell script. Plain interpolation breaks; if you give it `outPath = ../nixos-anywhere-test`, Nix copies only that subtree to the store and siblings vanish.

The trick:

```nix
nixosAnywhereTestFlake = import ../nixos-anywhere-test/flake.nix;
nixosAnywhereTest = nixosAnywhereTestFlake.outputs {
  self = nixosAnywhereTestFlake // {
    # self.outPath of a ?dir=sirati-nmbl flake already includes the
    # /sirati-nmbl suffix; dirOf gives the repo root, then we append the
    # sibling sub-tree path — still inside the whole-tree store copy.
    outPath = builtins.dirOf self.outPath + "/nixos-anywhere-test";
  };
  inherit nixpkgs disko nixos-anywhere;
};
```

This way `"${self}"` inside `nixos-anywhere-test`'s orchestrator interpolates to a store path **inside the whole-repo copy**, so `../sirati-nmbl` etc. still resolve.

### Home-manager 1Password ssh-agent override

`~/.ssh/config` is a home-manager-managed symlink to `/nix/store/.../home-manager-files/.ssh/config`. It contains:

```
Match host * exec "test -z $SSH_TTY"
  IdentityAgent ~/.1password/agent.sock
```

In any non-TTY context (orchestrator scripts, subprocesses), this overrides `SSH_AUTH_SOCK` and points ssh at 1Password. If 1Password's agent holds many identities, ssh blows through `MaxAuthTries` before getting to the one you actually wanted, returning a confusing "Too many authentication failures."

**Fix without disabling the agent:** combine `-i $KEY -o IdentitiesOnly=yes`. With those, ssh ignores agent identities entirely and only tries the explicit private-key file — the home-manager override becomes harmless. **Do not** add `IdentityAgent=none`; the user wants the agent available for normal flows.

### QEMU port collision when retrying

When two orchestrator runs share a port (e.g. two `install-test-gpt-uefi-systemd` runs both on host:22003), QEMU's slirp `hostfwd` *silently* fails to bind on the second one — QEMU keeps running, just unreachable. The Stage 4 SSH then lands on the **other** still-alive VM (typically a stale rescue env from a prior run), and you get bizarre verify output (`hostname=sysrescue`, `/boot` contains only memtest86+).

Run orchestrators **strictly sequentially**, and kill stale `qemu-system-x86_64` processes between runs.

### NMBL has no udev — must populate `/dev/disk/by-*` by hand

NixOS init normally relies on udev to create `/dev/disk/by-{partlabel,label,uuid,partuuid}/` symlinks. NMBL's stage-0 init is much smaller and doesn't run udev. Disko's generated `fileSystems."/" = { device = "/dev/disk/by-partlabel/disk-main-root"; ... }` therefore fails to mount with `Can't lookup blockdev`.

`sirati-nmbl/scripts/mount-and-kernel.sh.nix` now walks `/sys/class/block/`, runs `blkid -o export` on each, and creates the four kinds of symlinks. `pkgs.util-linux` (for `blkid`) is added to the initrd's `storePaths`.

### NMBL has no udev — must assemble mdadm arrays by hand

Same root cause: udev would auto-assemble via `mdadm --incremental`. Without it, `/dev/md*` never appears. The mount script now does `mdadm --stop --scan` (clear stale state from firmware peeks at /boot ESP, important for raid1+UEFI) then `mdadm --assemble --scan --force`. `pkgs.mdadm` is added to the initrd's `storePaths`.

### Storage-driver assertion needs to know about raid

`sirati-nmbl/lib/modules/storage-validation.nix` previously required a kernel module literally named `raid` for `/dev/md*` filesystems. That's not a real module name. Now it expects `md_mod` + a level-specific module (`raid1`, `raid0`, `raid10`, `raid456`, …) and accepts the umbrella `raid` as an alias.

### Disko output reuse — `--store-paths` over `--flake`

The orchestrator does `installConfigs.<name>.config.system.build.diskoScript` and `…build.toplevel` at Nix-eval time and passes both store paths to `nixos-anywhere --store-paths`. This:

1. Sidesteps the sibling-flake-eval problem above.
2. Pre-builds everything on the host with full access to the Nix store, then nixos-anywhere just `nix copy`s the closures to the target — no remote evaluation, faster, more robust.

### Disko-generated layouts use partlabels, not labels

For our setup disko names partitions `disk-main-boot`, `disk-main-ESP`, `disk-main-root` (and for raid1 `boot`/`root` arrays). NMBL's mount script consumes those via `/dev/disk/by-partlabel/...` — verify with `lsblk -o NAME,SIZE,TYPE,FSTYPE,PARTLABEL` on the installed system.

## Outstanding: RAID1 + kernel 6.18 superblock validation regression

All three RAID1 configs install cleanly (disko creates arrays, mkfs runs, nixos-install completes, NMBL bootloader files land in `/boot`). On boot, NMBL's `mdadm --assemble --scan` returns EINVAL on both arrays.

Investigation localised this to `super_1_load` in kernel 6.18's `md-mod.ko`. The new check there reads bytes around offset 0xe0 of the v1.x mdadm superblock (the `devflags`/`bblog_shift`/`bblog_size`/`bblog_offset` region) and rejects superblocks where those bytes aren't all zero. mdadm 4.4 writes `bblog_shift=0x02` by default, which trips the check.

Things that were tried and **did not** fix it:
- Switching the NMBL bootloader kernel from `linux_6_6` → `linux_6_18` (the check is in 6.18 itself).
- `--bitmap=none` in disko `extraArgs` (clears Feature Map bit 0x1, but `bblog_shift` is a separate field).
- `dyndbg=file super1.c +p` confirms the failure point in dmesg.

Plausible next steps (untried):
- `mdadm --grow /dev/md/X --update=no-bbl` as a disko `postCreateHook`. This is what mdadm exposes for zeroing the bad-block-log fields after an array exists.
- A newer mdadm (5.x) that's aware of 6.18's stricter check. Not in nixpkgs at time of writing.
- A raw `dd` patch of bytes 0xe0..0xe8 of each member's superblock as part of disko post-create.
- Install with a rescue VM kernel ≤ 6.12 so mdadm-from-userspace and md-from-kernel are in sync.

If you pick this up: `nix run .#install-test-gpt-uefi-systemd-raid1 -- --ssh-key /tmp/install-test-keys/key`, watch `stage3.log` for `md: super_1_load` lines (visible thanks to the `dyndbg=...` boot param still set in `install-configs.nix`).

## Bug fixes that landed along the way

| File | What changed | Why |
|---|---|---|
| `vm-serial-man-rs/src/manager/qemu.rs` | QEMU serial socket per-pid (`qemu-serial-${pid}.sock`) | Global `/tmp/qemu-serial.sock` collided across parallel VMs |
| `nixos-anywhere-test/flake.nix` | `-display none` instead of `-nographic` | `-nographic` is incompatible with `-daemonize` |
| `nixos-anywhere-test/flake.nix` | `--ssh-key PATH` (private key required) | nixos-anywhere's `-i` needs a private key (see above) |
| `nixos-anywhere-test/flake.nix` | `--post-kexec-ssh-port $PORT` | Defaults to 22 (see above) |
| `nixos-anywhere-test/flake.nix` | `--store-paths` instead of `--flake` | Sibling-flake-eval problem (see above) |
| `nixos-anywhere-test/flake.nix` | Pre-seed `/root/.ssh/authorized_keys` via ssh before invocation | nixos-anywhere's kexec script greps the rescue env's authorized_keys at kexec time; format / readability assumptions vary |
| `sirati-nmbl/flake.nix` | Synthesised `self.outPath` via `builtins.dirOf` (see "Cross-flake sibling access") | Was the only way to keep relative `import ../X/flake.nix` working across the wrapper |
| `sirati-nmbl/lib/config.nix` | `pkgs.util-linux` + `pkgs.mdadm` added to initrd storePaths | NMBL has no udev (see above) |
| `sirati-nmbl/scripts/mount-and-kernel.sh.nix` | Walk `/sys/class/block` + `blkid` → `/dev/disk/by-*`; `mdadm --stop --scan` then `--assemble --scan --force` | NMBL has no udev (see above) |
| `sirati-nmbl/lib/modules/storage-validation.nix` | Accept `md_mod` + raid-level modules + alias `raid` | The literal module name `raid` doesn't exist (see above) |
