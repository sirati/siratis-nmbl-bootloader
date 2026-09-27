//! `nmbl-ui-preview` — show NMBL's boot UI in an X11 window, driven by mock
//! scenarios, without booting anything.
//!
//! Rendering is NMBL's own code, unchanged: `nmbl_init::ui::render_app` draws
//! the ratatui views and `nmbl_init::ui::composite_frame` turns them into the
//! same XRGB8888 pixels the DRM splash would flip. Only the output surface (an
//! X11 window instead of a DRM dumb buffer) and the backend (mock state
//! instead of real devices) differ.
//!
//! Keys: Tab / Shift+Tab switch scenario, arrows/Enter/letters go to NMBL's
//! own key handler, F5 resets the scenario, q/Esc on the window quits.
//! `--scenario NAME` picks the start scenario, `--list` lists them, and
//! `--dump-ppm DIR` renders every scenario to PPM files headless (no X11).

mod scenarios;
mod x11;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nmbl_init::splash::glyph_cache;
use nmbl_init::splash::types::{CellDims, FramebufferDims};

use scenarios::{ALL, Scenario};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FONT_PX: f32 = 16.0;

struct Args {
    scenario: Scenario,
    dump: Option<PathBuf>,
    list: bool,
    width: u32,
    height: u32,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        scenario: Scenario::Selector,
        dump: None,
        list: false,
        width: WIDTH,
        height: HEIGHT,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--scenario" => {
                let n = it.next().ok_or("--scenario needs a name")?;
                args.scenario = Scenario::from_name(&n).ok_or(format!("unknown scenario {n}"))?;
            }
            "--dump-ppm" => {
                args.dump = Some(PathBuf::from(it.next().ok_or("--dump-ppm needs a dir")?))
            }
            "--size" => {
                let s = it.next().ok_or("--size needs WxH")?;
                let (w, h) = s.split_once('x').ok_or("--size is WxH")?;
                args.width = w.parse().map_err(|_| "bad width")?;
                args.height = h.parse().map_err(|_| "bad height")?;
            }
            "--list" => args.list = true,
            "-h" | "--help" => {
                println!(
                    "nmbl-ui-preview [--scenario NAME] [--size WxH] [--dump-ppm DIR] [--list]\n\
                     keys: Tab/Shift+Tab scenario, F5 reset, q quit; others go to NMBL"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

/// Everything needed to rasterise frames: the glyph cache, grid geometry and a
/// flat background (the preview does not need the wallpaper PNG).
pub struct Renderer {
    cache: glyph_cache::GlyphCache,
    pub dims: FramebufferDims,
    cell: CellDims,
    background: Vec<u8>,
}

impl Renderer {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let cache = glyph_cache::load_embedded_fallback(FONT_PX).map_err(|e| e.to_string())?;
        let size = cache.cell_size();
        let cell = CellDims {
            cols: u16::try_from(width / size.w.max(1)).unwrap_or(u16::MAX),
            rows: u16::try_from(height / size.h.max(1)).unwrap_or(u16::MAX),
            cell_w: size.w,
            cell_h: size.h,
        };
        let dims = FramebufferDims {
            w: width,
            h: height,
            stride: width * 4,
        };
        // Dark slate background, RGBA8 as the compositor expects.
        let background = [0x1c, 0x22, 0x2b, 0xff].repeat((width * height) as usize);
        Ok(Self {
            cache,
            dims,
            cell,
            background,
        })
    }

    /// Render `app` into a BGRX framebuffer (DRM/X11 byte order).
    pub fn frame(&self, app: &nmbl_init::ui::App<'_>) -> Result<Vec<u8>, String> {
        let mut fb = vec![0u8; (self.dims.stride * self.dims.h) as usize];
        nmbl_init::ui::composite_frame(
            &mut fb,
            self.dims,
            &self.background,
            &self.cache,
            self.cell,
            &mut |f| nmbl_init::ui::render_app(f, app),
        )
        .map_err(|e| e.to_string())?;
        Ok(fb)
    }
}

/// Write a BGRX framebuffer as a binary PPM.
fn write_ppm(path: &Path, fb: &[u8], dims: FramebufferDims) -> std::io::Result<()> {
    let mut out = format!("P6\n{} {}\n255\n", dims.w, dims.h).into_bytes();
    for px in fb.chunks_exact(4) {
        if let [b, g, r, _] = px {
            out.extend_from_slice(&[*r, *g, *b]);
        }
    }
    std::fs::write(path, out)
}

fn main() -> ExitCode {
    // Keep the marker referenced so it is present in this binary (and only
    // here); the flake check asserts production binaries lack it.
    std::hint::black_box(scenarios::PREVIEW_MARKER);
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("nmbl-ui-preview: {e}");
            return ExitCode::from(2);
        }
    };
    if args.list {
        for s in ALL {
            println!("{}", s.name());
        }
        return ExitCode::SUCCESS;
    }
    let renderer = match Renderer::new(args.width, args.height) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nmbl-ui-preview: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match &args.dump {
        Some(dir) => dump_all(&renderer, dir),
        None => x11::run(&renderer, args.scenario),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nmbl-ui-preview: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Headless mode: render every scenario to `DIR/<name>.ppm`.
fn dump_all(renderer: &Renderer, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let gens = scenarios::fake_generations();
    for s in ALL {
        let app = scenarios::build(s, &gens, 0);
        let fb = renderer.frame(&app)?;
        let path = dir.join(format!("{}.ppm", s.name()));
        write_ppm(&path, &fb, renderer.dims).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("{}", path.display());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_rasterises_non_blank() {
        let r = Renderer::new(640, 360).unwrap_or_else(|e| panic!("{e}"));
        let gens = scenarios::fake_generations();
        for s in ALL {
            let fb = r
                .frame(&scenarios::build(s, &gens, 0))
                .unwrap_or_else(|e| panic!("{e}"));
            // Text must have been drawn: some pixel differs from the background.
            let bg = [0x2b, 0x22, 0x1c];
            let drawn = fb.chunks_exact(4).any(|p| p.get(..3) != Some(&bg[..]));
            assert!(drawn, "{} rendered nothing", s.name());
        }
    }
}
