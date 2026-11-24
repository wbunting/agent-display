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
//!
//! `vnc/HEADLESS-N.json` records how the running viewer was started
//! (view-only, when it expires), so a second `show` reports the viewer that
//! actually runs and refuses to pretend a different mode. It also holds a
//! random token: a `show` stops a viewer only while that file still holds
//! the token it wrote, so it never stops a later viewer on the same output.
//! And whether a waiting `show` owns the viewer: one started by a `show`
//! that returned at once has no waiter, so the next waiting `show` takes it
//! over (and ends it when its condition is met); one a waiter owns is left
//! to that waiter, and the other caller's last line says it is still shown.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::cli::{ShowOpts, TabOpts};
use crate::ctx::*;
use crate::display::{Wayvnc, display, start_wayvnc, wayvnc_argv};
use crate::systemd::{is_running, stopping_or_stopped};
use crate::tab::{
    Found, GONE_RECHECK, LOOKUP_STEP_TIMEOUT, locate, lookup, no_such_target, output_windows, owned_ports,
    tab_alive, tab_output, valid_tab_id,
};
use crate::util::{Error, FileLock, Result, mkdir_private, exit, fmt_g, hex, is_socket, py_repr, random_bytes, unlink_missing_ok, which, write_private};
use crate::{bail, json};

/// How often a foreground `show` checks the tab (URL, still open).
const TAB_POLL: f64 = 0.5;
/// How long `show` waits for a tab's window to be back on its output:
/// WINDOW_WAIT_TRIES pauses of WINDOW_WAIT_STEP seconds (~2 s).
const WINDOW_WAIT_TRIES: usize = 40;
const WINDOW_WAIT_STEP: f64 = 0.05;
/// How long a view's end waits for an exiting viewer to be gone before
/// calling it still shown: SETTLE_TRIES pauses of SETTLE_STEP (~2 s).
const SETTLE_TRIES: usize = 20;
const SETTLE_STEP: f64 = 0.1;
/// While the event stream is broken but wayvnc seems to run: how often the
/// viewer unit's state is checked (backing off from TAB_POLL to this) and
/// the stream reopened (at most every STREAM_REOPEN seconds).
const STREAM_BACKOFF_MAX: f64 = 5.0;
const STREAM_REOPEN: f64 = 2.0;
/// A tab poll that keeps timing out is "browser busy" until it has failed
/// this long (seconds); then the tab is looked up afresh.
const STALL_RECHECK: f64 = 5.0;
/// wayvnc's own kill timer runs this much past the view's deadline, so a
/// foreground `show` always reaches its deadline (exit 3) first.
pub const VIEWER_GRACE: f64 = 2.0;

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

/// How the viewer of an output was started.
pub fn viewer_state(ctx: &Ctx, output: &str) -> PathBuf {
    ctx.vnc_dir().join(format!("{output}.json"))
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Create the 0700 vnc/ dir in the runtime dir.
fn ensure_vnc_dir(ctx: &Ctx) -> Result<()> {
    mkdir_private(&ctx.vnc_dir())
}

/// Exclusive per-output lock (`vnc/HEADLESS-N.lock`), held while a `show`
/// decides whether a viewer runs and starts one: two concurrent shows of
/// one tab must not both start (and unlink each other's socket and state).
struct ViewerLock {
    _lock: FileLock,
}

impl ViewerLock {
    fn acquire(ctx: &Ctx, output: &str) -> Result<ViewerLock> {
        ensure_vnc_dir(ctx)?;
        Ok(ViewerLock { _lock: FileLock::acquire(&ctx.vnc_dir().join(format!("{output}.lock")))? })
    }
}

fn state_json(ctx: &Ctx, output: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(viewer_state(ctx, output)).ok()?).ok()
}

/// What `vnc/HEADLESS-N.json` records about the running viewer.
struct ViewerState {
    view_only: bool,
    /// When it expires (unix seconds).
    expires: f64,
    token: String,
    /// A waiting `show` owns it (it will end the view).
    waiter: bool,
}

