# Phase F.5 — Network Rescue End-to-End Verification

Test configuration: `test-external-rescue-network`
NixOS 26.05 (kernel 6.18.10), UEFI+GRUB, `rescue.network = true`,
`serialConsole = true` (ConsoleRescueUi).

Host HTTP server: miniserve bound to `127.0.0.1:8080` serving
`/tmp/rescue-srv/nmbl-rescue.sfs` (700 416 bytes).
URL used in VM: `http://10.0.2.2:8080/nmbl-rescue.sfs` (QEMU slirp NAT).

## Bugs found and fixed

### Bug 1 — af_packet not loaded (commit 88f9f60, ac967ef)

`socket(AF_PACKET, SOCK_DGRAM, ETH_P_IP)` failed with EAFNOSUPPORT
because `af_packet` was present in the initrd (via makeModulesClosure)
but was absent from `config.toml`'s `[kernel_modules].explicit` list.
NMBL never called `modprobe af_packet` before the DHCP raw-socket path.

Fix: added `af_packet` to `extraExplicitModules` in `lib/config.nix`
and to the default of `boot.nmbl.explicitKernelModules` in
`lib/options.nix` (conditional on `rescue.network && mode == "external"`).

### Bug 2 — loop and squashfs not loaded (commit 2e1d3c7)

After DHCP succeeded the mount step failed: `/dev/loop-control: No such
file or directory`.  `loop` and `squashfs` were in `extraExplicitModules`
(so they were in the initrd) but were not in `boot.nmbl.explicitKernelModules`
(the list serialised into `config.toml`).  NMBL never ran
`modprobe loop` or `modprobe squashfs`.

Fix: added `loop` and `squashfs` to the `explicitKernelModules` default
in `lib/options.nix` (conditional on `rescue.mode == "external"`).

## Test 1 — Golden path (DHCP → URL → download → hash confirm → BusyBox)

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

**Result: PASS** — full path DHCP → URL prompt → HTTP fetch (700 416 B)
→ SHA-256 computed → hash confirmed → loop-mount → switch_root →
BusyBox shell.

## Test 2 — Wrong hash returns to source picker

Same flow as Test 1 up to hash confirm, then 'n' (no-mismatch):

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

**Result: PASS** — hash rejection returns cleanly to source picker with
a human-readable mismatch error.  No panic, no Rust backtrace.

## Test 3 — NIC link-down: clean error, no panic

NIC disabled via QEMU HMP `set_link virtio-net-pci.0 off` before
selecting network rescue:

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

**Result: PASS** — carrier-detect timeout surfaced as a clean diagnostic
banner.  No Rust panic, no backtrace.  System halted gracefully.
