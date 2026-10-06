# Signed EROFS generations

With `boot.nmbl.generationImage.enable`, the target system's `/nix` is a
signed, loop-backed EROFS image. NMBL verifies the selected image before
kexec, and the target initrd verifies it again after kexec. Each system
generation is a new image in a new directory, so an update needs no
repartitioning and no new NMBL kernel or initrd.

## Disk layout

The generation tree lives at `generationImage.stateRoot` (default
`/boot/nmbl-generations`). It can sit on `/boot`, on a larger dedicated
filesystem such as `/persistent`, or on the target root filesystem:

```text
<stateRoot>/
  active -> generations/<sha512>
  previous -> generations/<sha512>
  tested -> generations/<sha512>
  pending -> generations/<sha512>
  attempted -> generations/<sha512>
  rollback-event               # "<failed> <restored>" after an automatic rollback
  generations/<sha512>/
    generation                 # the directory name
    nix.erofs
    nix.erofs.sig
    config.toml
    config.toml.sig
    kernel.sig
    initrd.sig
    rescue.sfs
    rescue.sfs.sig
    network.erofs              # only with a rescue networking stage
    network.erofs.sig
    system                     # optional
```

The directory name is the lowercase SHA-512 of `nix.erofs`.
`nmbl-erofs-deploy remote` fills every member above. The local mode of
`nmbl-erofs-deploy` installs only `generation`, `nix.erofs` and
`nix.erofs.sig`.

Selectors are relative symlinks. `nmbl-erofsctl` and NMBL replace a selector
by creating a temporary link, renaming it over the old one and syncing the
directory. `nmbl-erofsctl install` copies a generation into a temporary
directory, syncs it, and renames it into `generations/` before any selector
can point at it. A crash therefore leaves either the old complete generation
or the new complete generation selected. The tree and each generation
directory have mode `0700`, so service accounts cannot read or change the
images or selectors.

NMBL's pre-kexec trust decision does not depend on the directory name. NMBL
opens the image once, verifies its sidecar under the `nmbl:generation-image:v1`
ML-DSA domain over that descriptor, and passes the same descriptor to
`LOOP_CONFIGURE`. A path replacement between verification and mount does not
change the mounted bytes.

When `stage1Store` is set, NMBL reads the generation kernel and initrd
signatures (`gen-kernel` and `gen-initrd` domains) from `<stateRoot>/active/`.
The kernel and initrd come from the system toplevel inside the verified image.
Without `stage1Store`, NMBL looks for those signatures under
`/boot/nmbl/sigs/<gen-id>/`.

This design protects integrity and keeps the signing roles apart. It does not
stop an attacker who can rewrite the `active` symlink from selecting an older
signed generation. Rollback protection needs a TPM NV counter or another
monotonic authority, and NMBL does not implement one. The pinned descriptor
stops path substitution, but the backing inode stays writable by root. The
`0700` tree keeps non-root accounts out. Protection against a compromised root
process that writes the active inode needs fs-verity or dm-verity on the
backing storage.

## Post-kexec verification

The loop device and mount that NMBL creates belong to the first kernel and do
not survive kexec, so the target NixOS initrd opens the selected image again.
The module installs the `nmbl-generation-mount` helper from `nmbl-init` and a
copy of NMBL's `config.toml` (as `/etc/nmbl/generation-mount.toml`) in the
systemd initrd. The helper runs after `sysroot.mount` and `sysroot-boot.mount`.
It opens `/sysroot<image path>`, verifies the sidecar at
`/sysroot<signaturePath>` with the public keys baked into `nmbl-init`, binds
the same descriptor to a read-only loop device, and mounts it at
`/sysroot/nix` with `ro,nodev,nosuid`.

A native `sysroot-nix.mount` unit replaces the fstab-generated loop mount. Its
source is `/dev/nmbl-verified-generation`, which the helper publishes only
after signature verification and `LOOP_CONFIGURE` succeed. When verification
fails, the unit has no source device and the boot stops. The module requires
`boot.initrd.systemd.enable`; evaluation fails for the scripted stage 1.

## Configuration

The NixOS filesystem entry and the post-kexec initrd both use the stable
selection path. Boot assessment needs the external NMBL config:

