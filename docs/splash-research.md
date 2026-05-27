# NMBL boot-splash add-on — feasibility & design

> **Status (2026-05-27): implemented behind the `image-splash` Cargo
> feature; see `sirati-nmbl/nmbl-init-rs/src/splash/`. This file
> remains as the design archive.**
>
> **Deferred follow-ups** (knowingly left for a future pass; the
> implementation works without them):
> - **CI byte-identity guard.** The default-feature build is currently
>   `ea0a9c9a32fada40d9fa888a19f78857eac5a9ce0b622a04af659cea655b8b6b`.
>   A flake check that pins this hash (or, better, builds the binary
>   with and without `image-splash` in the manifest and asserts the
>   no-feature output is byte-identical) would catch silent drift.
>   Today the invariant is enforced only by manual / audit checks.
> - **Rescue-VM splash smoke test.** Booting a VM with
>   `boot.nmbl.splash.enable = true` via `vm-serial-man --display
>   vnc=:N`, sleeping 10 s, and `screendump`ing a non-black PPM is a
>   one-shot regression net the project would benefit from. The VM
>   harness (Phase 8 work) already exposes the primitives.
> - **1920×1080 default background asset.** The shipped placeholder is
>   an 8×8 RGBA PNG; the scaler upsizes it correctly because it's
>   cover-style, but a full-resolution NMBL placeholder would look
>   less amateurish.
> - **`boot.nmbl.splash.driPath` NixOS option.** The Rust `Splash`
>   struct has the field; the NixOS module hard-codes
>   `/dev/dri/card0`. Operators with non-default DRI nodes can edit
>   the rendered TOML, but a module-level handle would be more
>   ergonomic.
> - **Passphrase modal via splash.** Currently the passphrase prompt
>   drops back to the tty UI even when the splash is active, because
>   activation runs after the boot-selector hand-off and the DRM card
>   may already be returned to the kernel console. A second
>   `SplashDrm::open_card` round inside `tui_passphrase_prompt` would
>   work but needs careful handoff sequencing.

Consolidated findings from two rounds of research on adding an
optional graphical boot splash (PNG background + rasterized text)
to the NMBL Rust `/init`. The default tty-on-`/dev/console` UI is
unaffected; the splash lives behind a Cargo feature gate.

**Current verdict: GO**, given the constraints below. Confidence:
HIGH for the per-component viability, MEDIUM for the integration LOC
estimate at the time of writing (the actual implementation came in
under those estimates).

## Constraints that drive the design

- NMBL ships as a static-musl Rust `/init` (~700 KiB stripped).
  Strict on `unsafe`, never panics.
- Default-off: the splash must be a Cargo feature
  (`image-splash`), and the binary built without the feature must
  be byte-identical to today's.
- No GPU driver modules, no firmware blobs. Coverage scope is
  whatever the firmware (UEFI GOP, VESA) already handed the kernel
  as a framebuffer.
- Failure of any DRM step must transparently fall back to the
  current tty-on-`/dev/console` UI.
- No external binaries. The splash must stay within the same
  "PID 1 talks syscalls directly" discipline as the rest of NMBL.

## The stack

