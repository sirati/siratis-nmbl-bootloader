# nmbl-simbox: the real NMBL PID 1 in a rootless container

`nmbl-simbox` runs the unmodified production `nmbl-init` binary as PID 1 in a
rootless podman container with every capability dropped. A supervisor outside
the container answers every kernel-facing syscall from a scenario description.
The run needs no VM and no root, and it covers the real boot path: device
discovery, LUKS unlock, mounts, the generation menu and the splash, up to the
kexec handoff.

```console
nix run .#nmbl-simbox -- run "$(nix build --print-out-paths .#simbox-scenario-normal)/scenario.toml"
nix run .#nmbl-simbox -- run …/simbox-scenario-luks/scenario.toml --reveal-keys
nix run .#nmbl-simbox -- run …/simbox-scenario-splash/scenario.toml --graphical
nix run .#test-nmbl-simbox        # automated: normal, LUKS, and splash with X11 keys
```

`nmbl-simbox`, `test-nmbl-simbox` and the three `simbox-scenario-*` builds are
packages of the `sirati-nmbl` flake. The `nmbl-simbox` package puts rootless
podman and crun on `PATH`.

| Option | Effect |
|---|---|
| `--headless` | Forwards no keyboard input, opens no X11 window, skips the Enter prompt after kexec and prints the report without colour. Scripted keys from the scenario still run. |
| `--graphical` | Simulates a DRM card and shows it in an X11 window (splash builds). With `--headless` the card is simulated without a window. |
| `--frame-dump FILE.ppm` | Writes the last simulated framebuffer to a PPM file at the end. |
| `--reveal-keys` | Shows LUKS unlock material in clear in the report. |
| `--json FILE` | Writes the structured handover facts. |
| `--trace` | Prints the log of simulated syscalls to stderr at the end. With `--json FILE`, also writes the console transcript next to it with the extension `.console`. |
| `--timeout SECS` | Ends the run after this many seconds (default 120). |
| `--init PATH` | Runs another `nmbl-init` binary as PID 1 (default: `/init` of the scenario's initramfs). |

The console shows NMBL's output on an 80 by 22 cell terminal, the grid of a
640 by 360 pixel screen with an 8 by 16 font. With `--graphical`, an X11 window
also shows the simulated DRM framebuffer, and keys typed into that window reach
NMBL as keyboard input. When NMBL performs the final kexec (load, then
`reboot(LINUX_REBOOT_CMD_KEXEC)`), the console says so and the window closes.
After you press Enter, the console shows what the kexec'd kernel would have
received.

## Mechanism: seccomp user notification

The container starts with an OCI seccomp profile in which the simulated
syscalls have the action `SCMP_ACT_NOTIFY` and all other syscalls are allowed.
The `run.oci.seccomp.receiver` annotation makes crun send the filter's
listener fd to the supervisor's unix socket (`SCM_RIGHTS`). Each trapped
syscall then blocks in the kernel until the supervisor replies with
`SECCOMP_IOCTL_NOTIF_SEND`. The reply is one of:

- a synthetic result;
- an errno;
- `SECCOMP_USER_NOTIF_FLAG_CONTINUE`, which lets the kernel run the syscall
  for real;
- a file descriptor installed in the task with `SECCOMP_IOCTL_NOTIF_ADDFD`.

Other approaches do not fit:

- ptrace needs the tracer in a position to trace a process in the container's
  user namespace, and Yama (`ptrace_scope=1`) restricts that. It also stops
  the tracee on every syscall.
- `LD_PRELOAD` has no effect. `nmbl-init` is a static musl binary, and much of
  its syscall traffic comes from raw `libc::syscall` and rustix's `linux_raw`
  backend.
- A modified binary would defeat the purpose, which is to run the production
  binary.

The container runs in a user namespace owned by the invoking user. The
supervisor therefore has capabilities in that namespace, and the container's
processes have none. That is enough to:

- read and write `/proc/<pid>/mem` for syscall arguments and results;
- duplicate the task's fds (`pidfd_getfd`), for example the kernel and initrd
  handed to `kexec_file_load`;
- join the container's user and mount namespaces (in a forked helper) to
  perform the bind mounts that replace block-device mounts.

After reading target memory and before replying, the supervisor calls
`SECCOMP_IOCTL_NOTIF_ID_VALID` to detect a task that died or was replaced.

## What is simulated

| Area | Behaviour |
|---|---|
| `mount` of a block device or LUKS mapper | Faithful. The supervisor resolves `/dev/disk/by-*` symlinks inside the container. The fstype must match the device's blkid `TYPE` (`EINVAL` otherwise), and an unknown device gives `ENOENT`. The device's scenario tree (a private writable copy per run) is bind-mounted at the target. |
| `mount` with `MS_BIND`, and `umount2` | Performed for real in the container's namespaces. |
| `proc`, `sysfs`, `devtmpfs`, `tmpfs` and `devpts` mounts | Stubbed: they succeed without effect. podman provides `/proc`, `/dev` and `/tmp`. `/sys` is the scenario's tree. |
| `MS_REMOUNT` | Stubbed: succeeds. |
| `mknod` and `mknodat` | Creates the node as an empty regular file, or returns `EEXIST`. |
| `stat`, `lstat`, `newfstatat` and `statx` on simulated `/dev` nodes | Faithful: reports a block device with the scenario's major and minor numbers, following symlinks. All other paths run for real. |
| `uname` | The host's utsname with the scenario's kernel release, so module lookups use the initramfs' `/lib/modules/<release>`. |
| `init_module` and `finit_module` | The supervisor reads the image and records its `.modinfo` `name=`. A repeat load gives `EEXIST`. |
| `blkid`, `cryptsetup` | Real processes. The simbox binary, copied in as `/bin/blkid` and `/bin/cryptsetup`, answers from the scenario. NMBL's fork and exec, the stdin passphrase pipe, exit codes and the wrong-passphrase retry (exit 2) all run for real. `cryptsetup open` checks the typed passphrase against the scenario and creates `/dev/mapper/<name>`. |
| DRM and KMS (`--graphical`) | Faithful to the kernel ABI for the subset the splash uses: version, caps, resources, connector (one connected Virtual connector with one preferred mode at the scenario size), encoder, CRTC get and set, dumb buffer create, map and destroy, framebuffer add and remove. `/dev/dri/card0` is a memfd installed with `ADDFD`, and `MAP_DUMB` returns offset 0, so NMBL's `mmap` maps shared memory that the X11 viewer reads. Every `SETCRTC` is a frame. |
| VT input (`--graphical`) | `/dev/tty1` is a dedicated pty whose master the X11 window types into. `VT_ACTIVATE`, `VT_WAITACTIVE`, `KDSETMODE`, `KDGETMODE`, `KDSKBMODE`, `KDGKBMODE` and `KDGKBLED` answer as a VT would. |
| Console | A real pty, sized 80 by 22, as the container's `/dev/console`. Raw mode, input parsing and rendering all run for real. |
| `kexec_file_load` | Faithful capture: the supervisor reads kernel and initrd through the task's own fds and the cmdline from its memory. Nothing is loaded. |
| `reboot` | `LINUX_REBOOT_CMD_KEXEC` ends the run with the captured handoff (`EINVAL` if nothing was loaded). Restart, halt and power-off end the run with that outcome. |
| `kexec_load` | `ENOSYS`. |
| Loop devices, TPM, efivars, network interfaces | Not simulated. `/dev/loop-control`, `/dev/tpmrm0` and `/sys/firmware/efi` are absent, so NMBL takes its no-device paths. Scenarios that need signed EROFS generations, driver images, measured boot, the stage-2 rescue image or network rescue therefore fail or degrade as on hardware without those devices. |

## Handover inspection

The report after kexec uses `nmbl_init::handover`, the same decoders
`nmblctl status` uses:

- the kernel cmdline, split into NMBL-set parameters (`init=`, the rollback
  marker, `nmbl.*`) and the generation's own parameters;
- the initrd additions: the NMBL cpio fragment after the compressed system
  initrd, with each file's path, size and type;
- the boot log, decoded into levelled lines;
- LUKS unlock material (`passToStage1` keyfiles, from a typed passphrase or a
  TPM-unsealed token), shown as volume, format and length, masked.
  `--reveal-keys` shows the value. Scenario keys are test keys;
- unknown blobs, shown as hex and strings.

`--json FILE` writes the structured facts. The automated test asserts on
them: the kexec outcome, `init=`, the cmdline, the kernel, the appended
`/nmbl-log/nmbl.log`, and for the LUKS scenario the `/etc/nmbl-luks/cryptroot`
keyfile length.

## Scenarios

A scenario is a TOML file (see `nmbl-simbox/src/scenario.rs`) that defines:

- the initramfs tree used as the container root;
- the kernel release and `/proc/cmdline`;
- block devices, each with major and minor numbers, blkid attributes and a
  filesystem tree;
- LUKS volumes (device, mapper name, passphrase, mapper tree);
- the framebuffer size;
- scripted key input, where each item can wait for text on the console.

`testing/simbox/scenarios.nix` builds three scenarios from real NixOS
configurations, so the container runs the production initramfs:

- `normal`: a GPT disk with a VFAT ESP and an ext4 root that holds three
  system profiles. NMBL boots generation 3.
- `luks`: a password-unlocked LUKS root with `passToStage1`. The typed
  passphrase reaches stage 1 as the keyfile.
- `splash`: the `image-splash` build. `test-nmbl-simbox` runs it on Xvfb,
  types Down and Enter into the framebuffer window, and checks that NMBL boots
  generation 2.

## Limits

- x86_64 syscall numbers only.
- Filesystems are directory trees. There are no on-disk formats, so fsck,
  resize and btrfs device scanning are not exercised.
- Every device exists from the start. Device timing is not simulated.
- NMBL's own stat of simulated device nodes is faithful. Other tools inside the
  container, such as busybox in the emergency shell, see regular files unless
  they use the trapped stat calls.
- podman provides a real `/proc`. `/proc/cmdline`, `/proc/modules` and
  `/proc/sys/kernel/printk` are bind-mounted scenario files, and
  `/proc/modules` does not track simulated module loads.
