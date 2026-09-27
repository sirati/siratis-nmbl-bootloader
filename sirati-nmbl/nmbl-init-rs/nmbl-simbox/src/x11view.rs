//! X11 window for the simulated DRM framebuffer. It polls the card memfd,
//! shows each new frame with `PutImage`, and turns key presses into the byte
//! sequences a VT keyboard would send, written into the console pty NMBL's
//! splash reads (`/dev/tty1` resolves to it). The window closes when the run
//! ends (kexec or reboot).

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, KeyButMask,
    PropMode, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

/// Translate an X keysym (+ modifiers) into the bytes a Linux VT sends.
pub fn keysym_bytes(keysym: u32, state: KeyButMask) -> Option<Vec<u8>> {
    let ctrl = state.contains(KeyButMask::CONTROL);
    let seq: &[u8] = match keysym {
        0xff0d => b"\r",
        0xff1b => b"\x1b",
        0xff08 => b"\x7f",
        0xff09 => b"\t",
        0xff51 => b"\x1b[D",
        0xff52 => b"\x1b[A",
        0xff53 => b"\x1b[C",
        0xff54 => b"\x1b[B",
        0xff50 => b"\x1b[1~",
        0xff57 => b"\x1b[4~",
        0xff55 => b"\x1b[5~",
        0xff56 => b"\x1b[6~",
        0xffff => b"\x1b[3~",
        k @ 0x20..=0x7e => {
            let c = u8::try_from(k).ok()?;
            if ctrl && c.is_ascii_alphabetic() {
                return Some(vec![c.to_ascii_lowercase() - b'a' + 1]);
            }
            return Some(vec![c]);
        }
        _ => return None,
    };
    Some(seq.to_vec())
}

pub struct Viewer {
    pub width: u32,
    pub height: u32,
    pub memfd: OwnedFd,
    pub frames: Arc<AtomicU64>,
    pub keys_out: std::fs::File,
    pub stop: Arc<AtomicBool>,
}

impl Viewer {
    pub fn run(mut self) -> Result<(), String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("X11: {e}"))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or("no X11 screen")?
            .clone();
        let win = conn.generate_id().map_err(|e| e.to_string())?;
        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        let (w16, h16) = (
            u16::try_from(self.width).unwrap_or(u16::MAX),
            u16::try_from(self.height).unwrap_or(u16::MAX),
        );
        conn.create_window(
            24,
            win,
            screen.root,
            0,
            0,
            w16,
            h16,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(screen.black_pixel)
                .event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS),
        )
        .map_err(|e| e.to_string())?;
        conn.create_gc(gc, win, &CreateGCAux::new())
            .map_err(|e| e.to_string())?;
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"nmbl-simbox framebuffer",
        )
        .map_err(|e| e.to_string())?;
        conn.map_window(win).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        let setup = conn.setup();
        let min = setup.min_keycode;
        let mapping = conn
            .get_keyboard_mapping(min, setup.max_keycode - min + 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let per = usize::from(mapping.keysyms_per_keycode);
        let stride = self.width as usize * 4;
        let rows_per = (conn.maximum_request_bytes().saturating_sub(64) / stride).max(1);
        let mut shown = u64::MAX;
        let mut buf = vec![0u8; stride * self.height as usize];
        while !self.stop.load(Ordering::Relaxed) {
            let frame = self.frames.load(Ordering::Acquire);
            let mut expose = false;
            while let Some(ev) = conn.poll_for_event().map_err(|e| e.to_string())? {
                match ev {
                    Event::KeyPress(k) => {
                        let idx = usize::from(k.detail.saturating_sub(min)) * per;
                        let shift = usize::from(k.state.contains(KeyButMask::SHIFT));
                        let sym = mapping
                            .keysyms
                            .get(idx + shift)
                            .copied()
                            .filter(|s| *s != 0)
                            .or_else(|| mapping.keysyms.get(idx).copied())
                            .unwrap_or(0);
                        if let Some(bytes) = keysym_bytes(sym, k.state) {
                            let _ = self.keys_out.write_all(&bytes);
                        }
                    }
                    Event::Expose(_) => expose = true,
                    _ => {}
                }
            }
            if frame != shown || expose {
                shown = frame;
                // SAFETY: pread from our memfd into an owned buffer.
                unsafe {
                    use std::os::fd::AsRawFd;
                    libc::pread(
                        self.memfd.as_raw_fd(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        0,
                    );
                }
                let mut y = 0usize;
                while y < self.height as usize {
                    let n = rows_per.min(self.height as usize - y);
                    let strip = buf.get(y * stride..(y + n) * stride).unwrap_or_default();
                    conn.put_image(
                        ImageFormat::Z_PIXMAP,
                        win,
                        gc,
                        w16,
                        u16::try_from(n).unwrap_or(1),
                        0,
                        i16::try_from(y).unwrap_or(0),
                        0,
                        24,
                        strip,
                    )
                    .map_err(|e| e.to_string())?;
                    y += n;
                }
                // Expose progress in the title: scripts can wait for the
                // first NMBL frame before typing (as a person would).
                let title = format!("nmbl-simbox framebuffer [frame {frame}]");
                conn.change_property8(
                    PropMode::REPLACE,
                    win,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    title.as_bytes(),
                )
                .map_err(|e| e.to_string())?;
                conn.flush().map_err(|e| e.to_string())?;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let _ = conn.destroy_window(win);
        let _ = conn.flush();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn translates_keys_to_vt_bytes() {
        let none = KeyButMask::default();
        assert_eq!(keysym_bytes(0xff0d, none), Some(b"\r".to_vec()));
        assert_eq!(keysym_bytes(0xff54, none), Some(b"\x1b[B".to_vec()));
        assert_eq!(keysym_bytes(u32::from(b'x'), none), Some(b"x".to_vec()));
        assert_eq!(
            keysym_bytes(u32::from(b'l'), KeyButMask::CONTROL),
            Some(vec![12])
        );
    }
}
