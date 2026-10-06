# Phase F.5: network rescue end-to-end verification

This is a test record from 2026-05-28 (commit `09f54ce`). The console output
below is what that build printed. The section
[Changes since this record](#changes-since-this-record) lists what the code
does differently on 2026-10-06. The record contains no later run.

Test configuration at the time: `test-external-rescue-network`
(`testing/build_configurations.nix`). NixOS 26.05 (kernel 6.18.10), UEFI with
GRUB, flat busybox rescue (`rescue.mode = "external"`,
`rescue.squashfsContents`), `rescue.network = true`, `rescue.defaultUrl = ""`,
`serialConsole = true` (`ConsoleRescueUi`).

Host HTTP server: miniserve bound to `127.0.0.1:8080` serving
`/tmp/rescue-srv/nmbl-rescue.sfs` (700 416 bytes).
URL used in the VM: `http://10.0.2.2:8080/nmbl-rescue.sfs` (QEMU slirp NAT).

## Bugs found and fixed

### Bug 1: af_packet not loaded (commits 88f9f60, ac967ef)

`socket(AF_PACKET, SOCK_DGRAM, ETH_P_IP)` failed with `EAFNOSUPPORT`.
`af_packet` was in the initrd (through makeModulesClosure) but missing from
the `[kernel_modules].explicit` list in `config.toml`, so NMBL did not load it
before the DHCP raw-socket path.

Fix: `af_packet` was added to `extraExplicitModules` in `lib/config.nix` and to
the default of `boot.nmbl.explicitKernelModules` in `lib/options.nix`, both
conditional on `rescue.network` with `rescue.mode = "external"`. This is
still the code on 2026-10-06 (`rescuePacketModule` in `lib/config.nix`).

### Bug 2: loop and squashfs not loaded (commit 2e1d3c7)

After DHCP succeeded, the mount step failed with `/dev/loop-control: No such
file or directory`. `loop` and `squashfs` were in `extraExplicitModules`, so
they were in the initrd, but they were missing from
`boot.nmbl.explicitKernelModules`, the list written to `config.toml`. NMBL
did not load them.

Fix at the time: `loop` and `squashfs` were added to the
`explicitKernelModules` default in `lib/options.nix`, conditional on
`rescue.mode = "external"`. Commit `f0ad52f` (2026-06-01) replaced this fix.
See [Changes since this record](#changes-since-this-record).

## Test 1: golden path (DHCP, URL, download, hash confirm, BusyBox)

```
--- nmbl rescue: source picker ---
disk rescue failed:
  rescue stage locate-sfs failed: io error while rescue squashfs
  /mnt/boot/nmbl-rescue.sfs not found on boot partition: entity not found
Choose: [n]etwork / [r]eboot / [h]alt
n
--- nmbl rescue: rescue URL ---
Enter rescue URL (http://host/path):
http://10.0.2.2:8080/nmbl-rescue.sfs
[  163.105471] init[1]: memfd_create() called without MFD_EXEC or MFD_NOEXEC_SEAL set
[nmbl] download: 7200 / 700416 bytes (1%)
[nmbl] download: 23584 / 700416 bytes (3%)
...
[nmbl] download: 700416 / 700416 bytes (100%)
--- nmbl rescue: hash confirm ---
computed: d732ff9c569f5cc71ab59184f076618fd8b89aac3dfdb6494232cd2cac86ee9c
no expected hash pre-filled
Confirm? [y]es / [n]o-mismatch / [a]bort
y
[  294.909794] loop0: detected capacity change from 0 to 1368
BusyBox v1.37.0 () built-in shell (ash)
sh: can't access tty; job control turned off
#
```

Result: PASS. The run went through DHCP, the URL prompt, the HTTP fetch
(700 416 bytes), the SHA-256 computation, hash confirmation, the loop mount,
`switch_root` and the BusyBox shell.

## Test 2: wrong hash returns to the source picker

Same flow as Test 1 up to the hash confirmation, then `n` (no-mismatch):

```
--- nmbl rescue: hash confirm ---
computed: d732ff9c569f5cc71ab59184f076618fd8b89aac3dfdb6494232cd2cac86ee9c
no expected hash pre-filled
Confirm? [y]es / [n]o-mismatch / [a]bort
n
--- nmbl rescue: source picker ---
disk rescue failed:
  hash mismatch: computed d732ff9c569f5cc71ab59184f076618fd8b89aac3dfdb6494232cd2cac86ee9c
  did not match expected
Choose: [n]etwork / [r]eboot / [h]alt
```

Result: PASS. The hash rejection returned to the source picker with a
readable mismatch error. There was no panic and no Rust backtrace.

## Test 3: NIC link down gives a clean error

The NIC was disabled with the QEMU HMP command `set_link virtio-net-pci.0 off`
before network rescue was selected:

```
--- nmbl rescue: source picker ---
disk rescue failed:
  rescue stage locate-sfs failed: io error while rescue squashfs
  /mnt/boot/nmbl-rescue.sfs not found on boot partition: entity not found
Choose: [n]etwork / [r]eboot / [h]alt
n
[nmbl] network rescue: no carrier on eth0 after 10s; trying next NIC
========================================================================
NMBL: no rescue toolkit available — halting
========================================================================
...
  rescue stage network-rescue-failed failed: rescue stage net-no-iface
  failed: config invalid (exhausted 1 NIC(s)): no candidate NIC produced
  a DHCP lease
[  362.338643] reboot: System halted
```

Result: PASS. NMBL reported the carrier timeout as a diagnostic banner,
without a Rust panic or backtrace, and halted the system.

## Changes since this record

State of the code on 2026-10-06:

- NMBL loads the rescue disk modules on demand. Since commit `f0ad52f`,
  `loop` and `squashfs` are absent from the eager
  `boot.nmbl.explicitKernelModules` default. `rescueDiskModules` in
  `lib/config.nix` keeps their `.ko` files in the initramfs, and
  `ensure_rescue_disk_modules` in `nmbl-init-rs/src/rescue/disk.rs` loads
  `loop`, the image filesystem and `overlay` right before the loop mount.
  The image filesystem is `erofs` for the full-system rescue and `squashfs`
  for the flat busybox rescue.
- The network path (`mount_overlay_for_child` in
  `nmbl-init-rs/src/rescue/net/download.rs`) also calls
  `ensure_rescue_disk_modules` before it mounts the download.
- NMBL stays PID 1 during rescue. It mounts the image read-only under a tmpfs
  overlay at `/rescue` and runs the rescue `/init` as a chrooted child
  (`run_external_rescue_child` in `nmbl-init-rs/src/rescue/child.rs`). The
  `switch_root` in the Test 1 result describes the 2026-05-28 build.
- The network path still computes SHA-256 and pre-fills the expected value
  from `boot.nmbl.rescue.defaultSha256`. With signing enabled it also
  verifies `<url>.sig` under the `rescue-sfs` domain, and a config-pinned
  stage-2 SHA-512 must match unless the operator chooses another image. It
  mounts the download with the configured image format
  (`[rescue.image].format`).
- The full-system rescue boots from a host-independent EROFS image that
  `config.toml` pins by SHA-512 (`[rescue.image]`). See
  [docs/rescue-stages.md](../docs/rescue-stages.md).
