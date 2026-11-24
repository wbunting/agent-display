//! Errors, environment snapshots, Python-compatible quoting/formatting,
//! small encoders and the subprocess runner.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The tool's one error type. `os` marks socket/OS failures, which `tab`
/// commands report as `CDP: ...` like the Python's `OSError` handler.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub msg: String,
    pub os: bool,
    /// Process exit status for this error (see `exit`).
    pub code: i32,
}

/// Exit statuses shared by the `tab` commands. Agents branch on these, so
/// they are a contract: never reuse a number for another meaning. Numbers
/// reserved for commands that don't exist yet are not documented to users
/// (README, `tab -h`) until those commands land.
pub mod exit {
    /// Any other failure (refusals, bad input, CDP errors).
    pub const ERROR: i32 = 1;
    /// Command line usage error (argparse).
    pub const USAGE: i32 = 2;
    /// A wait ran out of time (`tab show --timeout`).
    pub const TIMEOUT: i32 = 3;
    /// The tab is gone: closed or crashed, confirmed twice ~250 ms apart.
    pub const GONE: i32 = 4;
    /// The browser holding the tab is not running.
    pub const BROWSER_DOWN: i32 = 5;
    /// Reserved: read-back after an input action did not match. Not used
    /// by any command yet.
    pub const MISMATCH: i32 = 6;
    /// No definitive answer (browser stalled, lookup failed after internal
    /// retries): try again later. EX_TEMPFAIL.
    pub const TEMPFAIL: i32 = 75;
}

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error { msg: msg.into(), os: false, code: exit::ERROR }
    }
    pub fn os(msg: impl Into<String>) -> Self {
        Error { msg: msg.into(), os: true, code: exit::ERROR }
    }
    /// Same error with another exit status.
    pub fn with_code(mut self, code: i32) -> Self {
        self.code = code;
        self
    }
    pub fn is_timeout(&self) -> bool {
        self.msg.contains("timed out")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::os(os_error_text(&e))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::util::Error::new(format!($($t)*))) };
}

/// `[Errno N] text`, or `timed out`, as Python's OSError prints.
pub fn os_error_text(e: &io::Error) -> String {
    if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
        return "timed out".into();
    }
    match e.raw_os_error() {
        Some(code) => {
            let full = e.to_string();
            let text = full
                .strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&full);
            format!("[Errno {code}] {text}")
        }
        None => e.to_string(),
    }
}

/// Ordered environment (process order, like `os.environ`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environ(pub Vec<(String, String)>);

impl Environ {
    pub fn from_process() -> Self {
        Environ(
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        )
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    /// Value if set and non-empty (Python's `os.environ.get(k) or ...`).
    pub fn get_nonempty(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.is_empty())
    }
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    pub fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| k != key);
    }
    pub fn extend(&mut self, other: &Environ) {
        for (k, v) in &other.0 {
            self.set(k, v.clone());
        }
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

// ------------------------------------------------------------ quoting

/// Python's `shlex.quote`.
pub fn shquote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let safe = s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// Python's `shlex.join`.
pub fn shjoin<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|a| shquote(a.as_ref())).collect::<Vec<_>>().join(" ")
}

/// Python's `repr(str)` (close enough for messages).
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `repr(bytes)`.
pub fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c as char == quote => {
                out.push('\\');
                out.push(c as char);
            }
            0x20..=0x7e => out.push(c as char),
            c => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(quote);
    out
}