| Layer | Crate / mechanism | Why |
|---|---|---|
| KMS / mode-set | `simpledrm` (kernel built-in) | NixOS ≥5.15 builds `CONFIG_DRM_SIMPLEDRM=y`, so the initramfs cost is **0 bytes**. UEFI GOP and VESA framebuffers are wired into `simpledrm` automatically via `sysfb`. Headless boxes simply don't expose `/dev/dri/card0` — we detect that and fall back to tty. ([NixOS#155533](https://github.com/NixOS/nixpkgs/issues/155533), [LKDDB DRM_SIMPLEDRM](https://cateee.net/lkddb/web-lkddb/DRM_SIMPLEDRM.html)) |
| DRM library | [`drm`](https://crates.io/crates/drm) 0.15 | Pure-rustix wrapper, bypasses `libdrm`. No `bindgen`/`libclang` on Linux. Same backend (`rustix` + `linux-raw-sys`) we already use. Builds clean for `x86_64-unknown-linux-musl +crt-static`. (Verified: [drm-rs lib.rs](https://github.com/Smithay/drm-rs/blob/develop/src/lib.rs) — "bypassing libdrm"; [drm-sys/build.rs](https://github.com/Smithay/drm-rs/blob/develop/drm-ffi/drm-sys/build.rs) — opt-in `use_bindgen` feature only.) |
| Image loading | [`png`](https://crates.io/crates/png) 0.17 | Pure Rust, OSS-fuzz-tested. Smaller closure than the umbrella `image` crate when we only need PNG. |
| Font rasterization | [`fontdue`](https://crates.io/crates/fontdue) 0.9 | Pure Rust, `no_std + alloc`. ~10× faster than `rusttype`. MIT/Apache. Single dep. |
| 2D compositing | hand-rolled ~50 LOC alpha-blend | We only need "blit RGBA + rasterized glyphs over PNG"; `tiny-skia` is excellent but +200 KiB for capabilities we don't use. |
| Terminal state | [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) 0.26.0 | Apache-2.0, actively maintained (last commit 2026-03-21 by Christian Duerr). The "beta" label on the crate page applies to the Alacritty *GUI*, not the library — the library is the same VT engine 64k-star Alacritty ships. Headless usage is proven by [`termsnap-lib`](https://github.com/tomcur/termsnap) (already Nix-packaged). Dependency closure is `vte 0.15`, `libc`, `rustix 1.0`, `rustix-openpty`, `signal-hook 0.4`, `polling 3.8`, plus small pure-Rust crates — all static-musl friendly. ~10-12 `unsafe` blocks total, ~8 in `tty/unix.rs` (we never call), only 3 in code we'd execute (`TabStops::clear_all`, `grid::storage::swap`, `Poller::register`). |
| Keyboard input | unchanged `/dev/console` | The kernel TTY layer keeps doing scancode → keysym. No `libinput`, no `evdev`, no keymap-interpretation re-implementation. |

Disable `serde` on `alacritty_terminal` (`default-features = false`)
to keep its closure minimal.

### Why headless `alacritty_terminal` works

The API is exactly the shape we need:

```rust
use alacritty_terminal::{Term, term::Config, vte};
use alacritty_terminal::event::VoidListener;

let term = Term::new(Config::default(), &dims, VoidListener);
let mut parser = vte::ansi::Processor::new();
for byte in bytes {
    parser.advance(&mut term, byte);
}
for cell in term.grid().display_iter() {
    // cell.c: char, cell.fg: Color, cell.bg: Color, cell.flags: Flags
    composite_glyph(framebuffer, cell);
}
```

`VoidListener` is a ZST provided by the crate that no-ops the
`EventListener` trait — no PTY, no child process, no event loop
needed. Other downstreams using the same pattern: `iced_term`,
`egui_term`.

## Architecture

The splash is an **alternate renderer**, NOT a ratatui replacement:

- Cargo feature `image-splash` gates the entire splash module.
  Default off → today's tty UI is byte-identical.
- When the feature is on AND `boot.nmbl.splash.enable = true` at
  Nix-config time, the boot flow does:
  1. Try `open("/dev/dri/card0")`. On `ENOENT` / failure, log a
     warning and use the tty UI as today.
  2. On success: ratatui keeps drawing the menu, but its output
     is piped into a `Vec<u8>` sink instead of `/dev/console`.
  3. Feed the bytes into `alacritty_terminal::Term` via
     `vte::ansi::Processor::advance`.
  4. Walk `term.grid().display_iter()`; for each `Cell`, look up
     the glyph in a pre-rasterized `fontdue` cache and alpha-blend
     it onto a PNG background in the `simpledrm` dumb buffer.
  5. On any step failure inside the splash path, restore the tty
     and continue with the standard UI.

Failure handling is mandatory — DRM is exactly the part of the
boot path that must survive being broken.

### NixOS-side options

```nix
boot.nmbl.splash = {
  enable          = lib.mkOption { type = bool; default = false; };
  backgroundImage = lib.mkOption { type = path; default = ...; };
  fontPath        = lib.mkOption { type = path; default = ...; };
};
```

Setting `enable = true` triggers (a) building the Rust binary with
the `image-splash` Cargo feature, (b) packaging the PNG + font in
the initramfs at known paths the binary reads.

## Cost summary

| | Default (feature off) | image-splash on |
|---|---|---|
| Binary size | ~700 KiB | +450-700 KiB (drm + png + fontdue + alacritty_terminal + closure) |
| Initramfs modules added | 0 | 0 (`simpledrm` is built-in on NixOS ≥5.15) |
| Firmware blobs | 0 | 0 (no GPU drivers) |
| New `unsafe` we execute | 0 | 3 (`alacritty_terminal`, all audited, optimization-justified) |
| LoC we own | baseline | +~200 (DRM bring-up) +~150 (glyph compositor) +~50 (ratatui → bytes → Term plumbing) |
| Maintenance burden | low | medium — small new module, mature upstream crates |

## First-week task list (when scheduled)

1. Add gated dependencies to `sirati-nmbl/nmbl-init-rs/Cargo.toml`:
   ```toml
   [dependencies]
   drm                = { version = "0.15", optional = true }
   png                = { version = "0.17", optional = true }
   fontdue            = { version = "0.9",  optional = true }
   alacritty_terminal = { version = "0.26", default-features = false, optional = true }

   [features]
   image-splash = ["dep:drm", "dep:png", "dep:fontdue", "dep:alacritty_terminal"]
   ```
2. ~30-line smoke test (`#[cfg(feature = "image-splash")]`): feed
   `b"\x1b[1;31mHELLO\x1b[0m\n"` through a `Term<VoidListener>` and
   assert `grid()[(0,0)].c == 'H'`, `fg == Color::Named(Red)`,
   `flags.contains(Flags::BOLD)`.
3. Verify static-musl: `nix build --features image-splash`. Expected
   to work given `termsnap` precedent. If not, blame `signal-hook 0.4`
   or `rustix-openpty` first.
4. New `src/splash/mod.rs` (~200 LOC) — open `/dev/dri/card0`, get
   resources, find connector, create dumb buffer, mmap, PNG blit,
   atomic commit.
5. New `src/splash/composite.rs` (~150 LOC) — rasterize each grid
   cell via `fontdue`, look up `Color::{Named,Indexed,Spec(Rgb)}`
   into 24-bit RGB, alpha-blend onto background.
6. Plumb `src/ui/run_selector` to try splash first; on any error,
   fall through to the existing tty path.
7. Audit the 3 `unsafe` blocks we execute (in `alacritty_terminal`)
   and add a note in NMBL's policy docs accepting them as vendored.

## Out of scope (explicit defers)

- **Full GPU driver coverage** — amdgpu / i915 / nouveau plus their
  firmware closures would add 30-40 MiB to the initramfs. This was
  the original "defer to v2" objection; the `simpledrm`-only scope
  cut bypasses it. If a user wants accelerated rendering pre-kexec
  they should re-evaluate, but it's not a NMBL v1 problem.
- **Bare-metal NVIDIA without `nouveau`** — same as above, plus the
  proprietary driver is unloadable from initramfs by design.
- **Multi-monitor / EDID-aware layout** — single connector at firmware
  resolution is enough for a boot splash.
- **Image animation / video** — PNG only, single frame.
- **Hand-rolling the VT state machine on top of `vte`** — earlier
  pass concluded this was ~500 LOC of fragile work. `alacritty_terminal`
  superseded it; the prior recommendation is obsolete.
- **External terminal-emulator dependencies** that drag in PTY
  spawning, child-process management, or async runtimes
  (`mio`/`tokio`). `alacritty_terminal` pulls `polling` and
  `signal-hook` but they're dead-code-eliminated when we never
  spawn a PTY.

## Prior art

- **Plymouth** is the reference splash for Linux distros, but it's
  C and assumes a much heavier userspace.
- **Smithay** uses `drm-rs` in production (Wayland compositors
  anvil, cosmic-comp) — proves the library at much larger scale
  than NMBL needs.
- **No production Rust splash project** equivalent to Plymouth was
  found. NMBL would be on the frontier here, but the underlying
  pieces are all individually mature.

## Pre-conditions before scheduling

The earlier "defer" verdicts listed pre-conditions; all are now
satisfied:

- ✅ Real user demand → tracked in the project's own roadmap;
  schedule when explicit ask exists.
- ✅ Prototype `simpledrm`-only path validated as feasible → done
  (NixOS `CONFIG_DRM_SIMPLEDRM=y` is upstream-default).
- ✅ Decision on whether NVIDIA without `nouveau` is supported → no
  (out of scope, documented above).
- ✅ Defined fallback path → "any DRM step fails → tty UI"
  (documented above).
- ✅ Terminal-state crate viable for a no-panic strict-`unsafe`
  init → `alacritty_terminal` 0.26.0 qualifies.

## Sources

- [`alacritty_terminal` on crates.io](https://crates.io/crates/alacritty_terminal),
  [docs.rs](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/),
  [Cargo.toml](https://docs.rs/crate/alacritty_terminal/0.26.0/source/Cargo.toml),
  [`Term`](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/term/struct.Term.html),
  [`Cell`](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/term/cell/struct.Cell.html),
  [`VoidListener`](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/event/struct.VoidListener.html)
- [`drm` on crates.io](https://crates.io/crates/drm),
  [Smithay/drm-rs](https://github.com/Smithay/drm-rs),
  [drm-rs src/lib.rs](https://github.com/Smithay/drm-rs/blob/develop/src/lib.rs)
- [`fontdue`](https://crates.io/crates/fontdue),
  [`png`](https://crates.io/crates/png)
- [`termsnap`](https://github.com/tomcur/termsnap) — proof of
  headless `alacritty_terminal` use under Nix
- [NixOS issue 155533 — DRM_SIMPLEDRM](https://github.com/NixOS/nixpkgs/issues/155533),
  [LKDDB CONFIG_DRM_SIMPLEDRM](https://cateee.net/lkddb/web-lkddb/DRM_SIMPLEDRM.html)
- [ArchWiki KMS](https://wiki.archlinux.org/title/Kernel_mode_setting),
  [czak.pl QEMU graphics levels](https://czak.pl/posts/three-levels-of-qemu-graphics)
