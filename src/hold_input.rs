//! `hold-input`: keep one zwlr_virtual_pointer_v1 and one
//! zwp_virtual_keyboard_v1 alive for the compositor's lifetime.
//!
//! A headless seat has no input devices. Without a pointer, clients never
//! bind wl_pointer and `seat seat0 cursor ...` reaches nobody; without a
//! keyboard, the seat only has one while a wtype process runs, so apps lose
//! keyboard focus between calls. The sway config runs this. Minimal Wayland
//! wire-protocol client (no wlrctl, no libwayland).

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::ctx::Ctx;
use crate::sway::{IPC_EVENT_INPUT, IPC_SUBSCRIBE, IpcReader, ipc_message};
use crate::util::{Error, Result, py_bytes_repr};

pub const VPOINTER: &str = "zwlr_virtual_pointer_manager_v1";
pub const VKEYBOARD: &str = "zwp_virtual_keyboard_manager_v1";
pub const KEYMAP: &[u8] = b"xkb_keymap { xkb_keycodes { include \"evdev+aliases(qwerty)\" }; \
xkb_types { include \"complete\" }; \
xkb_compat { include \"complete\" }; \
xkb_symbols { include \"pc+us+inet(evdev)\" }; };\0";

pub fn wl_string(text: &str) -> Vec<u8> {
    let mut data = text.as_bytes().to_vec();
    data.push(0);
    let len = data.len() as u32;
    let pad = (4 - data.len() % 4) % 4;
    let mut out = len.to_le_bytes().to_vec();
    out.extend(data);
    out.extend(std::iter::repeat_n(0, pad));
    out
}

pub fn wl_request(obj: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut m = obj.to_le_bytes().to_vec();
    m.extend((((8 + payload.len()) as u32) << 16 | opcode as u32).to_le_bytes());
    m.extend_from_slice(payload);
    m
}

fn u32s(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn wl_bind(name: u32, iface: &str, version: u32, new_id: u32) -> Vec<u8> {
    let mut p = name.to_le_bytes().to_vec();
    p.extend(wl_string(iface));
    p.extend(u32s(&[version, new_id]));
    wl_request(2, 0, &p)
}

/// Incremental wl_display event parser.
#[derive(Default)]
pub struct WlReader {
    pub buf: Vec<u8>,
}

impl WlReader {
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<(u32, u16, Vec<u8>)>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while self.buf.len() >= 8 {
            let obj = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
            let word = u32::from_le_bytes(self.buf[4..8].try_into().unwrap());
            let size = (word >> 16) as usize;
            if size < 8 {
                return Err(Error::new("wayland: malformed message"));
            }
            if self.buf.len() < size {
                break;
            }
            let body: Vec<u8> = self.buf.drain(..size).skip(8).collect();
            let op = (word & 0xffff) as u16;
            if obj == 1 && op == 0 {
                return Err(Error::new(format!("wayland error: {}", py_bytes_repr(&body))));
            }
            out.push((obj, op, body));
        }
        Ok(out)
    }
}

/// Make the held keyboard the seat's active keyboard again. wlroots ignores
/// a modifiers event that changes nothing, so blip Shift in the modifier
/// state and clear it (no key events are sent).
pub fn keyboard_reassert() -> Vec<u8> {
    let mut m = wl_request(8, 2, &u32s(&[1, 0, 0, 0]));
    m.extend(wl_request(8, 2, &u32s(&[0, 0, 0, 0])));
    m
}

fn read_some(s: &mut UnixStream, what: &str) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 65536];
    let n = s.read(&mut buf)?;
    if n == 0 {
        return Err(Error::new(what.to_string()));
    }
    buf.truncate(n);
    Ok(buf)
}