```nix
fileSystems."/nix" = {
  device = "/boot/nmbl-generations/active/nix.erofs";
  fsType = "erofs";
  neededForBoot = true;
  options = [ "loop" "ro" ];
};

boot.initrd.systemd.enable = true;

boot.nmbl = {
  configLocation = "external";
  signing = {
    enable = true;
    enforce = true;
    publicKeys = [ ./generation-public-key ];
    deferInstallSigning = true;
  };
  generationImage = {
    enable = true;
    mountPoint = "/nix";
    signaturePath = "/boot/nmbl-generations/active/nix.erofs.sig";
    automaticRollback = true;
  };
  rescue.automatic = true;
};
```

`generationImage` options:

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `false` | Verify and mount the selected image before and after kexec. |
| `mountPoint` | `"/nix"` | Must match exactly one loop-backed EROFS entry in `fileSystems`. |
| `signaturePath` | `"/boot/nmbl-generations/active/nix.erofs.sig"` | Sidecar of the selected image. |
| `stateRoot` | `"/boot/nmbl-generations"` | Generation tree. Must lie below `/boot` or below `stage1Store.targetMountPoint`. |
| `automaticRollback` | `false` | Roll back a failed untested generation to the tested one. |
| `successDelaySec` | `30` | Delay before the first success assessment. |
| `stage1Store` | `null` | Mount the filesystem holding `stateRoot` during NMBL stage 1. |
| `bootstrapUpdates` | `false` | Ship the NMBL kernel and initrd inside the generation (BIOS GRUB only, see below). |

Evaluation also requires `signing.enable` and `signing.enforce`, an absolute
backing-file path for the image, and `configLocation = "external"` whenever
`automaticRollback` or `rescue.automatic` is set. The option
`generationImage.automaticRescue` was renamed to `boot.nmbl.rescue.automatic`.

To keep large images off a small boot partition, put the tree on a dedicated
filesystem:

```nix
fileSystems."/persistent" = {
  device = "/dev/disk/by-label/NMBLSTORE";
  fsType = "btrfs";
  neededForBoot = true;
  options = [ "subvol=nmbl" "noexec" "nodev" "nosuid" ];
};

fileSystems."/nix" = {
  device = "/persistent/nmbl-generations/active/nix.erofs";
  fsType = "erofs";
  neededForBoot = true;
  options = [ "loop" "ro" ];
};

boot.nmbl.generationImage = {
  enable = true;
  stateRoot = "/persistent/nmbl-generations";
  signaturePath = "/persistent/nmbl-generations/active/nix.erofs.sig";
  stage1Store = {
    targetMountPoint = "/persistent";
    runtimeMountPoint = "/mnt/nmbl-generation-store";   # the default
  };
};
```

`stage1Store.targetMountPoint` must match exactly one needed-for-boot
filesystem. The module adds that filesystem's type to the bootstrap kernel
modules. `runtimeMountPoint` must be an absolute, non-root path without `..`,
and `stateRoot` must lie strictly below the target with no `.` or `..`
components. For state tracking, NMBL mounts the store at `runtimeMountPoint`
before it reads the selectors. For a Btrfs store it first creates the
`/dev/disk/by-*` links and scans Btrfs devices. When the store is the same
block device as the bootstrap boot filesystem, the kernel refuses a second
mount with `EBUSY`. NMBL then bind-mounts the bootstrap mount and remounts
the shared superblock with the store options. The bootstrap mount keeps its
own read-only flag.

Remove separate mounts below `/nix` that would shadow the image, such as
persistent subvolumes at `/nix/store` or `/nix/var`. Keep the backing device
mounted as needed-for-boot storage. Stage the first signed image before you
switch the boot configuration.

A host with a single root filesystem needs no repartitioning. Mark `/` needed
for boot and use a root-relative tree. NMBL still mounts that filesystem at
its private stage-1 path:

```nix
fileSystems."/" = {
  device = "/dev/disk/by-label/NIXOS";
  fsType = "ext4";
  neededForBoot = true;
  options = [ "noexec" "nodev" "nosuid" ];
};

fileSystems."/nix" = {
  device = "/nmbl-generations/active/nix.erofs";
  fsType = "erofs";
  neededForBoot = true;
  options = [ "loop" "ro" ];
};

boot.nmbl.generationImage = {
  enable = true;
  stateRoot = "/nmbl-generations";
  signaturePath = "/nmbl-generations/active/nix.erofs.sig";
  stage1Store.targetMountPoint = "/";
};
```

Only the public key is a build input. Keep private keys out of Nix paths and
option values.

## Boot assessment and rollback

State tracking is on when `generationImage.automaticRollback` or
`boot.nmbl.rescue.automatic` is true. Before kexec, NMBL takes an exclusive
`flock` on the state directory and reads the selectors:

