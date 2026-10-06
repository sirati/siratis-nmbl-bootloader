# nmblctl and instant boot

`nmblctl` is the system-side control and inspection tool. The NMBL module adds
it to `environment.systemPackages` whenever `boot.nmbl.enable` is set. Every
subcommand except `--help` requires root.

```console
nmblctl [--color=auto|always|never] [--no-pager] <command>
```

| Command | Effect |
|---|---|
| `chain` | The configured boot chain: firmware, loader, config location, signing (algorithm, enforce, sidecar suffix, baked key fingerprints), generation mode and list, rescue, TPM and secure boot, stateful settings, operator selections. |
| `status` | The current boot: how it was selected, NMBL-set cmdline parameters, rescue sentinel, automatic rescue, signed-generation state (tested, pending, attempted), stateful `state.bin`, the units that block the success mark, and the decoded handover from NMBL. |
| `status --json` | Machine-readable recovery state from `state.bin`: counters, known-good generations, `max_recovery_attempts`, whether automatic recovery is exhausted, a pending retry, installed and active generations, signing flags. |
| `reboot-rescue [--yes]` | Asks for confirmation (skipped with `--yes`), writes the rescue sentinel durably, then runs `systemctl reboot`. Fails when `rescue.mode = "none"`. |
| `reboot-into [--generation N]` | Boots a generation once on the next boot, then reboots. Without `--generation`, a list picker opens. |
| `default [--generation N \| --latest \| --show]` | Sets or shows the persistent default. `--latest` selects the newest generation at each boot. Without a flag, a picker opens. |
| `retry-generation --generation N` | Authorizes one attempt of an installed profile after automatic recovery is exhausted. See [Retry after automatic recovery exhaustion](#retry-after-automatic-recovery-exhaustion). |

`chain` and `status` are coloured and paged like git: `$PAGER`, else
`less -R`. Output goes straight to stdout when stdout is not a TTY, when
`less` is missing, or with `--no-pager`.

On the booted system `nmblctl` reads the first config it finds among
`/etc/nmbl/config.toml`, `/boot/nmbl/config.toml`,
`/boot/nmbl-generations/active/config.toml` and
`/persistent/nmbl-generations/active/config.toml`, and scans the profiles in
`/nix/var/nix/profiles`. The global options `--config`, `--system-root`,
`--profiles-dir` and `--state-dir` point it at a mounted installed system
instead (see [From the full-system rescue](#from-the-full-system-rescue)).

## Handover in `status`

The "Handover from NMBL" section of `status` uses the decoders in
`nmbl_init::handover`, the same ones `nmbl-simbox` uses:

- the NMBL boot log, which the log-import service writes to the journal under
  the `nmbl-init` tag. `status` counts its lines and lists up to ten warnings
  (`journalctl -b -t nmbl-init` shows all of it);
- LUKS keyfiles left in `/etc/nmbl-luks`. Stage 1 removes the `passToStage1`
  keyfiles after use, so any file found there is reported as left over, with
  its value masked.

## Remembered choice

`nmblctl` writes two files in the state directory. The state directory is
`/boot/nmbl` on profile hosts and the generation state root on signed EROFS
hosts.

- `boot-default` contains `latest` or `generation N`.
- `boot-once` contains `generation N`. NMBL uses it once and then deletes it,
  like `grub-reboot`.

The selector uses the one-shot first, then the persistent default, then the
active profile. NMBL ignores a file that names a generation that is no longer
installed. The resolved choice is the countdown default, and it also applies
when the selector is skipped (instant boot, or LUKS unlock with "Select NixOS
Generation" unchecked). The format lives in `nmbl_init::boot_selection`, which
both `nmblctl` and the boot-time reader use.

`nmblctl` writes each file through a temporary file, `fsync`, rename, a second
`fsync` of the renamed file and an `fsync` of the directory. The second file
sync keeps the new content on VFAT after a reset right after the rename. The
`checks.x86_64-linux.nmblctl-vfat-rename-durability` VM check runs
`nmblctl default --latest` on a VFAT disk, crashes the VM right after the
command, and checks the file content after each of twenty crashes.

### Signed EROFS generations

On signed EROFS hosts the `active` symlink selects the boot generation, and
NMBL verifies the target's image and config before mounting them. A plain
flag file must not be able to select a generation in its place. On these
hosts:

- `nmblctl reboot-into`, `nmblctl default --generation` and
  `nmblctl retry-generation` exit with an error. Use `nmbl-erofsctl activate`
  or `rollback`, which verify the target's signature before the atomic rename.
- The boot-time resolver ignores `boot-default`, `boot-once` and
  `retry-generation`.

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
(`boot.nmbl.stateful.enable` or signed generation images), instant boot does
not trigger.

NMBL starts listening for keys as soon as `/dev/console` exists. It opens an
`O_RDONLY|O_NONBLOCK` descriptor in non-canonical mode and polls it through the
bootstrap, module, mount and verification phases. Any byte sets a flag that
stays set after NMBL closes the descriptor for console bring-up. From then
on, the interactive console's latch records key presses. One key anywhere
before the selector cancels instant boot. NMBL reads input from
`/dev/console`, which is the primary console: the last `console=` argument
(the serial line when `boot.nmbl.serialConsole` is set).

`nix run .#test-instant-boot-vm` tests instant boot on the stateful BIOS and
GRUB VM host: an untouched healthy boot skips the menu, and a key typed on the
serial console before the selector shows the menu.

## UI preview (development only)

`nix run .#nmbl-ui-preview` opens NMBL's boot UI in an X11 window. The
separate `nmbl-ui-preview` crate drives it with mock state. Its scenarios are
`selector`, `selector-countdown`, `boot-status`, `luks`, `luks-verifying`,
`wrong-password`, `emergency`, `rescue-confirm` and `error`. It renders through
NMBL's own code: `ui::render_app` for the ratatui views and
`ui::composite_frame`, the pipeline the DRM splash uses. Only the output
(an X11 window) and the state source differ.

- Tab and Shift+Tab switch scenarios, F5 resets, q quits. Every other key
  goes to NMBL's own key handler.
- `--scenario NAME` sets the starting scenario. `--list` lists the names.
- `--size WxH` sets the window size.
- `--dump-ppm DIR` renders every scenario to PPM files without X11.

The mock code exists only in that crate, and no other crate depends on it.
The `nmbl-ui-preview-absent` check of the `nmbl-init-rs` flake greps every
production binary (nmbl-init in its default, splash and full-feature builds,
nmblctl, nmbl-sign and nmbl-boot-update) for the preview's marker string and
crate name. The check also requires the marker in the preview binary itself,
so an ineffective scan fails.

## Retry after automatic recovery exhaustion

`nmblctl retry-generation --generation N` authorizes one attempt of an
installed profile after you repair the cause of the failure. It requires
stateful recovery (`boot.nmbl.stateful.enable`) and a readable `state.bin`.
The command:

1. checks that profile N resolves to a closure in the installed Nix store;
2. verifies the generation's pinned signatures when signing is enabled;
3. writes `retry-generation` (`generation N`) durably to the state directory;
4. clears the rescue sentinel;
5. reboots, unless `--no-reboot` is given.

At boot NMBL removes the request durably before it records the attempt in
`state.bin`, so the request authorizes one boot. The normal signature check
still runs before kernel loading. Recovery counters and known-good history
stay unchanged. If the retried generation fails, the next boot returns to
automatic rescue. Only the normal successful-boot health policy rearms
automatic recovery. Do not edit the counters or success flags by hand.

On stateful hosts, reaching the rescue also authorizes one retry. Once the
rescue console reports ready, NMBL records the failed generation in
`state.bin`, and the next boot tries that generation once. A failure of that
retry also returns to rescue.

### From the full-system rescue

The full-system rescue provides `nmblctl` from its own pinned tools image,
mounted at `/nmbl-tools` (see
[rescue-stages.md](rescue-stages.md#the-tools-image)). If NMBL refuses that
image, the rescue has no `nmblctl`. NMBL's root is bind-mounted at
`/nmbl-root` in the rescue, so from the authenticated root shell you can
inspect the installed state NMBL mounted:

```console
nmblctl --config /nmbl-root/mnt/boot/nmbl/config.toml \
  --system-root /nmbl-root/mnt/system \
  --state-dir /nmbl-root/mnt/boot-state/nmbl status --json
```

After repairing the host, run `retry-generation --generation N --no-reboot`
with the same paths, then reboot the machine with the rescue's reboot tool.
`--no-reboot` records the request without calling systemd. `--profiles-dir
PATH` overrides the installed profiles directory, which defaults to
`<system-root>/nix/var/nix/profiles`.

Explicit paths must be absolute and canonical, and the path and every
ancestor must be owned by root and not writable by group or others. NMBL
sets these permissions on its initramfs root when it starts as PID 1.
The example uses NMBL's defaults: `boot.nmbl.paths.systemRoot = "/mnt/system"`
and `boot.nmbl.stateful.rwMountpoint = "/mnt/boot-state"`. Adapt them to the
host's configuration.

The `nmbl` command in the rescue attaches to NMBL's own TUI. See
[rescue-stages.md](rescue-stages.md).