fn send_with_fd(sock: &UnixStream, msg: &[u8], fd: i32) -> Result<()> {
    // SAFETY: a single sendmsg with one iovec and one SCM_RIGHTS fd; all
    // buffers outlive the call.
    unsafe {
        let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut _, iov_len: msg.len() };
        let space = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cbuf.as_mut_ptr().cast();
        hdr.msg_controllen = space;
        let cmsg = libc::CMSG_FIRSTHDR(&hdr);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::copy_nonoverlapping((&fd as *const i32).cast::<u8>(), libc::CMSG_DATA(cmsg), 4);
        if libc::sendmsg(sock.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

pub fn hold_input(ctx: &Ctx, path: &Path, swaysock: &Path) -> Result<()> {
    let mut sock = UnixStream::connect(path)?;
    let mut wl = WlReader::default();
    // wl_display(1).get_registry -> 2; wl_display.sync -> 3
    let mut hello = wl_request(1, 1, &u32s(&[2]));
    hello.extend(wl_request(1, 0, &u32s(&[3])));
    sock.write_all(&hello)?;
    let mut globals: Vec<(String, u32)> = Vec::new();
    let mut synced = false;
    while !synced {
        let chunk = read_some(&mut sock, "agent display closed the Wayland connection")?;
        for (obj, op, body) in wl.feed(&chunk)? {
            if obj == 2 && op == 0 && body.len() >= 8 {
                // wl_registry.global(name, iface, version)
                let name = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let len = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
                let iface = String::from_utf8_lossy(&body[8..(8 + len.saturating_sub(1)).min(body.len())]).into_owned();
                if !globals.iter().any(|(i, _)| *i == iface) {
                    globals.push((iface, name));
                }
            } else if obj == 3 && op == 0 {
                synced = true; // wl_callback.done
            }
        }
    }
    let global = |iface: &str| globals.iter().find(|(i, _)| i == iface).map(|(_, n)| *n);
    let absent: Vec<&str> = [VPOINTER, VKEYBOARD, "wl_seat"].into_iter().filter(|g| global(g).is_none()).collect();
    if !absent.is_empty() {
        return Err(Error::new(format!("compositor lacks {}", absent.join(", "))));
    }
    // Pointer: bind manager -> 4, create_virtual_pointer(seat=null) -> 5.
    // Keyboard: bind wl_seat -> 6, manager -> 7, create_virtual_keyboard
    // (seat 6) -> 8, then keymap(xkb_v1, fd, size) with the fd attached.
    let mut setup = wl_bind(global(VPOINTER).unwrap(), VPOINTER, 1, 4);
    setup.extend(wl_request(4, 0, &u32s(&[0, 5])));
    setup.extend(wl_bind(global("wl_seat").unwrap(), "wl_seat", 1, 6));
    setup.extend(wl_bind(global(VKEYBOARD).unwrap(), VKEYBOARD, 1, 7));
    setup.extend(wl_request(7, 0, &u32s(&[6, 8])));
    sock.write_all(&setup)?;
    // SAFETY: memfd_create with a static NUL-terminated name.
    let fd = unsafe { libc::memfd_create(c"agent-display-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let sent = (|| {
        // SAFETY: fd is a fresh memfd we own; KEYMAP is valid for its length.
        let w = unsafe { libc::write(fd, KEYMAP.as_ptr().cast(), KEYMAP.len()) };
        if w != KEYMAP.len() as isize {
            return Err(std::io::Error::last_os_error().into());
        }
        send_with_fd(&sock, &wl_request(8, 0, &u32s(&[1, KEYMAP.len() as u32])), fd)
    })();
    // SAFETY: closing our own fd once sent.
    unsafe { libc::close(fd) };
    sent?;
    // Clients only get a keymap from the seat's *active* keyboard. A keyboard
    // that exists but is not active makes Chromium dereference a null xkb
    // state on enter (SIGSEGV). Become active now, and again whenever another
    // keyboard (wtype, wayvnc) goes away: wlroots then resets the active
    // keyboard to none.
    let mut first = keyboard_reassert();
    first.extend(wl_request(1, 0, &u32s(&[9])));
    sock.write_all(&first)?;
    let mut ipc = UnixStream::connect(swaysock)?;
    ipc.write_all(&ipc_message(IPC_SUBSCRIBE, br#"["input"]"#))?;
    let mut ipc_reader = IpcReader::default();
    loop {
        let mut fds = [
            libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: ipc.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if fds[0].revents != 0 {
            let chunk = read_some(&mut sock, "agent display closed the Wayland connection")?;
            for (obj, op, _) in wl.feed(&chunk)? {
                if obj == 9 && op == 0 {
                    ctx.eprintln("agent-display: virtual pointer and keyboard ready");
                }
            }
        }
        if fds[1].revents != 0 {
            let chunk = read_some(&mut ipc, "sway closed the IPC connection")?;
            for (kind, event) in ipc_reader.feed(&chunk)? {
                if kind == IPC_EVENT_INPUT && event.get("change").and_then(|c| c.as_str()) == Some("removed") {
                    sock.write_all(&keyboard_reassert())?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_padding() {
        assert_eq!(wl_string("abc"), [&4u32.to_le_bytes()[..], b"abc\0"].concat());
        assert_eq!(wl_string("abcd"), [&5u32.to_le_bytes()[..], b"abcd\0\0\0\0"].concat());
    }

    #[test]
    fn request_header() {
        let m = wl_request(7, 2, &[0; 8]);
        assert_eq!(u32::from_le_bytes(m[0..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(m[4..8].try_into().unwrap()), 16 << 16 | 2);
    }

    #[test]
    fn reader_splits_and_raises_on_display_error() {
        let mut r = WlReader::default();
        let a = wl_request(3, 0, b"\x01\0\0\0");
        let b = wl_request(2, 1, b"");
        let mut first = a.clone();
        first.extend(&b[..4]);
        assert_eq!(r.feed(&first).unwrap(), vec![(3, 0, b"\x01\0\0\0".to_vec())]);
        // The rest of (2,1) parses; a wl_display.error after it fails.
        let mut rest = b[4..].to_vec();
        assert_eq!(r.feed(&rest).unwrap(), vec![(2, 1, vec![])]);
        rest = wl_request(1, 0, b"xxxx");
        assert!(r.feed(&rest).unwrap_err().msg.starts_with("wayland error: b'xxxx'"));
    }

    #[test]
    fn reassert_changes_modifier_state_then_clears() {
        let m = keyboard_reassert();
        let words = |b: &[u8]| -> Vec<u32> { b.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect() };
        assert_eq!(&words(&m[..24])[2..], &[1, 0, 0, 0]);
        assert_eq!(&words(&m[24..])[2..], &[0, 0, 0, 0]);
    }

    #[test]
    fn keymap_fd_is_passed_with_the_message() {
        let (a, b) = UnixStream::pair().unwrap();
        // SAFETY: test-only memfd.
        let fd = unsafe { libc::memfd_create(c"t".as_ptr(), 0) };
        send_with_fd(&a, &wl_request(8, 0, &u32s(&[1, 3])), fd).unwrap();
        let mut buf = [0u8; 64];
        let mut cbuf = [0u8; 64];
        // SAFETY: recvmsg into local buffers.
        let got_fd = unsafe {
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
            let mut hdr: libc::msghdr = std::mem::zeroed();
            hdr.msg_iov = &mut iov;
            hdr.msg_iovlen = 1;
            hdr.msg_control = cbuf.as_mut_ptr().cast();
            hdr.msg_controllen = cbuf.len();
            assert_eq!(libc::recvmsg(b.as_raw_fd(), &mut hdr, 0), 16);
            let c = libc::CMSG_FIRSTHDR(&hdr);
            assert_eq!((*c).cmsg_type, libc::SCM_RIGHTS);
            let mut v = 0i32;
            std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c), (&mut v as *mut i32).cast(), 4);
            v
        };
        assert!(got_fd >= 0);
        unsafe {
            libc::close(fd);
            libc::close(got_fd);
        }
    }
}
