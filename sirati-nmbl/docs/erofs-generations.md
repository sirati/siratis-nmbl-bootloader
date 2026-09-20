# Signed EROFS generation switching

NMBL can verify a selected loop-backed EROFS image before mounting it as
`/nix`. This permits reboot-based updates on immutable systems without
repartitioning or replacing the NMBL UKI for every system generation.

## Disk layout

The writable image filesystem uses this layout:

```text
/boot/nmbl-generations/
  active -> generations/<sha512>
  previous -> generations/<sha512>
  tested -> generations/<sha512>
  pending -> generations/<sha512>
  attempted -> generations/<sha512>
  generations/<sha512>/
    generation
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
    automaticRescue = true;
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
line. The matching systemd target lets consumers react to that event. Failure
of a tested generation enters the signed external rescue when automatic rescue
is enabled. Atomic rename plus directory fsync protects every state update.

## Offline and remote deployment

The production command builds the unsigned image and signs it on the operator
machine. Local mode installs it directly:

```sh
nix run .#nmbl-erofs-deploy -- \
  .#nixosConfigurations.host /secure/offline/generation.key \
  /boot/nmbl-generations
```

Remote mode retains the private key on the operator machine and streams only
the signed bundle through SSH:

```sh
nix run .#nmbl-erofs-deploy -- remote \
  .#nixosConfigurations.host /secure/offline/generation.key update@host \
  --reboot
```

Configure the update key with `restrict` and a forced command equivalent to:

```text
sudo -n /run/current-system/sw/bin/nmbl-erofs-receive \
  /var/lib/nmbl-incoming /boot/nmbl-generations
```

The sudo rule must permit only that exact command and fixed paths. The receiver
reads a length-delimited stream into a private temporary directory, checks the
complete image hash, installs without changing `active`, and activates only
after the full stream validates. An interrupted transfer cannot affect the
next boot. NMBL verifies the signature before pre-kexec use; the target initrd
independently verifies it again after kexec.

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
