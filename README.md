This project is under development and large broken or untested.

It for now only aims to support NixOS. 

No more boot loader is the concept of using Linux as a bootloader. This allows for more complex setups than possible with traditional boot loaders. Most importantly being able to boot any disk configuration that would be mountable by Linux.

It also allows for rich behaviours such as detecting when a Linux boot failed, and only displaying boot options in such a case or during frequent rebooting. It also may allow limited local and remote shell access should booting fail.

## Device timeout

`boot.nmbl.deviceTimeoutSeconds = 30` (default) sets the per-device wait budget used during boot-fs mount and after LVM/cryptsetup activation while waiting for devices to appear. Bump it for slow storage controllers.

## Logging

NMBL keeps a 1 MiB byte ring inside the bootloader that captures every `nmbl_info!`, `nmbl_warn!`, and `nmbl_verbose!` line emitted by `nmbl-init`. The ring is flushed and `fsync`'d to `/nmbl-log/nmbl.log` before kexec, reboot, `execve`, or halt.

The log is cpio-injected into the booted initramfs alongside any LUKS key passthrough. A stage-1 `nmbl-log-import` systemd unit replays each line into the journal under the `nmbl-init` tag before `initrd-switch-root.target`, so the boot loader's diagnostics survive the kexec boundary.

Post-boot:

```
journalctl -b | grep nmbl-init
```

If the in-memory ring overflowed, the replayed log starts with a header of the form `=== nmbl-init: log truncated, earlier <N> bytes dropped ===`.

## Stateful boot tracking

Opt-in via `boot.nmbl.stateful.enable = true`. NMBL then maintains a 16 KiB CBOR `state.bin` at `${stateDir}/state.bin` (default `/boot/nmbl/state.bin`) on a RW twin mount of `/boot`, recording which generations have booted to the configured success target.

Options:

- `boot.nmbl.stateful.maxRecoveryAttempts` (default `5`) — how many rollback attempts before NMBL drops to the emergency screen.
- `boot.nmbl.stateful.successTarget` (default `multi-user.target`) — the systemd target that, once reached, marks the current generation good.
- `boot.nmbl.stateful.stateDir` (default `/boot/nmbl`) — where `state.bin` lives.
- `boot.nmbl.stateful.rwMountpoint` (default `/mnt/boot-state`) — mountpoint NMBL uses for its RW twin of `/boot`.

Boot flow: every `nixos-rebuild boot` re-initialises `state.bin` via `nmbl-init --init-state ${stateDir}`. At boot, if the previous boot did not reach `successTarget`, NMBL selects a known-good generation from a sliding 20-slot window and boots that instead. After `maxRecoveryAttempts` consecutive failures, NMBL drops to the emergency screen rather than continuing to roll back.

The on-disk format is forward-compatible: every post-v1 field on `state.bin` is `serde(default)`, so older `nmbl-init` binaries silently ignore unknown fields. A binary that finds a strictly newer `format_version` logs a fatal line through the logging facility above and falls back to non-stateful boot.
