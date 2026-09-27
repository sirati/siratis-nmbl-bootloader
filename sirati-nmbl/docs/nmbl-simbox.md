# nmbl-simbox: the real NMBL PID 1 in a rootless container

`nmbl-simbox` runs the unmodified production `nmbl-init` binary as PID 1 in a
rootless podman container with every capability dropped. A supervisor outside
the container answers every kernel-facing syscall from a scenario description.
You get a fast, VM-free, root-free end-to-end run of the real boot path: device
discovery, LUKS unlock, mounts, the generation menu and the splash, through to
the kexec handoff.

```console
nix run .#nmbl-simbox -- run "$(nix build --print-out-paths .#simbox-scenario-normal)/scenario.toml"
nix run .#nmbl-simbox -- run …/simbox-scenario-luks/scenario.toml --reveal-keys
nix run .#nmbl-simbox -- run …/simbox-scenario-splash/scenario.toml --graphical
nix run .#test-nmbl-simbox        # automated: normal, LUKS, splash + X11 keys
```

The console shows NMBL's output on an 80×22-cell terminal, the grid of a
640×360 px screen with an 8×16 font. With `--graphical`, an X11 window also
shows the simulated DRM framebuffer, and keys typed into that window reach
NMBL as keyboard input. When NMBL performs the final kexec (load, then
`reboot(LINUX_REBOOT_CMD_KEXEC)`), the console says so and the window closes.
After you press Enter, the console shows what the kexec'd kernel would have
received.

## Mechanism: seccomp user notification

The container is started with an OCI seccomp profile in which the simulated
syscalls have the action `SCMP_ACT_NOTIFY` and everything else is allowed. The
`run.oci.seccomp.receiver` annotation makes crun send the filter's listener fd
to the supervisor's unix socket (`SCM_RIGHTS`). Each trapped syscall then
blocks in the kernel until the supervisor replies with
`SECCOMP_IOCTL_NOTIF_SEND`. The reply is one of:

- a synthetic result;
- an errno;
- `SECCOMP_USER_NOTIF_FLAG_CONTINUE`, which lets the kernel run the syscall
  for real;
- a file descriptor installed in the task with `SECCOMP_IOCTL_NOTIF_ADDFD`.

The alternatives were rejected for these reasons:

- **ptrace** needs the tracer in a position to trace a process in the
  container's user namespace, and Yama (`ptrace_scope=1`) restricts it. It
  also stops the tracee on every syscall.
- **LD_PRELOAD** does not work: `nmbl-init` is a static musl binary, and much
  of its syscall traffic comes from raw `libc::syscall` and rustix's
  `linux_raw` backend, not from libc wrappers.
- **Modifying the binary** is out: the point is to run the unmodified
  production binary.

The container runs in a user namespace owned by the invoking user. The
supervisor therefore holds capabilities in that namespace, while the
container's processes hold none. That is enough to:

- read and write `/proc/<pid>/mem` for syscall arguments and results;
- duplicate the task's fds (`pidfd_getfd`), for example the kernel and initrd
  handed to `kexec_file_load`;
- join the container's user and mount namespaces (in a forked helper) to
  perform the bind mounts that stand in for block-device mounts.

After reading target memory, and before replying,
`SECCOMP_IOCTL_NOTIF_ID_VALID` guards against the task having died or been
replaced.

## What is simulated

