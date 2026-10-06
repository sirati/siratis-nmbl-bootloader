# Whole boot-set updates

`boot.nmbl.bootUpdate.enable` replaces the fixed NMBL boot files with two
complete signed slots, `A` and `B`, in `nmbl-boot-sets/` on each boot root.
The GRUB config is a fixed dispatcher. It reads the `active` selector, sources
the slot's `bootloader` metadata, and loads `kernel` and `initrd` from that
slot with `nmbl.config=/nmbl-boot-sets/<slot>/config` on the kernel command
line. A missing selector or invalid metadata stops the boot with a message.
GRUB has no fallback to fixed kernel or initrd files.

NMBL accepts only `/nmbl-boot-sets/A/config` and `/nmbl-boot-sets/B/config`
from that parameter. It reads the config from the bootstrap boot filesystem
and verifies the sidecar at the same path plus `.sig` before it parses the
file. The slot config names the slot's rescue
image (`/nmbl-boot-sets/<slot>/rescue`) and, with a networking stage,
`/nmbl-boot-sets/<slot>/network`.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `bootUpdate.enable` | `false` | Turn on the A/B updater. |
| `bootUpdate.user` | `"nmbl-update"` | Account allowed to request an update. It needs an explicit numeric uid. |
| `bootUpdate.publicKey` | `null` | ML-DSA public key the privileged service verifies with. Required. |
| `bootUpdate.socketPath` | `"/run/nmbl-boot-update/update.sock"` | Request socket. |
| `bootUpdate.spoolPath` | `"/var/lib/nmbl-boot-update/spool"` | Directory the requester prepares bundles in. |
| `bootUpdate.bootRoots` | `[ "/boot" ]` | Boot filesystems, updated one complete mirror at a time. |

Evaluation also requires GRUB as the loader and enforced signing
(`signing.enable` and `signing.enforce`). `generationImage.bootstrapUpdates`
cannot be combined with boot sets.

The module runs `nmbl-boot-update serve SOCKET SPOOL PUBLIC_KEY UID BOOT_ROOT...`
as `nmbl-boot-update.service`. The service runs as root with the update
user's group, under `ProtectSystem=strict` with write access to the boot
roots, the spool and its runtime directory, and with only `AF_UNIX` sockets.
tmpfiles creates `/var/lib/nmbl-boot-update` with mode `0710` and the spool
with mode `0700`, owned by the update user.

## Slot contents

`nmbl-boot-update prepare` builds one slot bundle:

| File | Required |
| --- | --- |
| `bootloader` | yes |
| `kernel` | yes |
| `initrd` | yes |
| `rescue` | yes |
| `config` | yes |
| `network` | no |
| `tools` | no |

Each file has a `<name>.sig` sidecar. The service installs that sidecar next
to the file, so each file is signed under the domain its verifier uses:

| File | Domain |
| --- | --- |
| `config` | `boot-config` |
| `rescue` | `rescue-sfs` |
| `network` | `network-stage` |
| `tools` | `rescue-tools` |
| `bootloader`, `kernel`, `initrd` | `boot-set-artifact` |

The manifest parser rejects a file signed under another domain. `manifest.json` lists the role, destination, payload, signature file,
domain and SHA-512 of every member, the target slot, and a set id (a SHA-512
over the slot and the member digests). `manifest.json.sig` signs it under
`nmbl:boot-set-manifest:v1`. The selector changes only after the complete
inactive slot is on disk, synced, and verified again from disk.

## Operator workflow

Generate private keys outside the Nix store and pass only the public key to
the NixOS evaluation. Build `system.build.nmblBootSetSources.A` (or `.B`) and
`system.build.nmblBootSetTool`, then run:

```console
nmbl-boot-update prepare A "$boot_set_sources_A" \
  /var/lib/nmbl-boot-update/spool/first \
  /run/operator/private.key /run/operator/public.key
nmbl-boot-update request /run/nmbl-boot-update/update.sock \
  /var/lib/nmbl-boot-update/spool/first /run/operator/public.key
```

The full usage is:

```text
nmbl-boot-update prepare A|B SOURCE OUTPUT PRIVATE_KEY|- PUBLIC_KEY
nmbl-boot-update check BUNDLE PUBLIC_KEY
nmbl-boot-update request SOCKET BUNDLE PUBLIC_KEY
nmbl-boot-update serve SOCKET SPOOL PUBLIC_KEY UID BOOT_ROOT...
```

With `-` as the private key, `prepare` reads the key once from stdin, so no
key file is needed:

```console
nix-secrets pipe-secret nmbl-boot-key \
  | nmbl-boot-update prepare A "$boot_set_sources_A" \
      /var/lib/nmbl-boot-update/spool/first - /run/operator/public.key
```

`prepare` is safe Rust. It signs outside the store and then validates the
whole bundle with the public key. `request` validates the bundle again before
it connects. The service reads the peer's credentials, requires the update
user's uid, and requires that the peer's `/proc/<pid>/exe` is the same inode
as its own binary. It repeats that check after the complete request has
arrived. The bundle must be a direct child of the spool. The service reopens
and verifies every input, writes from the pinned descriptors, and verifies the
completed slot again before the rename.

The bundle must target the inactive slot. With no selector, the inactive slot
is `A`. A request for the set that is already active returns `Unchanged`
without rewriting its files or the selector.

When boot updates are on, the bootloader installer writes none of the fixed
NMBL files: no kernel, initrd, external config, rescue image, networking
stage, driver images or boot-partition splash. For initial enrollment, run
the service against the mounted target boot filesystem and request slot `A`
before you install or enable the GRUB dispatcher. Keep the old boot entry
until the first signed slot and selector have been verified.

## Space and failure behaviour

A normal update stages a complete inactive slot and changes the selector
last. When free space cannot hold the new set but free space plus the old
inactive slot can, the service deletes that inactive slot and rebuilds it.
The active slot stays selected and bootable. When even that is too small, the
request fails before any change. An interrupted write can lose the inactive
slot, but the selector cannot point at a mixture of two sets.

With several `bootRoots`, the service completes and verifies one mirror
before it touches the next. A failure on a later mirror leaves the earlier
mirrors bootable.

## Trust-key replacement

The update protocol cannot replace its own trust anchor. Build a replacement
NMBL initrd and config from an independently supplied public key B. Keep the
old key A in its `signing.publicKeys`, because key A signs the complete
transition slot, including the config that NMBL verifies at boot. After the
slot boots, restart the service with public key B. It then accepts only
B-signed sets, built for key B alone. Private keys stay on the operator
machine or in private runtime storage.

## Tests

`nix run .#test-boot-update-vm` builds the GRUB dispatcher into a standalone
EFI image at the UEFI fallback path `EFI/BOOT/BOOTX64.EFI`. It enrolls slot A
with key A, installs a B transition slot signed by key A, then an A slot
signed by key B. Each slot boots into its generation. A slot whose config is
signed under the wrong domain stops the boot. After the rotation, the service
refuses the old A-signed bundle. The test scans the store closures, the boot tree, the ESP and the
boot disks for both private keys and a random marker.

The `nmbl-boot-update-eval` check confirms that the slot sources and the tool
build and that each slot config names its own rescue path.
