# Whole boot-set updates

`boot.nmbl.bootUpdate.enable` replaces mutable fixed boot files with two
complete signed slots below `nmbl-boot-sets`. A small fixed GRUB image reads
the strict `active` selector and loads the kernel, initrd, and external NMBL
configuration from one slot. That configuration names the rescue and optional
network images in the same slot. Missing selectors and malformed metadata fail
closed; GRUB does not fall back to legacy fixed kernel or initrd files.

Each slot contains a signed manifest, signed boot metadata, kernel, initrd,
rescue image, external config, and optional network image. The manifest binds
the role, destination, digest, target slot, and signature of every member. The
selector changes only after the complete inactive slot is durable and has been
reopened and verified from disk.

## Operator workflow

Generate private keys outside the Nix store. Pass only a public key into the
NixOS evaluation. Build `system.build.nmblBootSetTool`, then run:

```console
nmbl-boot-update prepare A "$boot_set_sources_A" \
  /var/lib/nmbl-boot-update/spool/first \
  /run/operator/private.key /run/operator/public.key
nmbl-boot-update request /run/nmbl-boot-update/update.sock \
  /var/lib/nmbl-boot-update/spool/first /run/operator/public.key
```

The first command is implemented in safe Rust. It signs outside the store and
then validates every input. The client validates the bundle again before it
connects. The privileged service checks peer UID and executable identity both
after accept and at the action boundary, confines the requested directory to
the spool, independently reopens and verifies every input, writes from pinned
descriptors, and reopens the completed slot for another verification.

Initial enrollment uses this same client/service protocol against the mounted
target boot filesystem before installing or enabling the stable GRUB
dispatcher. The installer deliberately does not create unsigned fixed-path
fallback artifacts when boot updates are enabled. Keep the old boot entry
available until the first signed slot and selector have been verified.

## Space and failure behavior

Normal updates stage a complete inactive slot and change one selector last.
When free space cannot hold the new set but free space plus the complete
inactive slot can, the receiver deletes only that inactive slot and rebuilds
it. The active slot stays selected and bootable. If even that capacity is too
small, validation fails before mutation. An interrupted write can lose the
inactive slot but cannot expose a cross-generation mixture through the
selector.

For mirrored boot filesystems, the receiver completes and verifies one mirror
before touching the next. This keeps one known-good copy during low-space
replacement. Reinstalling the active manifest returns `Unchanged` without
rewriting its files or selector.

## Trust-key replacement

The update protocol cannot replace its own trust anchor. Build a replacement
NMBL initrd/config from independently supplied public key B. Key A signs the
complete transition slot. After that slot boots, restart the privileged
service with public key B and accept only B-signed sets. Private keys remain on
the operator machine or private runtime storage throughout.

The checked VM test boots the exact generated GRUB dispatcher through the UEFI
fallback path, consumes A and B slot configs, exercises this key transition,
rejects stale A signatures, and scans evaluated sources, closures, bundles,
ESP and boot disks for private-key bytes.

This mechanism is for Stardust and the DNS VPS when they use signed EROFS main
generations. Hetzner2/3 keep their normal Btrfs Nix store and normal NixOS
switch workflow. Their small signed external rescue and network images remain
supported on `/boot`; enabling `generationImage` or whole main-system EROFS
updates is not required.
