//! Port and process ownership checks.

use std::path::{Path, PathBuf};

use crate::ctx::Ctx;
use crate::systemd;
use crate::util::{Result, random_bytes};

pub fn pid_in_unit(pid: u32, unit: &str) -> bool {
    let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
        return false;
    };
    let tail = format!("/{unit}");
    let mid = format!("/{unit}/");
    cgroup.lines().any(|l| l.ends_with(&tail) || l.contains(&mid))
}

/// PIDs listening on `port`, from `ss -Hltnp` (as the Python does).
pub fn listener_pids(ctx: &Ctx, port: u16) -> Vec<u32> {
    let out = systemd::run(ctx, &["ss", "-Hltnp", &format!("sport = :{port}")], None, Some(15.0), false)
        .unwrap_or_default();
    let mut pids = Vec::new();
    for part in out.split("pid=").skip(1) {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = digits.parse() {
            pids.push(p);
        }
    }
    pids
}

pub fn listener_in_unit(ctx: &Ctx, port: u16, unit: &str) -> bool {
    listener_pids(ctx, port).into_iter().any(|pid| ctx.sys.pid_in_unit(pid, &systemd::unit_name(unit)))
}

pub fn port_owner(ctx: &Ctx, port: u16) -> String {
    let out = systemd::run(ctx, &["ss", "-Hltnp", &format!("sport = :{port}")], None, Some(15.0), false)
        .unwrap_or_default();
    if out.is_empty() { "unknown process".into() } else { out }
}

/// Bind (no listen) 127.0.0.1 and ::1 with SO_REUSEADDR, like Chromium's
/// listener: TIME_WAIT left by CDP clients doesn't count, a listener does.
pub fn port_free(port: u16) -> bool {
    port_free_with(port, try_bind)
}

/// `bind(v6)` returns the errno of socket()/bind(), or Ok.
pub fn port_free_with(port: u16, bind: impl Fn(u16, bool) -> std::result::Result<(), i32>) -> bool {
    for v6 in [false, true] {
        if let Err(errno) = bind(port, v6) {
            // No IPv6 (disabled at boot, or on lo): nothing can listen there.
            if v6 && (errno == libc::EAFNOSUPPORT || errno == libc::EADDRNOTAVAIL) {
                continue;
            }
            return false;
        }
    }
    true
}

fn try_bind(port: u16, v6: bool) -> std::result::Result<(), i32> {
    let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    let family = if v6 { libc::AF_INET6 } else { libc::AF_INET };
    // SAFETY: plain socket calls on a fd we own and close.
    unsafe {
        let fd = libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(errno());
        }
        let one: libc::c_int = 1;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&one as *const libc::c_int).cast(), 4);
        let rc = if v6 {
            let mut a: libc::sockaddr_in6 = std::mem::zeroed();
            a.sin6_family = libc::AF_INET6 as u16;
            a.sin6_port = port.to_be();
            a.sin6_addr.s6_addr[15] = 1;
            libc::bind(fd, (&a as *const libc::sockaddr_in6).cast(), std::mem::size_of::<libc::sockaddr_in6>() as u32)
        } else {
            let mut a: libc::sockaddr_in = std::mem::zeroed();
            a.sin_family = libc::AF_INET as u16;
            a.sin_port = port.to_be();
            a.sin_addr.s_addr = u32::from_be_bytes([127, 0, 0, 1]).to_be();
            libc::bind(fd, (&a as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32)
        };
        let e = errno();
        libc::close(fd);
        if rc != 0 { Err(e) } else { Ok(()) }
    }
}

/// `tempfile.mkdtemp(prefix=..., dir=...)`: mode 0700, random suffix.
pub fn mkdtemp(prefix: &str, dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut last = None;
    for _ in 0..100 {
        let name: String = random_bytes(8).iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect();
        let path = dir.join(format!("{prefix}{name}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last.map(Into::into).unwrap_or_else(|| crate::util::Error::new("mkdtemp failed")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn listener_pids_parse_ss_output() {
        let line = r#"LISTEN 0 10 127.0.0.1:9425 0.0.0.0:* users:(("chromium",pid=4242,fd=88),("x",pid=7,fd=1))"#;
        let pids: Vec<u32> = line.split("pid=").skip(1)
            .filter_map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()).collect();
        assert_eq!(pids, vec![4242, 7]);
    }
}