1. With no `attempted` selector, NMBL points `attempted` at `active` and boots
   it.
2. When `attempted` names the `pending` generation, `automaticRollback` is on,
   and a different `tested` generation exists, NMBL points `active` and
   `attempted` at the tested generation, removes `pending` and writes
   `rollback-event`. It adds `nmbl.rollback-after-untested-new-generation-failed`
   to the target kernel command line, and the systemd target of the same name
   starts on that boot.
3. Any other `attempted` state means the previous boot failed and no rollback
   target exists. `boot.nmbl.rescue.automatic` alone decides the outcome.
   `true` enters the configured rescue, and `false` opens the emergency menu.

In the booted system, `systemd-boot-check-no-failures` runs before
`boot-complete.target`. The `nmbl-generation-success` timer fires
`successDelaySec` seconds after boot and then every 30 seconds. Each run of
`nmbl-generation-success.service` repeats the failed-unit check in
`ExecStartPre`, then runs `nmbl-generation-state mark-success <stateRoot>`
under the same lock. That command points `tested` at `active` and removes
`pending`, `rollback-event` and `attempted`. It fails when `attempted` is
missing or differs from `active`. A boot with failed units stays attempted.
If a unit fails until delayed secrets arrive and then recovers, a later timer
run marks the generation tested.

`nmbl-erofsctl activate` takes the same lock. It points `previous` at the old
active generation, `active` at the new one, and `pending` at the new one
unless it is already tested. It also removes `attempted` and `rollback-event`.
Activation therefore works during an unconfirmed or degraded boot. The
running boot can no longer mark anything tested, and the new generation gets
its own first attempt on the next boot. Only `mark-success` sets `tested`, so
with no successful predecessor `tested` stays absent.

`nmbl-erofsctl rollback` points `active` at `previous` and requires a reboot.
It refuses while an `attempted` selector exists, which on a state-tracking
host lasts until the running boot is marked tested.

`nmblctl status` shows the selector state on these hosts. `nmblctl` refuses
to write a remembered default, a one-shot or a retry by generation number on
a signed EROFS host and names `nmbl-erofsctl activate` and `rollback`.

## Deployment

`nmbl-erofs-deploy` builds the unsigned image and the external runtime config
and signs them on the operator machine:

```text
nmbl-erofs-deploy INSTALLABLE PRIVATE_KEY IMAGE_ROOT
nmbl-erofs-deploy remote INSTALLABLE PRIVATE_KEY SSH_TARGET [--reboot]
```

The command keeps every built artifact under GC roots in a
`nmbl-erofs-deploy.XXXXXXXX` directory in the current directory. It prints
that path and deletes the directory when it exits. A process killed without
cleanup leaves the directory behind for the operator to remove.

Local mode installs and activates the image in a local tree:

```sh
nix run .#nmbl-erofs-deploy -- \
  .#nixosConfigurations.host /secure/offline/generation.key \
  /boot/nmbl-generations
```

Remote mode keeps the private key on the operator machine and streams the
signed bundle through SSH:

```sh
nix run .#nmbl-erofs-deploy -- remote \
  .#nixosConfigurations.host /secure/offline/generation.key update@host \
  --reboot
```

Pass `-` as the key path and set `NMBL_SIGN_KEY_COMMAND` to a shell command
line that prints the key. The command runs once per signature under the
caller's `PATH`, and its output goes to `nmbl-sign sign --key-stdin`:

```sh
NMBL_SIGN_KEY_COMMAND='nix-secrets pipe-secret nmbl-generation-key' \
  nix run .#nmbl-erofs-deploy -- remote .#nixosConfigurations.host - update@host
```

`nmbl-erofsctl prepare IMAGE - OUT_DIR` accepts the same convention.
`NMBL_EROFS_SSH` replaces the `ssh` command, `NMBL_EROFS_REMOTE_COMMAND`
replaces the remote `nmbl-erofs-receive` name, and
`NMBL_EROFS_DEPLOY_IMPURE=1` adds `--impure` to the Nix calls.

Remote mode signs these members:

| File | Domain |
| --- | --- |
| `nix.erofs` | `generation-image` |
| `config.toml` | `boot-config` |
| `kernel`, `initrd` of the system toplevel | `gen-kernel`, `gen-initrd` |
| `rescue.sfs` | `rescue-sfs` |
| `network.erofs` | `network-stage` |
| `rescue-tools.erofs` | `rescue-tools` |

NMBL does not add the receiver to the system packages. Add
`config.system.build.nmblErofsReceive` to `environment.systemPackages`, then
give the update key `restrict` and a forced command equivalent to:

