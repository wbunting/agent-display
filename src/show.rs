//! `tab show` / `tab unshow`: hand exactly one tab to a human.
//!
//! Every tab from `tab open` sits alone on its own headless output, so a
//! wayvnc capturing only that output (`--output=HEADLESS-N`) shows that one
//! window and nothing of other agents' tabs. It listens on a Unix socket in
//! the private runtime dir (no TCP port), exits when its last viewer
//! disconnects (`--exit-on-disconnect`) and after `--timeout` at the latest
//! (coreutils `timeout`), so a finished or abandoned handoff leaves nothing
//! listening. Its control socket reports viewers connecting and leaving
//! (`wayvncctl event-receive`), which `show` prints as JSON lines when it
//! stays in the foreground.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::cli::{ShowOpts, TabOpts};
use crate::ctx::*;
use crate::display::display;
use crate::systemd::journal_tail;
use crate::tab::{GONE_RECHECK, lookup, marked_outputs, output_windows, tab_alive, valid_tab_id};
use crate::util::{Error, Result, exit, is_socket, py_repr, unlink_missing_ok, which, write_private};
use crate::{bail, json};

/// How often a foreground `show` checks the tab (URL, still open).
const TAB_POLL: f64 = 0.5;
/// How long `show` waits for wayvnc's socket.
const START_TIMEOUT: f64 = 10.0;

/// The viewer unit for a tab output (`agent-display-vnc-headless-3`).
pub fn viewer_unit(output: &str) -> String {
    format!("{VNC_UNIT}-{}", output.to_ascii_lowercase())
}

pub fn viewer_socket(ctx: &Ctx, output: &str) -> PathBuf {
    ctx.vnc_dir().join(format!("{output}.sock"))
}

pub fn viewer_control(ctx: &Ctx, output: &str) -> PathBuf {
    ctx.vnc_dir().join(format!("{output}.ctl"))
}

/// Stop the viewer of `output`, if there is one. Cheap when there isn't:
/// only asks systemd when a socket of it exists.
pub fn stop_viewer(ctx: &Ctx, output: &str) -> bool {
    let (sock, ctl) = (viewer_socket(ctx, output), viewer_control(ctx, output));
    if !sock.exists() && !ctl.exists() {
        return false;
    }
    let unit = viewer_unit(output);
    let was = ctx.sys.unit_active(ctx, &unit);
    if was {
        let _ = ctx.sys.stop_units(ctx, &[unit]);
    }
    unlink_missing_ok(&sock);
    unlink_missing_ok(&ctl);
    was
}

// ------------------------------------------------------------ events

/// One step of a viewer event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum VncEvent {
    /// A wayvnc event: `{"method": "client-connected", "params": {...}}`.
    Event(Value),
    /// Nothing within the timeout.
    Idle,
    /// wayvnc is gone (the stream ended).
    Closed,
}

pub trait VncEvents {
    fn next(&mut self, timeout: f64) -> VncEvent;
}

/// `wayvncctl -S CTL -w -j event-receive`, read without blocking past the
/// timeout. Ends (Closed) when wayvnc exits.
struct WayvncEvents {
    child: Child,
    out: ChildStdout,
    buf: Vec<u8>,
    eof: bool,
}