fn read_state(ctx: &Ctx, output: &str) -> Option<ViewerState> {
    let v = state_json(ctx, output)?;
    Some(ViewerState {
        view_only: v.get("view_only")?.as_bool()?,
        expires: v.get("expires")?.as_f64()?,
        token: v.get("token")?.as_str()?.to_string(),
        // Not recorded: assume owned, never take it over.
        waiter: v.get("waiter").and_then(Value::as_bool).unwrap_or(true),
    })
}

fn write_state(ctx: &Ctx, output: &str, st: &ViewerState) -> Result<()> {
    let record = json!({"view_only": st.view_only, "expires": st.expires, "token": st.token, "waiter": st.waiter});
    write_private(&viewer_state(ctx, output), &json::dumps(&record, true))
}

/// A viewer may be up on `output`: its socket exists and systemd doesn't
/// say it is stopping or stopped (an unknown state counts as up).
fn viewer_up(ctx: &Ctx, output: &str) -> bool {
    is_socket(&viewer_socket(ctx, output))
        && !ctx.sys.unit_active_state(ctx, &viewer_unit(output)).as_deref().is_some_and(stopping_or_stopped)
}

/// The token of the viewer currently recorded for `output`.
fn state_token(ctx: &Ctx, output: &str) -> Option<String> {
    state_json(ctx, output)?.get("token")?.as_str().map(String::from)
}

/// Which viewer `stop_viewer` may stop.
#[derive(Clone, Copy)]
pub enum Stop<'a> {
    /// Whatever viewer runs on the output; skipped (no systemctl call) when
    /// none of its files exist. For the output gc.
    Any,
    /// Same, but also asks systemd when the files are gone, so a viewer
    /// whose files were lost is still found. For `tab unshow`.
    Thorough,
    /// Only the viewer whose state file holds this token (`show`'s own).
    Token(&'a str),
}