```text
sudo -n /run/current-system/sw/bin/nmbl-erofs-receive \
  /var/lib/nmbl-incoming /persistent/nmbl-generations \
  /etc/nmbl/trusted-update.pub
```

The sudo rule must permit only that exact command with fixed paths. The
receiver is a compiled program from `nmbl-host-tools`, never a script, since
it runs as root on uploaded data. It reads the length-delimited
`NMBL-EROFS-BUNDLE-4` stream into a
private temporary directory, rejects payloads over 64 GiB and trailing data,
and checks the image and config digests. It verifies every signature against
the fixed public key before installation. If the generation directory already
exists, its contents must match the upload. The receiver then installs the
generation, compares and verifies the installed copies, and activates it
with the same state transitions as `nmbl-erofsctl activate`. It discards the
uploaded `kernel` and `initrd` and keeps only their signatures.

Point the bootstrap config at the active generation and make the bootstrap
filesystem the one that contains the tree:

```nix
boot.nmbl.bootstrap.configPath = "/nmbl-generations/active/config.toml";
```

One rename of `active` then selects the config, the image, the rescue image
and the networking stage together. No crash state pairs a new config with an
old image. An interrupted transfer before that rename leaves the prior
generation active. The image build includes the external config derivation
as a closure root, so a config-only change produces a new image hash and a
new generation directory.

Rollback and garbage collection:

```sh
nmbl-erofsctl rollback /boot/nmbl-generations
systemctl reboot

nmbl-erofsctl gc 2 /boot/nmbl-generations
nmbl-erofsctl status /boot/nmbl-generations
```

`gc KEEP` keeps the `KEEP` newest generations plus every generation a
selector points at.

## Installer behaviour

With `generationImage.enable`, the bootloader installer stages no external
config, rescue image or networking stage on `/boot`. Those files reach the
generation directory only through a signed deploy. The installer still writes
the NMBL kernel, initrd and GRUB config. Set `signing.deferInstallSigning = true`
on these hosts, because the deploy produces the signatures. NMBL resolves
`rescue.sfsPath` to `<state>/active/rescue.sfs` and
`rescue.fullSystem.networkStage.imagePath` to `<state>/active/network.erofs`,
relative to the boot volume NMBL mounts (the stage-1 store when `stage1Store`
is set, otherwise `/boot`). The config pins both images by SHA-512, as
described in [rescue-stages.md](rescue-stages.md).

The bootstrap filesystem may be the same partition as the stage-1 store, for
example one persistent ext4 that contains the config and the generations. A
tmpfs `/` is mounted directly without a device wait.

## BIOS bootstrap updates

`generationImage.bootstrapUpdates = true` puts the NMBL kernel and initrd into
the generation image at `/nmbl-bootstrap/kernel` and `/nmbl-bootstrap/initrd`.
It requires BIOS boot with GRUB and cannot be combined with
`boot.nmbl.bootUpdate`. The GRUB dispatcher (`lib/grub-dispatcher.nix`) loads
`<state>/active/bootstrap-kernel` and `<state>/active/bootstrap-initrd`
through the `active` symlink. The deploy tool must write those two files from
the verified image's `/nmbl-bootstrap/` into the generation directory. The
artifact receiver of `nix-update-remote` does this. The in-repo tools
(`nmbl-erofs-deploy`, `nmbl-erofs-receive` and `nmbl-erofsctl install`) do
not write them, so bootstrap updates need that external receiver.

## Tests

| Name | Kind | Covers |
| --- | --- | --- |
| `generation-image-vm-test` | app | Local deploy, signed image verification and the post-kexec mount. |
| `generation-state-vm-test` | app | Selector state, activation during an unconfirmed boot, rollback. |
| `test-erofs-bios-host-vm` | package | BIOS GRUB host with vfat `/boot`, ext4 `/persistent` and tmpfs `/`, two generations delivered by `nmbl-erofs-deploy remote` through key commands, rollback of a failed untested generation, rescue with the networking stage after a tested generation fails. |
| `generation-success-late-secrets` | check | A unit that fails until secrets arrive, then a later success mark. |
| `generation-image-initrd` | check | Initrd mount unit depends on the verifying helper. |
| `generation-root-store-eval` | check | Root-filesystem store layout and mount order. |
| `nmbl-erofs-bios-host-eval` | check | Production shape of the BIOS host configuration. |
| `nmbl-erofsctl`, `nmbl-erofs-receive` | checks | Tool behaviour without a VM. |
