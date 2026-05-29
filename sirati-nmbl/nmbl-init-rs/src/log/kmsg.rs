use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

use super::byte_ring::push_byte_ring;
use super::ring::push_ring;

/// `/dev/kmsg` accepts writes from userspace and routes the resulting
/// printk message to every registered kernel console (regardless of
/// the `console=` ordering, which only picks the `/dev/console` target
/// for stdin/stdout/stderr). Teeing every NMBL log line here means
/// kernel messages, NMBL phase info, and the emergency shell all land
/// on the serial log AND on the framebuffer.
///
/// The fd is opened lazily on first write and cached for the lifetime
/// of the process. Failures (missing kmsg, permission denied) are
/// swallowed silently — the eprintln! path still produces output.
static KMSG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Try to write a line to /dev/kmsg. Must not be called with the KMSG
/// mutex held by the caller.
///
/// This is also the single tee-point for the in-memory log ring used by
/// the BootStatus TUI screen: every line that goes to kmsg is also
/// pushed onto the ring (without the `<6>[nmbl] ` prefix — see
/// `push_ring`). Callers should keep emitting through this entry point
/// so on-screen logs stay in sync with the serial/kernel log.
pub fn emit_kmsg(line: &str) {
    push_ring(line);
    push_byte_ring(line);
    let Ok(mut guard) = KMSG.lock() else {
        return;
    };
    if guard.is_none() {
        let opened = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open("/dev/kmsg");
        if let Ok(f) = opened {
            *guard = Some(f);
        }
    }
    if let Some(file) = guard.as_mut() {
        // Kernel `/dev/kmsg` treats each write(2) as one record.
        // Format the entire line up-front and submit it in a single
        // write_all so we don't get "<6>[nmbl]" / message / "\n" as
        // three separate records.
        // Use printk level 6 (KERN_INFO). The kernel parses "<6>" at
        // the start of each line and routes the rest as the message.
        let buf = format!("<6>[nmbl] {line}\n");
        let _ = file.write_all(buf.as_bytes());
    }
}
