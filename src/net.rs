//! Port and process ownership checks.
//!
//! `listener_in_unit` replaces the Python's `ss -Hltnp` scan (20-25 ms: it
//! walks every process's fds): find the LISTEN socket inodes for the port in
//! `/proc/net/tcp{,6}`, then look for an fd pointing at one of them in only
//! the processes of the unit's cgroup. Same property: the listening socket
//! belongs to a pid inside the unit.


use std::collections::HashSet;
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

/// Inodes of TCP sockets in LISTEN state on `port` (any local address).
pub fn listen_inodes(port: u16) -> HashSet<u64> {
    let mut out = HashSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(table) {
            out.extend(parse_listen_inodes(&text, port));
        }
    }
    out
}

pub fn parse_listen_inodes(table: &str, port: u16) -> Vec<u64> {
    let mut out = Vec::new();
    for line in table.lines().skip(1) {
        // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || f[3] != "0A" {
            continue;
        }
        let Some((_, p)) = f[1].rsplit_once(':') else { continue };
        if u16::from_str_radix(p, 16).ok() != Some(port) {
            continue;
        }
        if let Ok(inode) = f[9].parse::<u64>()
            && inode != 0
        {
            out.push(inode);
        }
    }
    out
}

/// Every pid in the cgroup directory and its descendants.
pub fn cgroup_pids(dir: &Path) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(text) = std::fs::read_to_string(d.join("cgroup.procs")) {
            pids.extend(text.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(e.path());
                }
            }
        }
    }
    pids
}

/// The first of `inodes` that `pid` holds an fd for.
pub fn pid_socket(pid: u32, inodes: &HashSet<u64>) -> Option<u64> {
    let rd = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    for e in rd.flatten() {
        if let Ok(target) = std::fs::read_link(e.path()) {
            let t = target.as_os_str().as_encoded_bytes();
            if let Some(rest) = t.strip_prefix(b"socket:[")
                && let Some(num) = rest.strip_suffix(b"]")
                && let Some(inode) = std::str::from_utf8(num).ok().and_then(|s| s.parse::<u64>().ok())
                && inodes.contains(&inode)
            {
                return Some(inode);
            }
        }
    }
    None
}

/// Start time of `pid` in clock ticks since boot (`/proc/PID/stat` field
/// 22). With the pid it names one process: pids get reused, start times
/// with them don't.
pub fn pid_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces and parens: split after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Who serves a browser's CDP port: the process inside the unit holding the
/// LISTEN socket, remembered so later commands can re-check it from /proc
/// alone (no systemctl, no cgroup walk).
#[derive(Debug, Clone, PartialEq)]
pub struct Owner {
    pub port: u16,
    pub pid: u32,
    pub start_time: u64,
    pub inode: u64,
}

impl Owner {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"port": self.port, "pid": self.pid, "start_time": self.start_time, "inode": self.inode})
    }
    pub fn from_json(v: &serde_json::Value) -> Option<Owner> {
        Some(Owner {
            port: u16::try_from(v.get("port")?.as_u64()?).ok()?,
            pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
            start_time: v.get("start_time")?.as_u64()?,
            inode: v.get("inode")?.as_u64()?,
        })
    }
}

/// Is the remembered owner still the one? All from /proc: same process
/// (pid + start time), still inside the unit's cgroup, and the LISTEN socket
/// on the port is still the same inode, held by that process.
pub fn owner_valid(o: &Owner, unit: &str) -> bool {
    o.inode != 0
        && pid_start_time(o.pid) == Some(o.start_time)
        && pid_in_unit(o.pid, unit)
        && listen_inodes(o.port).contains(&o.inode)
        && pid_socket(o.pid, &HashSet::from([o.inode])).is_some()
}

/// A cgroup path from systemd, as a directory under /sys/fs/cgroup.
pub fn cgroup_dir(cgroup: &str) -> Option<PathBuf> {
    if !cgroup.starts_with('/') || cgroup.split('/').any(|c| c == ".." || c == ".") {
        return None;
    }
    Some(PathBuf::from(format!("/sys/fs/cgroup{cgroup}")))
}