pub fn vnc_events_real(ctx: &Ctx, ctl: &Path) -> Result<Box<dyn VncEvents>> {
    let tool = which("wayvncctl", &ctx.env).ok_or_else(|| Error::new("wayvncctl not found (installed with wayvnc)"))?;
    let mut child = Command::new(tool)
        .args(["-S", &ctl.to_string_lossy(), "-w", "-j", "event-receive"])
        .env_clear()
        .env("PATH", ctx.env.get("PATH").unwrap_or("/usr/bin"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let out = child.stdout.take().ok_or_else(|| Error::new("wayvncctl: no stdout"))?;
    Ok(Box::new(WayvncEvents { child, out, buf: Vec::new(), eof: false }))
}

/// The first complete JSON value in `buf`, consumed. Garbage up to the next
/// newline is dropped; an incomplete value stays for the next read.
pub fn take_json(buf: &mut Vec<u8>) -> Option<Value> {
    loop {
        let start = buf.iter().position(|b| !b.is_ascii_whitespace())?;
        buf.drain(..start);
        let mut it = serde_json::Deserializer::from_slice(buf).into_iter::<Value>();
        match it.next()? {
            Ok(v) => {
                let used = it.byte_offset();
                buf.drain(..used);
                return Some(v);
            }
            Err(e) if e.is_eof() => return None,
            Err(_) => match buf.iter().position(|b| *b == b'\n') {
                Some(nl) => drop(buf.drain(..=nl)),
                None => {
                    buf.clear();
                    return None;
                }
            },
        }
    }
}

impl VncEvents for WayvncEvents {
    fn next(&mut self, timeout: f64) -> VncEvent {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
        loop {
            if let Some(v) = take_json(&mut self.buf) {
                return VncEvent::Event(v);
            }
            if self.eof {
                return VncEvent::Closed;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd { fd: self.out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd for a pipe we own.
            let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if n == 0 {
                return VncEvent::Idle;
            }
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                self.eof = true;
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.out.read(&mut chunk) {
                Ok(0) | Err(_) => self.eof = true,
                Ok(k) => self.buf.extend_from_slice(&chunk[..k]),
            }
        }
    }
}

impl Drop for WayvncEvents {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------ commands

fn tool(ctx: &Ctx, name: &str) -> Result<String> {
    which(name, &ctx.env)
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| Error::new(format!("{name} not found ({}; needed by tab show)", install_hint(name))))
}

fn emit(ctx: &Ctx, pairs: Vec<(&str, Value)>) {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.into(), v);
    }
    ctx.println(&json::dumps(&Value::Object(m), true));
}

/// The output `tab open` gave TAB (from its marker), if any.
fn tab_output(ctx: &Ctx, tab: &str) -> Option<String> {
    marked_outputs(ctx).into_iter().find(|(_, t)| t == tab).map(|(o, _)| o)
}

pub fn cmd_tab_show(ctx: &Ctx, o: &TabOpts, tab: &str, s: &ShowOpts) -> Result<i32> {
    let until_url = match &s.until_url {
        Some(p) => Some(
            regex_lite::Regex::new(p)
                .map_err(|e| Error::new(format!("invalid --until-url {}: {e}", py_repr(p))).with_code(exit::USAGE))?,
        ),
        None => None,
    };
    if !(s.timeout > 0.0 && s.timeout.is_finite()) {
        return Err(Error::new("--timeout must be a positive number of seconds").with_code(exit::USAGE));
    }
    let foreground = s.wait_viewer || s.until_closed || until_url.is_some();
    let disp = display(ctx)?;
    let wayvnc = tool(ctx, "wayvnc")?;
    let timeout_bin = tool(ctx, "timeout")?;
    let mut found = lookup(ctx, tab, o.port_raw)?;
    let Some(output) = tab_output(ctx, tab) else {
        found.cdp.close();
        bail!("tab {tab} has no output of its own (opened before outputs existed, or sway couldn't add one); open a new tab with `tab open` and show that");
    };
    let windows = output_windows(ctx, &disp)?.get(&output).copied().unwrap_or(0);
    if windows == 0 {
        found.cdp.close();
        bail!("tab {tab}'s window is not on {output} right now; try again");
    }
    let unit = viewer_unit(&output);
    let (sock, ctl) = (viewer_socket(ctx, &output), viewer_control(ctx, &output));
    let already = ctx.sys.unit_active(ctx, &unit) && is_socket(&sock);
    if !already {
        start_viewer(ctx, &disp, &unit, &output, &sock, &ctl, s, &wayvnc, &timeout_bin)?;
    }
    let sock_s = sock.to_string_lossy().into_owned();
    emit(ctx, vec![
        ("shown", tab.into()),
        ("output", output.clone().into()),
        ("socket", sock_s.clone().into()),
        ("viewer_command", format!("vncviewer {sock_s}").into()),
        ("unit", unit.clone().into()),
        ("view_only", s.view_only.into()),
        ("timeout", json!(s.timeout)),
        ("already_shown", already.into()),
    ]);
    if !foreground {
        found.cdp.close();
        return Ok(0);
    }
    let events = match ctx.sys.vnc_events(ctx, &ctl) {
        Ok(e) => e,
        Err(e) => {
            found.cdp.close();
            return Err(e);
        }
    };
    let r = follow(ctx, tab, &output, &unit, s, until_url.as_ref(), events, found.cdp.as_mut());
    found.cdp.close();
    r
}

#[allow(clippy::too_many_arguments)]
fn start_viewer(
    ctx: &Ctx,
    disp: &Disp,
    unit: &str,
    output: &str,
    sock: &Path,
    ctl: &Path,
    s: &ShowOpts,
    wayvnc: &str,
    timeout_bin: &str,
) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(ctx.vnc_dir()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    unlink_missing_ok(sock);
    unlink_missing_ok(ctl);
    // Empty config: never the user's ~/.config/wayvnc (see `view`).
    let config = ctx.runtime_dir().join("wayvnc.config");
    write_private(&config, "")?;
    let mut argv = env_argv(&ctx.minimal_env(disp));
    argv.extend([
        timeout_bin.to_string(),
        "--kill-after=5".into(),
        crate::util::fmt_g(s.timeout),
        wayvnc.to_string(),
    ]);
    if s.view_only {
        argv.push("--disable-input".into());
    }
    argv.extend([
        format!("--config={}", config.display()),
        format!("--socket={}", ctl.display()),
        format!("--output={output}"),
        "--disable-resizing".into(),
        "--render-cursor".into(),
        "--exit-on-disconnect".into(),
        format!("--name=agent-display {output}"),
        "--unix-socket".into(),
        sock.to_string_lossy().into_owned(),
    ]);
    let post: Vec<String> =
        vec!["/usr/bin/rm".into(), "-f".into(), "--".into(), sock.to_string_lossy().into(), ctl.to_string_lossy().into()];
    ctx.sys.systemd_run(ctx, unit, &argv, true, Some(&post), Some(&format!("Agent display VNC for one tab ({output})")))?;
    let deadline = Instant::now() + Duration::from_secs_f64(START_TIMEOUT);
    for _ in 0..(START_TIMEOUT / 0.05) as usize {
        if is_socket(sock) {
            return Ok(());
        }
        if Instant::now() > deadline || !ctx.sys.unit_active(ctx, unit) {
            break;
        }
        ctx.sys.sleep(0.05);
    }
    let log = journal_tail(ctx, unit, 12);
    let _ = ctx.sys.stop_units(ctx, &[unit.to_string()]);
    unlink_missing_ok(sock);
    unlink_missing_ok(ctl);
    bail!("wayvnc did not start for {output}:\n{log}");
}

/// Foreground part of `show`: print viewer events until the view ends.
#[allow(clippy::too_many_arguments)]
fn follow(
    ctx: &Ctx,
    tab: &str,
    output: &str,
    unit: &str,
    s: &ShowOpts,
    until_url: Option<&regex_lite::Regex>,
    mut events: Box<dyn VncEvents>,
    cdp: &mut dyn crate::cdp::CdpConn,
) -> Result<i32> {
    let deadline = Instant::now() + Duration::from_secs_f64(s.timeout);
    let mut seen_viewer = false;
    let mut next_poll = Instant::now();
    let end = |reason: &str, code: i32| -> Result<i32> {
        stop_viewer(ctx, output);
        emit(ctx, vec![("unshown", tab.into()), ("reason", reason.into())]);
        Ok(code)
    };
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
        if left <= 0.0 {
            return end("timeout", exit::TIMEOUT);
        }
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + Duration::from_secs_f64(TAB_POLL);
            match cdp.call("Target.getTargetInfo", json!({"targetId": tab}), None, Some(5.0)) {
                Ok(r) => {
                    let url = r["targetInfo"]["url"].as_str().unwrap_or("");
                    // Never print the URL: after a sign-in it can hold codes.
                    if until_url.is_some_and(|re| re.is_match(url)) {
                        return end("url-matched", 0);
                    }
                }
                Err(e) if e.msg.contains("No target with given id") => {
                    ctx.sys.sleep(GONE_RECHECK);
                    if tab_alive(ctx, tab) == Some(false) {
                        return if s.until_closed { end("closed", 0) } else { end("tab-gone", exit::GONE) };
                    }
                }
                // A busy browser: keep showing, check again later.
                Err(_) => {}
            }
        }
        match events.next(left.min(TAB_POLL)) {
            VncEvent::Idle => {}
            VncEvent::Closed => {
                // wayvnc exited: its last viewer left (--exit-on-disconnect),
                // `tab unshow`, or its own timeout.
                let gone = !ctx.sys.unit_active(ctx, unit);
                if gone || seen_viewer {
                    return end(if seen_viewer { "disconnected" } else { "stopped" }, 0);
                }
                // The event stream broke but wayvnc runs: keep the deadline.
                ctx.sys.sleep(TAB_POLL);
            }
            VncEvent::Event(ev) => {
                let clients = ev["params"]["connection_count"].clone();
                match ev["method"].as_str() {
                    Some("client-connected") => {
                        seen_viewer = true;
                        emit(ctx, vec![("viewer", "connected".into()), ("clients", clients)]);
                    }
                    Some("client-disconnected") => {
                        emit(ctx, vec![("viewer", "disconnected".into()), ("clients", clients.clone())]);
                        if clients.as_i64() == Some(0) {
                            return end("disconnected", 0);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

pub fn cmd_tab_unshow(ctx: &Ctx, tab: &str) -> Result<i32> {
    if !valid_tab_id(tab) {
        bail!("invalid tab id {}", py_repr(tab));
    }
    let output = tab_output(ctx, tab);
    let was = output.as_deref().is_some_and(|o| stop_viewer(ctx, o));
    emit(ctx, vec![
        ("unshown", tab.into()),
        ("output", output.map(Value::from).unwrap_or(Value::Null)),
        ("was_shown", was.into()),
    ]);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_stream_split_across_reads_and_garbage() {
        let mut b = br#"{"method":"client-connected","params":{"connection_count":1}}
{"method":"client-disc"#
            .to_vec();
        assert_eq!(take_json(&mut b).unwrap()["params"]["connection_count"], 1);
        assert_eq!(take_json(&mut b), None);
        b.extend_from_slice(b"onnected\",\"params\":{\"connection_count\":0}}\nnot json\n{\"method\":\"x\"}");
        assert_eq!(take_json(&mut b).unwrap()["method"], "client-disconnected");
        assert_eq!(take_json(&mut b).unwrap()["method"], "x");
        assert_eq!(take_json(&mut b), None);
    }

    #[test]
    fn viewer_names_are_per_output() {
        assert_eq!(viewer_unit("HEADLESS-12"), "agent-display-vnc-headless-12");
    }
}