| Area | Behaviour |
|---|---|
| `mount` of a block device or LUKS mapper | Faithful. `/dev/disk/by-*` symlinks are resolved inside the container. The fstype must match the device's blkid `TYPE` (`EINVAL` otherwise), and an unknown device gives `ENOENT`. The device's scenario tree (a private writable copy per run) is bind-mounted at the target. |
| `mount` with `MS_BIND`, `umount2` | Performed for real in the container's namespaces. |
| `proc`/`sysfs`/`devtmpfs`/`tmpfs`/`devpts` mounts | Stubbed: they succeed without effect. podman provides `/proc`, `/dev` and `/tmp`; `/sys` is the scenario's tree. |
| `MS_REMOUNT` | Stubbed: succeeds. |
| `mknod`/`mknodat` | Creates the node as an empty regular file (unprivileged), or returns `EEXIST`. |
| `stat`/`lstat`/`newfstatat`/`statx` on simulated `/dev` nodes | Faithful: reports a block device with the scenario's major:minor, following symlinks. Everything else runs for real. |
| `uname` | The host's utsname with the scenario's kernel release, so module lookups use the initramfs' `/lib/modules/<release>`. |
| `init_module`/`finit_module` | The image is read and its `.modinfo` `name=` recorded. A repeat load gives `EEXIST`. |
| `blkid`, `cryptsetup` | Real processes: the simbox binary, copied in as `/bin/blkid` and `/bin/cryptsetup`, answers from the scenario. NMBL's fork/exec, stdin passphrase pipe, exit codes and wrong-passphrase retry (exit 2) all run for real. `cryptsetup open` checks the typed passphrase against the scenario and creates `/dev/mapper/<name>`. |
| DRM/KMS (`--graphical`) | Faithful to the kernel ABI for the subset the splash uses: version, caps, resources, connector (one connected Virtual connector with one preferred mode at the scenario size), encoder, CRTC get/set, dumb buffer create/map/destroy, framebuffer add/remove. `/dev/dri/card0` is a memfd installed via ADDFD, and `MAP_DUMB` returns offset 0, so NMBL's `mmap` maps shared memory that the X11 viewer reads. Every `SETCRTC` is a frame. |
| VT input (`--graphical`) | `/dev/tty1` is a dedicated pty whose master the X11 window types into. `VT_ACTIVATE`, `VT_WAITACTIVE`, `KDSETMODE`, `KDGETMODE`, `KDSKBMODE`, `KDGKBMODE` and `KDGKBLED` answer as a VT would. |
| Console | A real pty, sized 80×22, as the container's `/dev/console`. Raw mode, input parsing and rendering all run for real. |
| `kexec_file_load` | Faithful capture: kernel and initrd are read through the task's own fds and the cmdline from its memory. Nothing is loaded. |
| `reboot` | `LINUX_REBOOT_CMD_KEXEC` ends the run with the captured handoff (`EINVAL` if nothing was loaded). Restart, halt and power-off end the run with that outcome. |
| `kexec_load` | `ENOSYS`. |
| Loop devices, TPM, efivars, network interfaces | Not simulated. `/dev/loop-control`, `/dev/tpmrm0` and `/sys/firmware/efi` are absent, so NMBL takes its no-device paths. Scenarios that need signed EROFS generations, driver images, a measured boot or network rescue therefore fail or degrade exactly as on hardware without those devices. |

## Handover inspection

The report after kexec uses `nmbl_init::handover`, the same decoders
`nmblctl status` uses:

- **Kernel cmdline:** split into NMBL-set parameters (`init=`, the rollback
  marker, `nmbl.*`) and the generation's own parameters.
- **Initrd additions:** the NMBL cpio fragment after the compressed system
  initrd, with each file's path, size and type.
- **Boot log:** decoded into levelled lines.
- **LUKS unlock material** (`passToStage1` keyfiles, from a typed passphrase or
  a TPM-unsealed token): volume, format and length, masked. `--reveal-keys`
  shows the value, which is only acceptable because scenario keys are test
  keys.
- **Unknown blobs:** shown as hex and strings.

`--json FILE` writes the structured facts; the automated test asserts on them.

## Scenarios

A scenario is a TOML file (see `nmbl-simbox/src/scenario.rs`) that defines:

- the initramfs tree used as the container root;
- the kernel release and `/proc/cmdline`;
- block devices, each with major:minor, blkid attributes and a filesystem
  tree;
- LUKS volumes (device, mapper name, passphrase, mapper tree);
- the framebuffer size;
- scripted key input, each item optionally gated on text appearing on the
  console.

`testing/simbox/scenarios.nix` builds three scenarios from real NixOS
configurations, so the container runs the actual production initramfs:

- `normal`: GPT disk with an ESP and an ext4 root holding three generations;
- `luks`: a password-unlocked root with `passToStage1`;
- `splash`: the `image-splash` build.

## Limits

- x86_64 syscall numbers only.
- Filesystems are directory trees. There are no on-disk formats, so fsck,
  resize and btrfs device scanning are not exercised.
- Device timing is instantaneous: every device exists from the start.
- NMBL's own stat of simulated device nodes is faithful. Other tools inside the
  container, such as busybox in the emergency shell, see regular files unless
  they use the trapped stat calls.
- podman provides a real `/proc`. `/proc/cmdline`, `/proc/modules` and
  `/proc/sys/kernel/printk` are bind-mounted scenario files, and
  `/proc/modules` does not track simulated module loads.