pub fn listener_in_unit(ctx: &Ctx, port: u16, unit: &str) -> bool {
    listener_owner(ctx, port, unit).is_some()
}

/// The process inside `unit` (systemd's cgroup for it) that holds the
/// LISTEN socket on `port`, or None.
pub fn listener_owner(ctx: &Ctx, port: u16, unit: &str) -> Option<Owner> {
    let inodes = listen_inodes(port);
    if inodes.is_empty() {
        return None;
    }
    let cg = ctx.sys.unit_cgroup(ctx, &systemd::unit_name(unit))?;
    let dir = cgroup_dir(&cg)?;
    let mut pids = cgroup_pids(&dir);
    // Chromium's main process holds the listener: look there first. Only
    // the order changes; every pid scanned still comes from cgroup.procs.
    let main = ctx.units.borrow().get(&systemd::unit_name(unit)).map(|u| u.main_pid);
    if let Some(i) = main.and_then(|m| pids.iter().position(|p| *p == m)) {
        pids.swap(0, i);
    }
    pids.into_iter().find_map(|pid| {
        let inode = pid_socket(pid, &inodes)?;
        Some(Owner { port, pid, start_time: pid_start_time(pid)?, inode })
    })
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
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:24D1 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:24D1 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 5555 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:24D2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 7777 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn finds_only_listen_sockets_on_the_port() {
        assert_eq!(parse_listen_inodes(TCP, 9425), vec![4242]);
        assert_eq!(parse_listen_inodes(TCP, 9426), vec![7777]);
        assert!(parse_listen_inodes(TCP, 80).is_empty());
    }

    #[test]
    fn real_listener_found_in_own_process_only() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let inodes = listen_inodes(port);
        assert_eq!(inodes.len(), 1);
        assert!(pid_socket(std::process::id(), &inodes).is_some());
        assert!(pid_socket(1, &inodes).is_none());
    }

    #[test]
    fn owner_is_rechecked_from_proc_alone() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let pid = std::process::id();
        let inode = *listen_inodes(port).iter().next().unwrap();
        // Our own cgroup's last component stands in for the browser unit.
        let cg = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let unit = cg.lines().find_map(|l| l.strip_prefix("0::")).unwrap().rsplit('/').next().unwrap().to_string();
        let start_time = pid_start_time(pid).unwrap();
        let owner = Owner { port, pid, start_time, inode };
        assert!(owner_valid(&owner, &unit));
        assert_eq!(Owner::from_json(&owner.to_json()), Some(owner.clone()));
        assert!(!owner_valid(&Owner { start_time: start_time + 1, ..owner.clone() }, &unit), "pid reused");
        assert!(!owner_valid(&owner, "agent-display-browser.service"), "pid outside the unit");
        assert!(!owner_valid(&Owner { inode: inode + 1, ..owner.clone() }, &unit), "another socket");
        drop(l);
        assert!(!owner_valid(&owner, &unit), "nothing listening any more");
    }

    #[test]
    fn start_time_survives_odd_process_names() {
        assert!(pid_start_time(std::process::id()).is_some());
        assert_eq!(pid_start_time(u32::MAX), None);
    }

    #[test]
    fn cgroup_paths_are_validated() {
        assert!(cgroup_dir("/user.slice/a.service").is_some());
        assert!(cgroup_dir("").is_none());
        assert!(cgroup_dir("relative").is_none());
        assert!(cgroup_dir("/a/../../etc").is_none());
    }

    #[test]
    fn own_cgroup_contains_self() {
        let cg = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let path = cg.lines().find_map(|l| l.strip_prefix("0::")).unwrap().to_string();
        let pids = cgroup_pids(&cgroup_dir(&path).unwrap());
        assert!(pids.contains(&std::process::id()));
    }
}
