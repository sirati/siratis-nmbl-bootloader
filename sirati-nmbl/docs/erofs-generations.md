# Signed EROFS generation switching

NMBL can verify a selected loop-backed EROFS image before mounting it as
`/nix`. This permits reboot-based updates on immutable systems without
repartitioning or replacing the NMBL UKI for every system generation.

## Disk layout

The writable image filesystem uses this layout. It may live on `/boot`, on a
larger dedicated filesystem such as `/persistent`, or on the target root
filesystem:

```text
/boot/nmbl-generations/
  active -> generations/<sha512>
  previous -> generations/<sha512>
  tested -> generations/<sha512>
  pending -> generations/<sha512>
  attempted -> generations/<sha512>
  generations/<sha512>/
    generation
    config.toml
    config.toml.sig
    nix.erofs
    nix.erofs.sig
    system                 # optional NixOS toplevel metadata
```

Selectors are relative symlinks replaced with one `rename(2)`. The image and
its detached signature are installed into a temporary directory, flushed,
and renamed into `generations/` before `active` can reference them. A crash
therefore leaves either the old complete generation or the new complete
generation selected. `previous` supplies an operator-controlled rollback.
`tested`, `pending`, and `attempted` drive boot assessment and rollback. The
image root and generation directories are mode `0700`; ordinary service users
cannot inspect or modify the backing images or selectors.

The directory name is the lowercase SHA-512 of `nix.erofs`. The pre-kexec boot
trust decision does not rely on that name: NMBL opens the image once, verifies
its ML-DSA sidecar under the dedicated `nmbl:generation-image:v1` domain, and
passes the same pinned file descriptor to `LOOP_CONFIGURE`. A path replacement
between that verification and mount cannot change the mounted bytes.

This protects integrity and role separation. It does not provide anti-rollback
against an attacker who can rewrite the mutable selection symlink. Enforcing a
minimum generation requires a TPM NV counter or an equivalently monotonic
external authority and is deliberately a later protocol extension.

Pinned descriptors prevent pathname substitution, but they do not make a
regular backing inode immutable. The `0700` image tree prevents every
non-root service account from opening it for modification. Resistance to an
already-compromised root process writing the active inode requires fs-verity
or dm-verity on the backing storage; that remains a separate hardening step.

## Post-kexec verification boundary

The loop device and mount created by NMBL belong to the first kernel and do not
survive `kexec`. The target NixOS initrd therefore reopens the selected image.
The generation-image module installs the static `nmbl-generation-mount` helper
in the systemd initrd for that second trust decision. After the mutable root
and `/boot` are mounted, it opens
`/sysroot/boot/nmbl-generations/active/nix.erofs` and verifies the sidecar
with the public key baked into the helper, attaches the same descriptor to a
read-only loop device, and mounts it at `/sysroot/nix` with `nodev,nosuid`.

A native `sysroot-nix.mount` unit replaces the ordinary fstab-generated loop
mount. Its source is `/dev/nmbl-verified-generation`, which the helper publishes
only after signature verification and `LOOP_CONFIGURE` succeed. Verification
failure therefore cannot fall back to reopening the mutable image pathname.
The module requires a systemd initrd; evaluation fails for scripted stage 1.

When the image tree is outside `/boot`, NMBL mounts its backing filesystem at
a private stage-1 path before it reads selectors or opens the EROFS image. The
same filesystem is then mounted or bind-mounted at its final target before
`/nix`. `stage1Store.targetMountPoint` must identify exactly one filesystem
that is needed for boot. The private runtime mount must be an absolute,
non-root path, and `stateRoot` must remain strictly below the target with no
`.` or `..` components.

## Configuration

The NixOS filesystem entry and the post-kexec initrd must both use the stable
selection path:

```nix
fileSystems."/nix" = {
  device = "/boot/nmbl-generations/active/nix.erofs";
  fsType = "erofs";
  neededForBoot = true;
  options = [ "loop" "ro" ];
};

boot.nmbl = {
  signing = {
    enable = true;
    enforce = true;
    publicKeys = [ ./generation-public-key ];
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

For a dedicated persistent filesystem, keep large images off the small boot
partition:

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
    runtimeMountPoint = "/mnt/nmbl-generation-store";
  };
};
```

On an existing Hetzner layout that mounts persistent subvolumes at both
`/nix/store` and `/nix/var`, remove those two mounts in the generation-image
configuration. They would shadow paths inside the verified `/nix` EROFS.
Keep the Btrfs device mounted at `/persistent` as needed-for-boot storage, and
let the single verified EROFS mount supply the complete `/nix` tree after the
next reboot. Stage the first signed image before switching the boot config.