/// Stop the viewer of `output` under its ViewerLock (so it can't interleave
/// with a `show` starting one). Ok(true) if one was (or may have been)
/// running and is now stopped.
///
/// An unknown unit state (systemctl failed or timed out) is not "stopped":
/// the unit is stopped unless confirmed inactive, and the files are only
/// removed once the stop succeeded or the unit is confirmed inactive. A
/// failed stop keeps them (and is an Err, exit 75) so `tab unshow` can retry.
pub fn stop_viewer(ctx: &Ctx, output: &str, which: Stop) -> Result<bool> {
    let files = [viewer_socket(ctx, output), viewer_control(ctx, output), viewer_state(ctx, output)];
    if matches!(which, Stop::Any | Stop::Token(_)) && !files.iter().any(|f| f.exists()) {
        return Ok(false);
    }
    let _lock = ViewerLock::acquire(ctx, output)?;
    if let Stop::Token(t) = which
        && state_token(ctx, output).as_deref() != Some(t)
    {
        return Ok(false); // not ours (any more)
    }
    let unit = viewer_unit(output);
    let state = crate::systemd::unit_state(ctx, &unit);
    if state != Some(false) {
        // Active, stopping, or unknown: stop it (a no-op if it isn't
        // running). Keep its files if that fails.
        if let Err(e) = ctx.sys.stop_units(ctx, std::slice::from_ref(&unit)) {
            return Err(Error::new(format!("could not stop the viewer {unit}: {}; try again", e.msg))
                .with_code(exit::TEMPFAIL));
        }
    }
    for f in &files {
        unlink_missing_ok(f);
    }
    Ok(state != Some(false))
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

/// `wayvncctl -S CTL [-w] -j event-receive`, read without blocking past the
/// timeout. Ends (Closed) when wayvnc exits; without -w (`wait` false) it
/// ends at once if wayvnc isn't there (~2 ms), so a reopened stream can't
/// hide an exit.
struct WayvncEvents {
    child: Child,
    out: ChildStdout,
    buf: Vec<u8>,
    eof: bool,
}

pub fn vnc_events_real(ctx: &Ctx, ctl: &Path, wait: bool) -> Result<Box<dyn VncEvents>> {
    let tool = which("wayvncctl", &ctx.env).ok_or_else(|| Error::new("wayvncctl not found (installed with wayvnc)"))?;
    let ctl = ctl.to_string_lossy().into_owned();
    let mut args = vec!["-S", ctl.as_str()];
    if wait {
        args.push("-w");
    }
    args.extend(["-j", "event-receive"]);
    let mut child = Command::new(tool)
        .args(&args)
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
    let timeout_bin = tool(ctx, "timeout")?;
    tool(ctx, "wayvnc")?;
    let found = lookup(ctx, tab, o.port_raw)?;
    let Some(output) = tab_output(ctx, tab) else {
        bail!("tab {tab} has no output of its own (opened before outputs existed, or sway couldn't add one); open a new tab with `tab open` and show that");
    };
    // The window can be briefly off its output (remapped: fullscreen, a
    // dialog); wait a little for it, then it's "try again" (75), not 1.
    let mut tries = 0;
    while output_windows(ctx, &disp)?.get(&output).copied().unwrap_or(0) == 0 {
        tries += 1;
        if tries > WINDOW_WAIT_TRIES {
            return Err(Error::new(format!("tab {tab}'s window is not on {output} right now; try again"))
                .with_code(exit::TEMPFAIL));
        }
        ctx.sys.sleep(WINDOW_WAIT_STEP);
    }
    let unit = viewer_unit(&output);
    let sock = viewer_socket(ctx, &output);
    // Released when the decision and any start are done (before following).
    let lock = ViewerLock::acquire(ctx, &output)?;
    // Unknown is not "not running": never start over (and unlink) a viewer
    // that may be someone else's. Stopping or stopped: start (start_wayvnc
    // waits out a stopping unit).
    let has_files = [&sock, &viewer_control(ctx, &output), &viewer_state(ctx, &output)].iter().any(|f| f.exists());
    let running = match ctx.sys.unit_active_state(ctx, &unit).as_deref() {
        Some(s) if is_running(s) && is_socket(&sock) => true,
        Some(s) if is_running(s) => {
            return Err(Error::new(format!(
                "a viewer of {output} is running without its socket; `agent-display tab unshow {tab}` first"
            ))
            .with_code(exit::TEMPFAIL));
        }
        Some(s) if stopping_or_stopped(s) => false,
        // Unknown (systemctl failed) or starting: fine only if nothing of a
        // viewer is here; systemd-run still refuses a loaded unit.
        _ if !has_files => false,
        state => {
            let why = state.map(|s| format!("it is {s}")).unwrap_or_else(|| "systemctl failed".into());
            return Err(Error::new(format!(
                "can't tell whether the viewer of {output} is running ({why}); try again"
            ))
            .with_code(exit::TEMPFAIL));
        }
    };
    // The token of the viewer this call will end: one it starts, or one
    // it takes over (None: another waiter owns the running viewer).
    let mut token: Option<String> = None;
    // When the view ends: ours from --timeout, or the running viewer's.
    let (view_only, expires) = if running {
        let Some(mut st) = read_state(ctx, &output) else {
            bail!("tab {tab} is already shown by a viewer with unknown options; `agent-display tab unshow {tab}` first");
        };
        if st.view_only != s.view_only {
            let (has, want) = if st.view_only { ("view-only", "interactive") } else { ("interactive", "view-only") };
            bail!("tab {tab} is already shown {has}; `agent-display tab unshow {tab}` first to show it {want}");
        }
        if foreground && !st.waiter {
            // Started by a `show` that returned at once: nobody else will
            // end it. Take it over (under the lock: only one waiter does).
            st.waiter = true;
            write_state(ctx, &output, &st)?;
            token = Some(st.token.clone());
        }
        (st.view_only, st.expires)
    } else {
        let expires = unix_now() + s.timeout; // before the launch: see VIEWER_GRACE
        let t = start_viewer(ctx, &disp, &unit, &output, s.view_only, expires, foreground, &timeout_bin)?;
        token = Some(t);
        (s.view_only, expires)
    };
    drop(lock);
    let left = (expires - unix_now()).max(0.0);
    let sock_s = sock.to_string_lossy().into_owned();
    emit(ctx, vec![
        ("shown", tab.into()),
        ("output", output.clone().into()),
        ("socket", sock_s.clone().into()),
        ("viewer_command", format!("vncviewer {sock_s}").into()),
        ("unit", unit.clone().into()),
        ("view_only", view_only.into()),
        ("timeout", json!((left * 10.0).round() / 10.0)),
        ("already_shown", running.into()),
    ]);
    if !foreground {
        return Ok(0);
    }
    let events = ctx.sys.vnc_events(ctx, &viewer_control(ctx, &output), true)?;
    // --wait-viewer alone follows the viewer, so its wait ends when the
    // viewer expires at the latest. --until-* watch the tab past the viewer
    // (`follow`), so on someone else's viewer they wait our own --timeout.
    // On ours, `left` is our --timeout measured from before the launch.
    let until = until_url.is_some() || s.until_closed;
    let wait = if until && running { s.timeout } else { left.min(s.timeout) };
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    let view = View { tab, output: &output, unit: &unit, token, deadline };
    follow(ctx, &view, s, until_url.as_ref(), events, found, o.port_raw)
}

/// Start a viewer for `output`; returns its token (see the module doc).
/// Call with the output's ViewerLock held.
#[allow(clippy::too_many_arguments)]
fn start_viewer(
    ctx: &Ctx,
    disp: &Disp,
    unit: &str,
    output: &str,
    view_only: bool,
    expires: f64,
    waiter: bool,
    timeout_bin: &str,
) -> Result<String> {
    ensure_vnc_dir(ctx)?;
    let (sock, ctl, state) = (viewer_socket(ctx, output), viewer_control(ctx, output), viewer_state(ctx, output));
    for f in [&sock, &ctl, &state] {
        unlink_missing_ok(f);
    }
    // wayvnc dies VIEWER_GRACE after the view's deadline at the latest.
    let kill_in = (expires - unix_now()).max(0.0) + VIEWER_GRACE;
    let prefix = [timeout_bin.to_string(), "--kill-after=5".into(), fmt_g(kill_in.ceil())];
    let argv = wayvnc_argv(ctx, disp, &prefix, &Wayvnc {
        output,
        ctl: &ctl,
        name: &format!("agent-display {output}"),
        view_only,
        exit_on_disconnect: true,
        listen: &["--unix-socket".into(), sock.to_string_lossy().into_owned()],
    })?;
    let mut post: Vec<String> = vec!["/usr/bin/rm".into(), "-f".into(), "--".into()];
    post.extend([&sock, &ctl, &state].iter().map(|p| p.to_string_lossy().into_owned()));
    let ready = || is_socket(&sock);
    if let Err(e) = start_wayvnc(ctx, unit, &argv, Some(&post), &format!("Agent display VNC for one tab ({output})"), &ready) {
        for f in [&sock, &ctl, &state] {
            unlink_missing_ok(f);
        }
        return Err(Error::new(format!("{output}: {}", e.msg)).with_code(e.code));
    }
    // Only now: if the previous viewer's unit was still stopping, its
    // ExecStopPost (`rm -f sock ctl state`) has finished (the new unit
    // couldn't start before), so it can't remove this. The ViewerLock keeps
    // every other `show` (the only reader of the options) out until then.
    let token = hex(&random_bytes(8));
    if let Err(e) = write_state(ctx, output, &ViewerState { view_only, expires, token: token.clone(), waiter }) {
        let _ = ctx.sys.stop_units(ctx, &[unit.to_string()]);
        return Err(e);
    }
    Ok(token)
}

/// The view a foreground `show` follows.
struct View<'a> {
    tab: &'a str,
    output: &'a str,
    unit: &'a str,
    /// Token of the viewer this invocation ends: one it started or took over
    /// (None: another waiter's). Once ours exits, a later `show` records a
    /// new token, so a waiter that outlives its viewer (--until-*) can't stop
    /// the next one.
    token: Option<String>,
    deadline: Instant,
}

