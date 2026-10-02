# nmblctl and instant boot

`nmblctl` is the system-side control and inspection tool. It is added to
`environment.systemPackages` whenever `boot.nmbl.enable` is set. Every
subcommand except `--help` requires root.

```console
nmblctl [--color=auto|always|never] [--no-pager] <command>
```

| Command | Effect |
|---|---|
| `chain` | The configured boot chain: firmware, loader, config location, signing (algorithm, enforce, baked key fingerprints), generation mode and list, rescue, TPM/secure boot, stateful settings, operator selections. |
| `status` | The current boot: how it was selected, NMBL-set cmdline parameters, rescue sentinel, automatic rescue, signed-generation state (tested/pending/attempted), stateful `state.bin`, and the units blocking the success mark. |
| `reboot-rescue [--yes]` | Writes the rescue sentinel durably, then runs `systemctl reboot`. Refuses when `rescue.mode = "none"`. |
| `reboot-into [--generation N]` | Boots a generation once on the next boot, then reboots. Without `--generation`, a list picker opens. |
| `default [--generation N \| --latest \| --show]` | Sets or shows the persistent default. `--latest` always picks the newest generation. |

`chain` and `status` are coloured and paged like git: `$PAGER`, else
`less -R`. Output goes straight to stdout when stdout is not a TTY, when
`less` is missing, or with `--no-pager`.

## Remembered choice

Before `nmblctl`, NMBL did not remember a choice made in its boot menu. The
menu always defaulted to the active Nix profile. `state.bin` records the
last attempted generation only for rollback.

`nmblctl` writes two files in the state directory (`/boot/nmbl` by default):

- `boot-default` contains `latest` or `generation N`.
- `boot-once` contains `generation N`. NMBL uses it once and then deletes it,
  like `grub-reboot`.

The selector uses the one-shot first, then the persistent default, then the
active profile. A file that names a generation that is no longer installed is
ignored. The format lives in `nmbl_init::boot_selection` and is shared by the
writer and the boot-time reader.

### Signed EROFS generations

On signed EROFS hosts the `active` symlink selects the boot generation, and
NMBL verifies the target's image and config before mounting them. A plain
flag file must not be able to select a generation in its place. On these
hosts:

- `nmblctl reboot-into` and `nmblctl default` refuse to write a generation
  override. Use `nmbl-erofsctl activate` or `rollback` instead, which verify
  the target's signature before the atomic rename.
- The boot-time resolver ignores these files.

Signature verification is unchanged.

## Instant boot

```nix
boot.nmbl.instantBoot.enable = true;
```

NMBL skips the selector countdown and boots the default generation at once
when all of the following hold:

- the last boot succeeded (stateful `last_boot_succeeded`, or the active
  signed generation is `tested`);
- this boot is not a stateful rollback or fallback, and no untested
  generation was rolled back;
- there is no rescue sentinel and no pending untested generation;
- no key was pressed during early boot.

Otherwise the normal timeout applies. Without a boot-health source
(stateful tracking or signed generation state), instant boot never
triggers.

NMBL starts listening for keys as soon as `/dev/console` exists: an
`O_RDONLY|O_NONBLOCK` descriptor in non-canonical mode, polled through the
bootstrap, module, mount and verification phases. Any byte sets a flag that
stays set after the descriptor is closed for console bring-up. From then
on, the interactive console's latch records presence. One key anywhere before
the selector cancels instant boot. Input is read from `/dev/console`, which
is the primary console: the last `console=` argument (the serial line when
`boot.nmbl.serialConsole` is set).

## UI preview (development only)

`nix run .#nmbl-ui-preview` opens NMBL's boot UI in an X11 window. The
separate `nmbl-ui-preview` crate drives it with mock state: fake generations,
boot progress, LUKS prompt and verification, wrong passphrase, emergency
menu, rescue confirmation, and signature errors. It renders through NMBL's
own code, unchanged: `ui::render_app` for the ratatui views and
`ui::composite_frame`, the pipeline the DRM splash uses. Only the output
surface (an X11 window) and the state source differ.

- Tab and Shift+Tab switch scenarios, F5 resets, q quits. Every other key
  goes to NMBL's own key handler.
- `--scenario NAME` sets the starting scenario; `--list` lists the names.
- `--size WxH` sets the window size.
- `--dump-ppm DIR` renders every scenario to PPM files without X11.

The mock code exists only in that crate, and no other crate depends on it.
The `nmbl-ui-preview-absent` flake check greps every production binary
(nmbl-init in its default, splash and full-feature builds, nmblctl, nmbl-sign
and nmbl-boot-update) for the preview's marker string and crate name. The
check also requires the marker to be present in the preview binary itself,
so it cannot pass vacuously.

## Retry after automatic recovery exhaustion

`nmblctl retry-generation --generation N` authorizes one attempt of an installed
profile after repairing the cause of failure. It verifies the bootable closure
and configured generation signatures, writes a private one-use request, and
reboots. NMBL consumes that request durably before recording the attempt; the
normal pinned signature check still runs before kernel loading. Recovery counters
and known-good history are retained. A failed retry returns to automatic rescue;
only the ordinary successful-boot health policy rearms automatic recovery.
Signed EROFS numeric retries are refused.

The production control binary and dependencies are included in full-system
rescue. From its authenticated root shell, inspect mounted installed state:

```console
nmblctl --config /nmbl-root/mnt/boot/nmbl/config.toml \
  --system-root /nmbl-root/mnt/system --state-dir /nmbl-root/mnt/boot-state/nmbl status --json
```

After repairing the host, use the same mounted paths with
`retry-generation --generation N --no-reboot`, then reboot the machine using the
rescue reboot tool. `--no-reboot` records the request without calling systemd.
`--profiles-dir PATH` can override the installed profiles directory. Explicit
paths must be canonical, root-owned, and protected against other users writing
any ancestor. The default installed system root above is `/mnt/system`, and the state twin
is `/mnt/boot-state`; adapt it to the host's
configured system root. No state counters or success flags should be edited.
