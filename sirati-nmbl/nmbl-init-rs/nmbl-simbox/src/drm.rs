//! A simulated DRM/KMS card: exactly the subset NMBL's splash uses, faithful
//! to the kernel ABI (`drm_mode_card_res`, `drm_mode_get_connector`, …).
//!
//! One connector (connected, one preferred mode = the scenario framebuffer
//! size), one encoder, one CRTC, dumb buffers backed by a memfd the supervisor
//! also maps. `open("/dev/dri/card0")` is answered with a memfd installed in
//! the task via `SECCOMP_IOCTL_NOTIF_ADDFD`; every ioctl on it is trapped (the
//! seccomp filter notifies on all `ioctl`s and the supervisor passes through
//! the ones not on the card fd). `DRM_IOCTL_MODE_MAP_DUMB` returns offset 0,
//! so NMBL's `mmap(card_fd, 0)` maps the memfd itself: the pixels it writes
//! land in shared memory the X11 viewer reads.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::seccomp::Reply;
use crate::task::Task;

const CONNECTOR_ID: u32 = 40;
const ENCODER_ID: u32 = 41;
const CRTC_ID: u32 = 42;
const FB_ID: u32 = 43;
const DUMB_HANDLE: u32 = 1;

// DRM ioctl numbers (_IOWR('d', nr, size)); the size field varies, so match
// on type 'd' and nr only.
const NR_VERSION: u64 = 0x00;
const NR_GET_CAP: u64 = 0x0c;
const NR_SET_CLIENT_CAP: u64 = 0x0d;
const NR_SET_MASTER: u64 = 0x1e;
const NR_DROP_MASTER: u64 = 0x1f;
const NR_GETRESOURCES: u64 = 0xA0;
const NR_GETCRTC: u64 = 0xA1;
const NR_SETCRTC: u64 = 0xA2;
const NR_GETENCODER: u64 = 0xA6;
const NR_GETCONNECTOR: u64 = 0xA7;
const NR_GETPROPERTY: u64 = 0xAA;
const NR_ADDFB: u64 = 0xAE;
const NR_RMFB: u64 = 0xAF;
const NR_DIRTYFB: u64 = 0xB1;
const NR_CREATE_DUMB: u64 = 0xB2;
const NR_MAP_DUMB: u64 = 0xB3;
const NR_DESTROY_DUMB: u64 = 0xB4;
const NR_OBJ_GETPROPERTIES: u64 = 0xB9;

pub struct Card {
    pub width: u32,
    pub height: u32,
    /// Shared framebuffer memory (XRGB8888, stride = width * 4).
    pub memfd: OwnedFd,
    /// Bumped on every SETCRTC/DIRTYFB: a new frame is ready.
    pub frames: u64,
}

