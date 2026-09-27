//! X11 output surface for the preview: a window that shows the framebuffer via
//! core-protocol `PutImage`, and translates X key presses into the crossterm
//! `KeyEvent`s NMBL's `App::on_key` consumes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, KeyButMask, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::Renderer;
use crate::scenarios::{self, ALL, Scenario};

// X keysyms we translate (from X11/keysymdef.h).
const XK_TAB: u32 = 0xff09;
const XK_ISO_LEFT_TAB: u32 = 0xfe20;
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_BACKSPACE: u32 = 0xff08;
const XK_LEFT: u32 = 0xff51;
const XK_UP: u32 = 0xff52;
const XK_RIGHT: u32 = 0xff53;
const XK_DOWN: u32 = 0xff54;
const XK_PAGE_UP: u32 = 0xff55;
const XK_PAGE_DOWN: u32 = 0xff56;
const XK_HOME: u32 = 0xff50;
const XK_END: u32 = 0xff57;
const XK_F5: u32 = 0xffc2;

/// What a key press means to the preview shell.
enum Action {
    Quit,
    NextScenario,
    PrevScenario,
    Reset,
    Key(KeyEvent),
    None,
}

fn translate(keysym: u32, state: KeyButMask) -> Action {
    let shift = state.contains(KeyButMask::SHIFT);
    let mut mods = KeyModifiers::NONE;
    if shift {
        mods |= KeyModifiers::SHIFT;
    }
    if state.contains(KeyButMask::CONTROL) {
        mods |= KeyModifiers::CONTROL;
    }
    let code = match keysym {
        XK_ISO_LEFT_TAB => return Action::PrevScenario,
        XK_TAB if shift => return Action::PrevScenario,
        XK_TAB => return Action::NextScenario,
        XK_F5 => return Action::Reset,
        XK_RETURN => KeyCode::Enter,
        XK_ESCAPE => KeyCode::Esc,
        XK_BACKSPACE => KeyCode::Backspace,
        XK_LEFT => KeyCode::Left,
        XK_RIGHT => KeyCode::Right,
        XK_UP => KeyCode::Up,
        XK_DOWN => KeyCode::Down,
        XK_PAGE_UP => KeyCode::PageUp,
        XK_PAGE_DOWN => KeyCode::PageDown,
        XK_HOME => KeyCode::Home,
        XK_END => KeyCode::End,
        // Latin-1 keysyms equal their character code.
        k @ 0x20..=0x7e => match char::from_u32(k) {
            Some('q') if mods.is_empty() => return Action::Quit,
            Some(c) => KeyCode::Char(c),
            None => return Action::None,
        },
        _ => return Action::None,
    };
    Action::Key(KeyEvent::new(code, mods))
}

pub fn run(renderer: &Renderer, start: Scenario) -> Result<(), String> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| format!("cannot connect to X11 ($DISPLAY): {e}"))?;
    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or("X11 screen missing")?
        .clone();
    if screen.root_depth != 24 {
        return Err(format!("unsupported X11 root depth {}", screen.root_depth));
    }
    let (w, h) = (renderer.dims.w, renderer.dims.h);
    let win = conn.generate_id().map_err(|e| e.to_string())?;
    let gc = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        24,
        win,
        screen.root,
        0,
        0,
        u16::try_from(w).unwrap_or(u16::MAX),
        u16::try_from(h).unwrap_or(u16::MAX),
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY),
    )
    .map_err(|e| e.to_string())?;
    conn.create_gc(gc, win, &CreateGCAux::new())
        .map_err(|e| e.to_string())?;
    let title = b"NMBL UI preview";
    conn.change_property8(
        x11rb::protocol::xproto::PropMode::REPLACE,
        win,
        x11rb::protocol::xproto::AtomEnum::WM_NAME,
        x11rb::protocol::xproto::AtomEnum::STRING,
        title,
    )
    .map_err(|e| e.to_string())?;
    conn.map_window(win).map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;

    // Keyboard mapping, to turn keycodes into keysyms.
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let mapping = conn
        .get_keyboard_mapping(min, max - min + 1)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let per = usize::from(mapping.keysyms_per_keycode);

    let gens = scenarios::fake_generations();
    let mut idx = ALL.iter().position(|s| *s == start).unwrap_or(0);
    let mut tick: u8 = 0;
    let scenario_at = |i: usize| ALL.get(i).copied().unwrap_or(Scenario::Selector);
    let mut app = scenarios::build(scenario_at(idx), &gens, tick);
    let max_req = conn.maximum_request_bytes();

    loop {
        let fb = renderer.frame(&app)?;
        // PutImage in horizontal strips so each request fits the server limit.
        let stride = renderer.dims.stride as usize;
        let rows_per = (max_req.saturating_sub(64) / stride).clamp(1, h as usize);
        let mut y = 0usize;
        while y < h as usize {
            let n = rows_per.min(h as usize - y);
            let strip = fb
                .get(y * stride..(y + n) * stride)
                .ok_or("strip out of range")?;
            conn.put_image(
                ImageFormat::Z_PIXMAP,
                win,
                gc,
                u16::try_from(w).unwrap_or(u16::MAX),
                u16::try_from(n).unwrap_or(u16::MAX),
                0,
                i16::try_from(y).unwrap_or(i16::MAX),
                0,
                24,
                strip,
            )
            .map_err(|e| e.to_string())?;
            y += n;
        }
        conn.flush().map_err(|e| e.to_string())?;

        // Wait for input; animate spinners at ~10 Hz when idle.
        let event = match conn.poll_for_event().map_err(|e| e.to_string())? {
            Some(ev) => Some(ev),
            None => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                tick = tick.wrapping_add(1);
                None
            }
        };
        match event {
            None => {
                // Refresh animated scenarios in place without losing key state.
                if let nmbl_init::ui::Screen::BootStatus(d) = &mut app.screen {
                    d.spinner_frame = tick;
                }
                if let nmbl_init::ui::Screen::Passphrase { spinner_frame, .. } = &mut app.screen {
                    *spinner_frame = tick;
                }
            }
            Some(Event::KeyPress(k)) => {
                let base = usize::from(k.detail.saturating_sub(min)) * per;
                let keysym = mapping.keysyms.get(base).copied().unwrap_or(0);
                match translate(keysym, k.state) {
                    Action::Quit => return Ok(()),
                    Action::NextScenario => {
                        idx = (idx + 1) % ALL.len();
                        app = scenarios::build(scenario_at(idx), &gens, tick);
                    }
                    Action::PrevScenario => {
                        idx = (idx + ALL.len() - 1) % ALL.len();
                        app = scenarios::build(scenario_at(idx), &gens, tick);
                    }
                    Action::Reset => app = scenarios::build(scenario_at(idx), &gens, tick),
                    Action::Key(key) => {
                        app.on_key(key);
                        if app.decision.is_some() {
                            eprintln!("[preview] decision: {:?}", app.decision);
                            app = scenarios::build(scenario_at(idx), &gens, tick);
                        }
                    }
                    Action::None => {}
                }
                eprintln!("[preview] scenario: {}", scenario_at(idx).name());
            }
            Some(Event::DestroyNotify(_)) => return Ok(()),
            Some(_) => {}
        }
    }
}