impl View<'_> {
    /// Stop our viewer, if it is still the one recorded for the output. A
    /// failed stop is reported on stderr (the view's outcome is still
    /// printed); its files stay, so `tab unshow` can retry.
    fn stop_own(&self, ctx: &Ctx) {
        if let Some(t) = self.token.as_deref()
            && let Err(e) = stop_viewer(ctx, self.output, Stop::Token(t))
        {
            ctx.eprintln(&format!("agent-display: {}; stop it with `agent-display tab unshow {}`", e.msg, self.tab));
        }
    }
}

/// Foreground part of `show`: print viewer events until the view ends.
///
/// With --until-url / --until-closed the condition decides: exit 0 only
/// when it is met. The viewer may go first (the human disconnects, `tab
/// unshow`); the tab is then still watched until the condition, --timeout
/// (3) or the tab going away (4, or 5 if its whole browser went away),
/// because a sign-in often finishes after the human has left (phone
/// approval, an emailed link). Only --wait-viewer alone ends with the
/// viewer (0). --until-closed ends with "closed" (0) however the tab went.
///
/// The tab is polled on the connection `lookup` opened. A poll that fails
/// with a broken connection (browser exited or restarted) makes it look the
/// tab up afresh at once (`locate`: one scan, which also returns a fresh
/// connection); one that keeps timing out (a stalled browser) does after
/// STALL_RECHECK; further looks are at most every STALL_RECHECK. Gone ends
/// the view; found switches to the fresh connection. Every CDP step is
/// bounded by the view's deadline.
fn follow(
    ctx: &Ctx,
    v: &View,
    s: &ShowOpts,
    until_url: Option<&regex_lite::Regex>,
    mut events: Box<dyn VncEvents>,
    found: Found,
    port_arg: Option<i64>,
) -> Result<i32> {
    let Found { mut port, mut cdp, .. } = found;
    // While polls fail: when to look the tab up afresh (None while they
    // answer).
    let mut recheck_at: Option<Instant> = None;
    let tab = v.tab;
    let until = until_url.is_some() || s.until_closed;
    let mut seen_viewer = false;
    // The viewer has exited; only the tab is watched (until mode).
    let mut viewer_gone = false;
    let mut next_poll = Instant::now();
    // A broken event stream (wayvncctl ended while wayvnc may still run):
    // when to look at the unit again, how long to back off, when the
    // stream may be reopened.
    let mut next_check = Instant::now();
    let mut backoff = TAB_POLL;
    let mut reopen_at = Instant::now();
    // The view ends: stop our viewer, then say whether the tab is still
    // shown (someone else's viewer, or ours could not be stopped). After a
    // disconnect or stop the viewer is exiting: give it a moment first.
    let end = |reason: &str, code: i32| -> Result<i32> {
        v.stop_own(ctx);
        let mut up = viewer_up(ctx, v.output);
        if matches!(reason, "disconnected" | "stopped") {
            for _ in 0..SETTLE_TRIES {
                if !up {
                    break;
                }
                ctx.sys.sleep(SETTLE_STEP);
                up = viewer_up(ctx, v.output);
            }
        }
        if up {
            emit(ctx, vec![("done", tab.into()), ("reason", reason.into()), ("still_shown", true.into())]);
        } else {
            emit(ctx, vec![("unshown", tab.into()), ("reason", reason.into())]);
        }
        Ok(code)
    };
    // The tab is gone for good: why, and how this view ends.
    let gone = |port: u16| -> Result<i32> {
        let browser_down = !owned_ports(ctx).iter().any(|(p, _)| *p == port as i64);
        if s.until_closed {
            end("closed", 0)
        } else if browser_down {
            end("browser-down", exit::BROWSER_DOWN)
        } else {
            end("tab-gone", exit::GONE)
        }
    };
    // wayvnc's own timer ends VIEWER_GRACE after the deadline; an end
    // this close to the deadline is the timeout.
    let timed_out = || Instant::now() + Duration::from_secs_f64(VIEWER_GRACE / 2.0) >= v.deadline;
    loop {
        let left = v.deadline.saturating_duration_since(Instant::now()).as_secs_f64();
        if left <= 0.0 {
            return end("timeout", exit::TIMEOUT);
        }
        if viewer_gone || Instant::now() >= next_poll {
            next_poll = Instant::now() + Duration::from_secs_f64(TAB_POLL);
            // Bounded by the view's deadline, like every step below.
            let step = left.clamp(0.05, LOOKUP_STEP_TIMEOUT);
            match cdp.call("Target.getTargetInfo", json!({"targetId": tab}), None, Some(step)) {
                Ok(r) => {
                    recheck_at = None;
                    let url = r["targetInfo"]["url"].as_str().unwrap_or("");
                    // Never print the URL: after a sign-in it can hold codes.
                    if until_url.is_some_and(|re| re.is_match(url)) {
                        return end("url-matched", 0);
                    }
                }
                Err(e) if no_such_target(&e) => {
                    ctx.sys.sleep(GONE_RECHECK);
                    if tab_alive(ctx, tab) == Some(false) {
                        return gone(port);
                    }
                }
                Err(e) => {
                    // Look the tab up afresh: at once for a broken connection
                    // (it never gets better), after STALL_RECHECK of timeouts
                    // (a busy browser); then at most every STALL_RECHECK
                    // while no fresh look settles it.
                    let now = Instant::now();
                    let stall = Duration::from_secs_f64(STALL_RECHECK);
                    let due = *recheck_at.get_or_insert(if e.is_timeout() { now + stall } else { now });
                    if now >= due {
                        recheck_at = Some(now + stall);
                        match locate(ctx, tab, port_arg, step) {
                            // Answered on a fresh connection: poll on that
                            // one (whether the old one broke or only hangs).
                            Some(Some(f)) => {
                                port = f.port;
                                cdp = f.cdp;
                                recheck_at = None;
                            }
                            Some(None) => return gone(port),
                            // No definitive answer: keep showing.
                            None => {}
                        }
                    }
                }
            }
        }
        if viewer_gone {
            ctx.sys.sleep(left.min(TAB_POLL));
            continue;
        }
        match events.next(left.min(TAB_POLL)) {
            VncEvent::Idle => backoff = TAB_POLL,
            VncEvent::Closed => {
                // wayvnc exited: its last viewer left (--exit-on-disconnect),
                // `tab unshow`, or its own timer at the deadline. Or only
                // wayvncctl ended (below).
                if timed_out() {
                    return end("timeout", exit::TIMEOUT);
                }
                let now = Instant::now();
                if now < next_check {
                    // Backing off: keep the tab polls going meanwhile.
                    ctx.sys.sleep(left.min(TAB_POLL).min((next_check - now).as_secs_f64()));
                    continue;
                }
                // Stopped only if systemd says so: an unknown state (systemctl
                // failing) is looked at again later, until the deadline.
                let state = ctx.sys.unit_active_state(ctx, v.unit);
                let stopped = state.as_deref().is_some_and(stopping_or_stopped);
                if seen_viewer || stopped {
                    if until {
                        // Nobody can see the tab any more, but the condition
                        // can still be met: keep watching the tab.
                        viewer_gone = true;
                        emit(ctx, vec![("viewer", "stopped".into())]);
                        continue;
                    }
                    return end(if seen_viewer { "disconnected" } else { "stopped" }, 0);
                }
                // The event stream broke but wayvnc runs (or its state can't
                // be read yet). Usually wayvncctl just saw the exit before
                // systemd did, and the next look settles it; if wayvncctl
                // itself died, reopen the stream (without -w: it ends at once
                // if wayvnc is gone) so later viewer events are seen.
                // Back off the state checks meanwhile.
                next_check = Instant::now() + Duration::from_secs_f64(backoff);
                backoff = (backoff * 2.0).min(STREAM_BACKOFF_MAX);
                if state.as_deref().is_some_and(is_running) && Instant::now() >= reopen_at {
                    reopen_at = Instant::now() + Duration::from_secs_f64(STREAM_REOPEN);
                    if let Ok(fresh) = ctx.sys.vnc_events(ctx, &viewer_control(ctx, v.output), false) {
                        events = fresh;
                    }
                }
            }
            VncEvent::Event(ev) => {
                backoff = TAB_POLL;
                let clients = ev["params"]["connection_count"].clone();
                match ev["method"].as_str() {
                    Some("client-connected") => {
                        seen_viewer = true;
                        emit(ctx, vec![("viewer", "connected".into()), ("clients", clients)]);
                    }
                    Some("client-disconnected") => {
                        emit(ctx, vec![("viewer", "disconnected".into()), ("clients", clients.clone())]);
                        if clients.as_i64() == Some(0) && !until {
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
    let was = match output.as_deref() {
        Some(o) => stop_viewer(ctx, o, Stop::Thorough)?,
        None => false,
    };
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