impl Card {
    pub fn new(width: u32, height: u32) -> std::io::Result<Self> {
        // SAFETY: memfd_create with a static name; checked.
        let fd = unsafe { libc::memfd_create(c"nmbl-simbox-drm".as_ptr(), 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fresh fd.
        let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
        let len = u64::from(width) * u64::from(height) * 4;
        // SAFETY: ftruncate on our memfd.
        if unsafe { libc::ftruncate(memfd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            width,
            height,
            memfd,
            frames: 0,
        })
    }

    pub fn len(&self) -> usize {
        (self.width * self.height * 4) as usize
    }

    /// Handle an ioctl on the card fd. `None` if `request` is not a DRM ioctl.
    pub fn ioctl(&mut self, task: &Task, request: u64, arg: u64) -> Option<Reply> {
        if (request >> 8) & 0xff != u64::from(b'd') {
            return None;
        }
        let nr = request & 0xff;
        let r = match nr {
            NR_VERSION => self.version(task, arg),
            NR_GET_CAP => {
                // capability@0 u64, value@8 u64. DUMB_BUFFER=1 -> 1, others 0.
                let cap = read_u64(task, arg).unwrap_or(0);
                write(task, arg + 8, &u64::from(cap == 1).to_ne_bytes())
            }
            NR_SET_CLIENT_CAP | NR_SET_MASTER | NR_DROP_MASTER | NR_RMFB | NR_DESTROY_DUMB => {
                Reply::Value(0)
            }
            NR_GETRESOURCES => self.resources(task, arg),
            NR_GETCONNECTOR => self.connector(task, arg),
            NR_GETENCODER => {
                // encoder_id, encoder_type(DAC=1), crtc_id, possible_crtcs, possible_clones
                let mut b = Vec::new();
                for v in [ENCODER_ID, 1, CRTC_ID, 1, 0] {
                    b.extend_from_slice(&v.to_ne_bytes());
                }
                write(task, arg, &b)
            }
            NR_GETCRTC => {
                // set_connectors_ptr u64, count_connectors, crtc_id, fb_id, x, y,
                // gamma_size, mode_valid, mode(68 bytes).
                let mut b = vec![0u8; 8 + 4 * 7 + 68];
                put32(&mut b, 12, CRTC_ID);
                write(task, arg, &b)
            }
            NR_SETCRTC => {
                self.frames += 1;
                Reply::Value(0)
            }
            NR_DIRTYFB => {
                self.frames += 1;
                Reply::Value(0)
            }
            NR_ADDFB => write(task, arg, &FB_ID.to_ne_bytes()),
            #[allow(clippy::indexing_slicing, reason = "fixed 16-byte buffer")]
            NR_CREATE_DUMB => {
                // height, width, bpp, flags, handle@16, pitch@20, size@24 u64
                let mut b = [0u8; 16];
                put32(&mut b, 0, DUMB_HANDLE);
                put32(&mut b, 4, self.width * 4);
                b[8..16].copy_from_slice(&(self.len() as u64).to_ne_bytes());
                write(task, arg + 16, &b)
            }
            NR_MAP_DUMB => write(task, arg + 8, &0u64.to_ne_bytes()),
            NR_GETPROPERTY | NR_OBJ_GETPROPERTIES => Reply::Errno(libc::EINVAL),
            _ => Reply::Errno(libc::EINVAL),
        };
        Some(r)
    }

    fn version(&self, task: &Task, arg: u64) -> Reply {
        // major, minor, patch (3x i32, then pad), name_len@16, name@24,
        // date_len@32, date@40, desc_len@48, desc@56.
        let name = b"simbox";
        let name_ptr = read_u64(task, arg + 24).unwrap_or(0);
        let name_cap = read_u64(task, arg + 16).unwrap_or(0);
        if name_ptr != 0 && name_cap >= name.len() as u64 {
            let _ = task.write_mem(name_ptr, name);
        }
        let mut b = [0u8; 12];
        put32(&mut b, 0, 1);
        let _ = write(task, arg, &b);
        let _ = write(task, arg + 16, &(name.len() as u64).to_ne_bytes());
        let _ = write(task, arg + 32, &0u64.to_ne_bytes());
        write(task, arg + 48, &0u64.to_ne_bytes())
    }

    fn resources(&self, task: &Task, arg: u64) -> Reply {
        // fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr (u64 each),
        // count_fbs@32, count_crtcs@36, count_connectors@40, count_encoders@44,
        // min_w, max_w, min_h, max_h.
        let ptrs: Vec<u64> = (0..4)
            .map(|i| read_u64(task, arg + i * 8).unwrap_or(0))
            .collect();
        let fill = |ptr: u64, cap_off: u64, id: u32| {
            let cap = read_u32(task, arg + cap_off).unwrap_or(0);
            if ptr != 0 && cap >= 1 {
                let _ = task.write_mem(ptr, &id.to_ne_bytes());
            }
        };
        fill(ptrs.get(1).copied().unwrap_or(0), 36, CRTC_ID);
        fill(ptrs.get(2).copied().unwrap_or(0), 40, CONNECTOR_ID);
        fill(ptrs.get(3).copied().unwrap_or(0), 44, ENCODER_ID);
        let mut b = [0u8; 32];
        for (i, v) in [0u32, 1, 1, 1, 1, self.width, 1, self.height]
            .iter()
            .enumerate()
        {
            put32(&mut b, i * 4, *v);
        }
        write(task, arg + 32, &b)
    }

    fn connector(&self, task: &Task, arg: u64) -> Reply {
        // encoders_ptr@0 modes_ptr@8 props_ptr@16 prop_values_ptr@24,
        // count_modes@32 count_props@36 count_encoders@40 encoder_id@44
        // connector_id@48 connector_type@52 type_id@56 connection@60
        // mm_width@64 mm_height@68 subpixel@72.
        let enc_ptr = read_u64(task, arg).unwrap_or(0);
        let modes_ptr = read_u64(task, arg + 8).unwrap_or(0);
        let modes_cap = read_u32(task, arg + 32).unwrap_or(0);
        let enc_cap = read_u32(task, arg + 40).unwrap_or(0);
        if modes_ptr != 0 && modes_cap >= 1 {
            let _ = task.write_mem(modes_ptr, &self.mode());
        }
        if enc_ptr != 0 && enc_cap >= 1 {
            let _ = task.write_mem(enc_ptr, &ENCODER_ID.to_ne_bytes());
        }
        let mut b = [0u8; 44];
        // count_modes, count_props, count_encoders, encoder_id, connector_id,
        // connector_type (Virtual=15), type_id, connection (1=connected),
        // mm_width, mm_height, subpixel.
        for (i, v) in [1u32, 0, 1, ENCODER_ID, CONNECTOR_ID, 15, 1, 1, 300, 170, 0]
            .iter()
            .enumerate()
        {
            put32(&mut b, i * 4, *v);
        }
        write(task, arg + 32, &b)
    }

    /// `struct drm_mode_modeinfo` (68 bytes) for the scenario resolution.
    #[allow(clippy::indexing_slicing, reason = "fixed 68-byte ABI struct")]
    fn mode(&self) -> Vec<u8> {
        let mut m = vec![0u8; 68];
        let (w, h) = (self.width as u16, self.height as u16);
        put32(&mut m, 0, 25_000); // clock kHz
        let hs = [w, w + 16, w + 32, w + 48, 0];
        for (i, v) in hs.iter().enumerate() {
            m[4 + i * 2..6 + i * 2].copy_from_slice(&v.to_ne_bytes());
        }
        let vs = [h, h + 3, h + 6, h + 9, 0];
        for (i, v) in vs.iter().enumerate() {
            m[14 + i * 2..16 + i * 2].copy_from_slice(&v.to_ne_bytes());
        }
        put32(&mut m, 24, 60); // vrefresh
        put32(&mut m, 32, 1 << 3); // type: DRM_MODE_TYPE_PREFERRED
        let name = format!("{}x{}", self.width, self.height);
        for (i, c) in name.bytes().take(31).enumerate() {
            m[36 + i] = c;
        }
        m
    }
}

#[allow(clippy::indexing_slicing, reason = "fixed-size local buffers")]
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn read_u64(task: &Task, addr: u64) -> Option<u64> {
    let b = task.read_mem(addr, 8).ok()?;
    Some(u64::from_ne_bytes(b.try_into().ok()?))
}

fn read_u32(task: &Task, addr: u64) -> Option<u32> {
    let b = task.read_mem(addr, 4).ok()?;
    Some(u32::from_ne_bytes(b.try_into().ok()?))
}

fn write(task: &Task, addr: u64, data: &[u8]) -> Reply {
    match task.write_mem(addr, data) {
        Ok(()) => Reply::Value(0),
        Err(_) => Reply::Errno(libc::EFAULT),
    }
}