/// Python's `format(x, 'g')`.
pub fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return if x.is_nan() { "nan".into() } else if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{x:.5e}");
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if (-4..6).contains(&exp) {
        let prec = (5 - exp).max(0) as usize;
        let s = format!("{x:.prec$}");
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    } else {
        let mant = if mant.contains('.') { mant.trim_end_matches('0').trim_end_matches('.') } else { mant };
        format!("{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// `pathlib.Path(p).absolute()`: joins cwd, drops `.` and duplicate
/// slashes, keeps `..`.
pub fn absolute(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `os.path.realpath`-like resolve that tolerates missing tails
/// (`pathlib.Path.resolve()` in non-strict mode).
pub fn resolve(p: &Path) -> PathBuf {
    let abs = absolute(p);
    if let Ok(r) = std::fs::canonicalize(&abs) {
        return r;
    }
    // Resolve the longest existing prefix, then append the rest lexically.
    let comps: Vec<Component> = abs.components().collect();
    for i in (0..comps.len()).rev() {
        let prefix: PathBuf = comps[..i].iter().collect();
        if let Ok(mut base) = std::fs::canonicalize(&prefix) {
            for c in &comps[i..] {
                match c {
                    Component::ParentDir => {
                        base.pop();
                    }
                    Component::Normal(n) => base.push(n),
                    _ => {}
                }
            }
            return base;
        }
    }
    abs
}

// ------------------------------------------------------------ encoders

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let v = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= n {
                out.push(B64[(v >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 decode (Python's `b64decode`: padding required,
/// non-alphabet characters discarded).
pub fn b64decode(text: &str) -> Result<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits, mut pad) = (0u32, 0u32, 0usize);
    for &c in text.as_bytes() {
        if c == b'=' {
            pad += 1;
            continue;
        }
        let v = table[c as usize];
        if v == 255 {
            continue;
        }
        if pad > 0 {
            return Err(Error::new("Incorrect padding"));
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        // SAFETY: buf is valid for n - filled bytes from the offset.
        let r = unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), n - filled, 0) };
        if r > 0 {
            filled += r as usize;
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    buf
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------ files

/// Write a 0600 file (created or truncated), like the Python's write_private.
/// An exclusive `flock` on `path` (created 0600 if missing), held until
/// dropped. Blocks until it is free; retries on EINTR.
pub struct FileLock {
    _file: std::fs::File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<FileLock> {
        use std::os::fd::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)?;
        loop {
            // SAFETY: flock on a fd we own.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(FileLock { _file: f });
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }
}

pub fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

pub fn unlink_missing_ok(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).map(|m| m.file_type().is_socket()).unwrap_or(false)
}

/// `shutil.which` against `PATH` from `env`.
pub fn which(tool: &str, env: &Environ) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if tool.contains('/') {
        let p = PathBuf::from(tool);
        return p.is_file().then_some(p);
    }
    let path = env.get("PATH").unwrap_or("/usr/local/bin:/usr/bin:/bin");
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let cand = Path::new(dir).join(tool);
        if let Ok(m) = std::fs::metadata(&cand)
            && m.is_file()
            && m.permissions().mode() & 0o111 != 0
        {
            return Some(cand);
        }
    }
    None
}

pub fn getuid() -> u32 {
    // SAFETY: getuid has no failure modes.
    unsafe { libc::getuid() }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// RAII umask.
pub struct Umask(libc::mode_t);
impl Umask {
    pub fn set(mask: libc::mode_t) -> Self {
        // SAFETY: umask always succeeds.
        Umask(unsafe { libc::umask(mask) })
    }
}
impl Drop for Umask {
    fn drop(&mut self) {
        // SAFETY: restores the previous mask.
        unsafe { libc::umask(self.0) };
    }
}

// ------------------------------------------------------------ processes

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn pidfd_open(pid: u32) -> Option<i32> {
    // SAFETY: plain syscall; returns -1 on error.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    (fd >= 0).then_some(fd as i32)
}

/// Run argv with captured output, like `subprocess.run(capture_output=True,
/// text=True, timeout=...)`. `env` None inherits; Some replaces entirely.
pub fn spawn_capture(argv: &[String], env: Option<&Environ>, timeout: Option<f64>) -> Result<Output> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(env) = env {
        cmd.env_clear();
        for (k, v) in &env.0 {
            cmd.env(k, v);
        }
    }
    let mut child = cmd.spawn().map_err(|e| Error::new(python_spawn_error(&e, &argv[0])))?;
    let reader = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            b
        })
    };
    let out = reader(Box::new(child.stdout.take().unwrap()));
    let err = reader(Box::new(child.stderr.take().unwrap()));
    let status = match timeout {
        None => Some(child.wait()?),
        Some(t) => wait_deadline(&mut child, Instant::now() + Duration::from_secs_f64(t.max(0.0)))?,
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::new(format!(
            "Command '{}' timed out after {} seconds",
            py_list_repr(argv),
            fmt_seconds(timeout.unwrap_or(0.0))
        )));
    };
    let ob = out.join().unwrap_or_default();
    let eb = err.join().unwrap_or_default();
    Ok(Output { code: exit_code(status), stdout: lossy(&ob), stderr: lossy(&eb) })
}

fn fmt_seconds(t: f64) -> String {
    if t.fract() == 0.0 { format!("{}", t as i64) } else { format!("{t}") }
}

fn py_list_repr(argv: &[String]) -> String {
    format!("[{}]", argv.iter().map(|a| py_repr(a)).collect::<Vec<_>>().join(", "))
}

pub fn python_spawn_error(e: &io::Error, prog: &str) -> String {
    format!("{}: {}", os_error_text(e), py_repr(prog))
}

fn wait_deadline(child: &mut std::process::Child, deadline: Instant) -> Result<Option<std::process::ExitStatus>> {
    if let Some(fd) = pidfd_open(child.id()) {
        loop {
            if let Some(st) = child.try_wait()? {
                // SAFETY: fd is ours.
                unsafe { libc::close(fd) };
                return Ok(Some(st));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                unsafe { libc::close(fd) };
                return Ok(None);
            }
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let ms = left.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, ms.max(1)) };
        }
    }
    let mut sleep = Duration::from_micros(200);
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(Some(st));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(sleep);
        sleep = (sleep * 2).min(Duration::from_millis(10));
    }
}

pub fn exit_code(st: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match st.code() {
        Some(c) => c,
        None => -st.signal().unwrap_or(1),
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub fn os(s: &str) -> &OsStr {
    OsStr::from_bytes(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_matches_python() {
        assert_eq!(shquote(""), "''");
        assert_eq!(shquote("abc/-_.:=@%+,"), "abc/-_.:=@%+,");
        assert_eq!(shquote("a b"), "'a b'");
        assert_eq!(shquote("it's"), "'it'\"'\"'s'");
        assert_eq!(shjoin(&["/usr/bin/rm", "-rf", "--", "/var/tmp/x"]), "/usr/bin/rm -rf -- /var/tmp/x");
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\nb"), "'a\\nb'");
        assert_eq!(py_bytes_repr(b"HTTP/1.1 404"), "b'HTTP/1.1 404'");
    }

    #[test]
    fn g_format_matches_python() {
        for (x, s) in [(30.0, "30"), (2.5, "2.5"), (0.1, "0.1"), (1e6, "1e+06"), (123456.0, "123456"),
                       (1234567.0, "1.23457e+06"), (0.0001, "0.0001"), (0.00001, "1e-05")] {
            assert_eq!(fmt_g(x), s, "{x}");
        }
    }

    #[test]
    fn base64_round_trip() {
        for n in 0..20 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64decode(&b64encode(&data)).unwrap(), data);
        }
        assert_eq!(b64encode(b"\x89PNG fake"), "iVBORyBmYWtl");
    }

    #[test]
    fn absolute_keeps_dotdot_drops_dot() {
        assert_eq!(absolute(Path::new("/a/./b/../c//d")), PathBuf::from("/a/b/../c/d"));
    }
}