A Stardust or fresh DNS VPS with a single root filesystem needs no
repartitioning. Mark `/` needed for boot
and use a root-relative image tree; NMBL still mounts the filesystem privately
during stage 1 rather than treating `/` as its private mountpoint:

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
  stage1Store = {
    targetMountPoint = "/";
    runtimeMountPoint = "/mnt/nmbl-generation-store";
  };
};
```

Only the public key is an immutable build input. Do not use a private key as a
Nix path or option value.

Activating a new image marks it pending while preserving the last tested
generation. NMBL records the attempted selection before kexec. After the normal
system reaches `multi-user.target`, a delayed `boot-complete.target` health
check runs `systemd-boot-check-no-failures`; only then does
`nmbl-generation-success` mark the selection tested. A degraded boot remains
attempted. On the next boot an untested failure rolls back first and adds
`nmbl.rollback-after-untested-new-generation-failed` to the kernel command
line. The matching systemd target lets consumers react to that event. When no
rollback target exists (a tested generation failed, or an untested one without
a tested predecessor), `boot.nmbl.rescue.automatic` alone decides: `true`
enters the configured rescue, `false` opens the emergency menu. Atomic rename
plus directory fsync protects every state update.

## Offline and remote deployment

The production command builds the unsigned image and external runtime config,
then signs both on the operator machine. Local mode installs the image directly:

```sh
nix run .#nmbl-erofs-deploy -- \
  .#nixosConfigurations.host /secure/offline/generation.key \
  /boot/nmbl-generations
```

Remote mode retains the private key on the operator machine and streams the
signed generation and config bundle through SSH:

```sh
nix run .#nmbl-erofs-deploy -- remote \
  .#nixosConfigurations.host /secure/offline/generation.key update@host \
  --reboot
```

The private key need not be a file. Pass `-` instead of the key path and set
`NMBL_SIGN_KEY_COMMAND` to a shell command line that prints the key; it runs
once per signature and is piped into `nmbl-sign sign --key-stdin`:

```sh
NMBL_SIGN_KEY_COMMAND='nix-secrets pipe-secret nmbl-generation-key' \
  nix run .#nmbl-erofs-deploy -- remote .#nixosConfigurations.host - update@host
```

`nmbl-erofsctl prepare IMAGE - OUT_DIR` accepts the same convention.

Configure the update key with `restrict` and a forced command equivalent to:

```text
sudo -n /run/current-system/sw/bin/nmbl-erofs-receive \
  /var/lib/nmbl-incoming /persistent/nmbl-generations \
  /etc/nmbl/trusted-update.pub
```

The sudo rule must permit only that exact command and fixed paths. The receiver
reads a length-delimited stream into a private temporary directory and verifies
both detached signatures against the fixed public key before installation.
The config, config signature, image, and image signature live in the same
content-addressed generation directory. Configure the bootstrap filesystem as
the filesystem holding that directory and set
`configPath = "/nmbl-generations/active/config.toml"`. One atomic `active`
symlink rename then selects the config and image together; there is no crash
state containing a new config with an old image or the reverse. An interrupted
transfer before that rename leaves the prior pair active. NMBL repeats
generation verification before pre-kexec use and in the target initrd after
kexec. The external config derivation is also an EROFS closure root, ensuring
a config-only change produces a different image hash and generation directory.

Rollback selects the preserved predecessor and requires a reboot:

```sh
nmbl-erofsctl rollback /boot/nmbl-generations
systemctl reboot
```

Garbage collection protects every referenced selector, regardless of its
retention argument:

```sh
nmbl-erofsctl gc 2 /boot/nmbl-generations
```

The update account needs no shell and no direct access to the image root.
Recovery networking and SSH belong in a separately signed NMBL driver/rescue
image so normal generations can change without rebuilding the immutable UKI.

## Rescue and network stage in the generation directory

With `generationImage.enable`, the rescue image and the optional signed
network stage live in the selected generation directory, next to
`config.toml`: `rescue.sfsPath` becomes `<state>/active/rescue.sfs` and
`rescue.fullSystem.networkStage.imagePath` becomes `<state>/active/network.erofs`,
relative to the boot volume NMBL mounts (the stage-1 store when
`stage1Store` is set, otherwise `/boot`). `nmbl-erofs-deploy remote` signs
both (`rescue-sfs`, `network-stage`) and the receiver verifies them with the
rest of the bundle before `active` changes, so a switch never pairs a new
config with an old rescue or network image.

This works on BIOS/GRUB hosts as well as UEFI. With generation images the
installer writes only GRUB and the NMBL kernel/initrd to `/boot`; config,
rescue and network images reach the generation directory only through a
signed deploy.

The bootstrap filesystem may be the same partition as the stage-1 store (one
persistent ext4 holding config and generations, as on the DNS VPS). NMBL then
bind-mounts the already-mounted bootstrap filesystem for the store and
remounts it read-write for state updates; the bootstrap view stays
read-only. A tmpfs `/` (an impermanent root) is mounted directly without a
device wait.

`test-erofs-bios-host-vm` boots this layout from a real BIOS disk image:
SeaBIOS and GRUB from the MBR and BIOS boot partition, the NMBL kernel on a
vfat `/boot`, generations on an ext4 `/persistent`, and a tmpfs `/`. Two
generations are signed through key commands and delivered by
`nmbl-erofs-deploy remote`. The test boots the tested generation, fails an
untested one, verifies the automatic rollback and its kernel command-line
marker, fails the tested generation, and requires the signed rescue with the
signed network stage from the generation directory, a static address, the
hardened sshd settings, and a recovery SSH login with the dedicated key.
`nmbl-erofs-bios-host-eval` checks the production shape of the same
configuration without booting.
