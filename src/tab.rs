//! Drive one tab of an agent-display browser over CDP. The persistent
//! browser is shared by all agents, so every command takes an explicit tab
//! id (from `tab open`), never activates a tab and never touches other
//! targets. There is no "current tab" state anywhere.
//!
//! Each agent tab window gets its own headless output. On HEADLESS-1 all
//! windows share one tabbed stack, so all but the focused one are hidden;
//! sway sends hidden windows no frame callbacks, and Page.captureScreenshot
//! on them waits for a frame that never comes. A window alone on its own
//! output is always on screen. Outputs are created per `tab open` and
//! unplugged once their window is gone. Each is marked with a file in
//! tab-outputs/ named after the output, holding the tab id.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::browser::{OWNERSHIP_TIMEOUT, cdp_problem};
use crate::cdp::CdpConn;
use crate::cli::TabOpts;
use crate::ctx::*;
use crate::display::{display, ensure_runtime_dir, walk, windows};
use crate::util::{Error, Result, absolute, b64decode, fmt_g, hex, py_repr, random_bytes, unlink_missing_ok, write_private};
use crate::{bail, json};

/// `/json/list` timeout. Raised with the ownership check (5 s in the
/// Python): it is the same stalled HTTP server.
pub const TARGETS_TIMEOUT: f64 = 15.0;
/// sway poll interval while a tab window appears/disappears. The Python
/// polls every 20 ms because each poll spawns swaymsg; over IPC a poll
/// costs ~0.3 ms.
pub const WINDOW_POLL: f64 = 0.005;
/// `tab close` attempts while the browser unit stays active (fix 1).
pub const CLOSE_ATTEMPTS: u32 = 3;

pub fn valid_tab_id(tab: &str) -> bool {
    (8..=64).contains(&tab.len()) && tab.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn valid_tab_output(name: &str) -> bool {
    name.strip_prefix("HEADLESS-").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// [(port, unit)] for running agent-display browsers (`browser list`), in
/// the Python dict's order (later duplicates replace earlier values).
pub fn owned_ports(ctx: &Ctx) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = Vec::new();
    for b in ctx.sys.browsers(ctx) {
        let Some(port) = b.port() else { continue };
        if reserved_reason(port).is_some() {
            continue;
        }
        match out.iter_mut().find(|(p, _)| *p == port) {
            Some(slot) => slot.1 = b.unit().to_string(),
            None => out.push((port, b.unit().to_string())),
        }
    }
    out
}

pub fn check_owned(ctx: &Ctx, port: i64, unit: &str) -> Result<u16> {
    if let Some(why) = reserved_reason(port) {
        bail!("port {port} is reserved ({why}); never drive it");
    }
    let Ok(p) = u16::try_from(port) else {
        bail!("CDP on port {port} is not served from {unit}; refusing");
    };
    if let Some(problem) = cdp_problem(ctx, p, unit, OWNERSHIP_TIMEOUT) {
        bail!("{problem}; refusing");
    }
    Ok(p)
}

/// Port to open or list tabs on: --port, else the persistent browser.
pub fn tab_port(ctx: &Ctx, port: Option<i64>) -> Result<u16> {
    let owned = owned_ports(ctx);
    if let Some(port) = port {
        let unit = owned.iter().find(|(p, _)| *p == port).map(|(_, u)| u.clone());
        match unit {
            Some(u) if reserved_reason(port).is_none() => return check_owned(ctx, port, &u),
            _ => bail!("port {port} is not an agent-display browser (see `agent-display browser list`); refusing"),
        }
    }
    if let Some((p, unit)) = owned.iter().find(|(_, u)| u == BROWSER_UNIT) {
        return check_owned(ctx, *p, unit);
    }
    match owned.len() {
        1 => check_owned(ctx, owned[0].0, &owned[0].1),
        0 => bail!("no agent-display browser is running; start one with `agent-display browser start`"),
        _ => bail!("several agent browsers are running; pass --port"),
    }
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "None".into(),
        Some(other) => other.to_string(),
    }
}

/// (port, target) for TAB in an agent-display browser, or Error.
pub fn find_tab(ctx: &Ctx, tab: &str, port: Option<i64>) -> Result<(u16, Value)> {
    if !valid_tab_id(tab) {
        bail!("invalid tab id {}", py_repr(tab));
    }
    let owned = owned_ports(ctx);
    let ports: Vec<i64> = match port {
        Some(p) => {
            tab_port(ctx, Some(p))?;
            vec![p]
        }
        None => {
            let mut v: Vec<i64> = owned.iter().map(|(p, _)| *p).collect();
            v.sort();
            v
        }
    };
    let mut skipped = Vec::new();
    for p in ports {
        let attempt = (|| -> Result<(u16, Vec<Value>)> {
            let p16 = if port.is_none() {
                let unit = owned.iter().find(|(q, _)| *q == p).map(|(_, u)| u.clone()).unwrap_or_default();
                check_owned(ctx, p, &unit)?
            } else {
                p as u16
            };
            Ok((p16, ctx.sys.cdp_targets(p16, TARGETS_TIMEOUT)?))
        })();
        let (p16, targets) = match attempt {
            Ok(x) => x,
            Err(e) if port.is_none() => {
                // Another agent's browser may be starting or hung; it must
                // not break lookups of tabs in the healthy ones.
                skipped.push(format!("{p} ({e})"));
                continue;
            }
            Err(e) => return Err(e),
        };
        for t in targets {
            if t.get("id").and_then(Value::as_str) == Some(tab) {
                let kind = str_of(t.get("type"));
                if kind != "page" {
                    bail!("{tab} is a {kind}, not a tab");
                }
                return Ok((p16, t));
            }
        }
    }
    let extra = if skipped.is_empty() { String::new() } else { format!("; skipped unhealthy browsers: {}", skipped.join(", ")) };
    bail!("no tab {tab} in an agent-display browser (closed?){extra}");
}

/// `timeout or default`, as the Python's falsy check does.
fn or_default(t: Option<f64>, default: f64) -> f64 {
    match t {
        Some(x) if x != 0.0 => x,
        _ => default,
    }
}

/// Flat CDP session on one tab; detaches (never closes) on drop.
pub struct Tab<'a, 'c> {
    ctx: &'a Ctx<'c>,
    pub port: u16,
    pub tab: String,
    pub timeout: f64,
    cdp: Box<dyn CdpConn>,
    session: String,
}

impl<'a, 'c> Tab<'a, 'c> {
    pub fn open(ctx: &'a Ctx<'c>, tab: &str, port: Option<i64>, timeout: f64) -> Result<Tab<'a, 'c>> {
        let (port, _) = find_tab(ctx, tab, port)?;
        let mut cdp = ctx.sys.cdp_connect(ctx, port, timeout)?;
        let session = match cdp.call("Target.attachToTarget", json!({"targetId": tab, "flatten": true}), None, None) {
            Ok(r) => r.get("sessionId").and_then(Value::as_str).map(String::from),
            Err(e) => {
                cdp.close();
                return Err(e);
            }
        };
        let Some(session) = session else {
            cdp.close();
            bail!("Target.attachToTarget: no sessionId");
        };
        Ok(Tab { ctx, port, tab: tab.to_string(), timeout, cdp, session })
    }

    pub fn call(&mut self, method: &str, params: Value, timeout: Option<f64>) -> Result<Value> {
        self.cdp.call(method, params, Some(&self.session), timeout)
    }

    pub fn wait(&mut self, method: &str, timeout: Option<f64>) -> Result<Value> {
        self.cdp.wait_event(method, Some(&self.session), timeout)
    }

    pub fn evaluate(&mut self, expression: &str, timeout: Option<f64>) -> Result<Value> {
        let res = self.call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            timeout,
        )?;
        if let Some(d) = res.get("exceptionDetails") {
            let desc = d.get("exception").and_then(|e| e.get("description")).filter(|v| truthy(v));
            let text = desc.or_else(|| d.get("text").filter(|v| truthy(v)));
            let text = match text {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => "exception".into(),
            };
            bail!("eval threw: {text}");
        }
        let r = res.get("result").cloned().unwrap_or_else(|| json!({}));
        if let Some(v) = r.get("value") {
            return Ok(v.clone());
        }
        if r.get("type").and_then(Value::as_str) == Some("undefined") {
            return Ok(Value::Null);
        }
        Ok(r.get("description")
            .filter(|v| truthy(v))
            .or_else(|| r.get("unserializableValue"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub fn info(&mut self) -> Result<Value> {
        self.evaluate("({url: location.href, title: document.title})", None)
    }

    pub fn goto(&mut self, url: &str, wait_ms: Option<i64>, wait_load: bool, timeout: Option<f64>) -> Result<Value> {
        let timeout = or_default(timeout, self.timeout);
        if wait_load {
            self.call("Page.enable", json!({}), None)?;
        }
        let res = self.call("Page.navigate", json!({"url": url}), Some(timeout))?;
        if let Some(err) = res.get("errorText").filter(|v| truthy(v)) {
            bail!("navigate {url}: {}", err.as_str().map(String::from).unwrap_or_else(|| err.to_string()));
        }
        // Same-document navigations (#hash) have no loader and no load event.
        if wait_load
            && res.get("loaderId").is_some_and(truthy)
            && self.wait("Page.loadEventFired", Some(timeout)).is_err()
        {
            bail!("{url} did not finish loading in {}s; use --wait-ms to wait a fixed time instead", fmt_g(timeout));
        }
        if let Some(ms) = wait_ms.filter(|ms| *ms != 0) {
            self.ctx.sys.sleep(ms as f64 / 1000.0);
        }
        self.info()
    }

    /// PNG of the tab. Overriding the viewport (same size, or the full
    /// content height) after a 1 px nudge forces a new frame; a capture
    /// that gets no frame never answers, so give up early and retry.
    pub fn screenshot(&mut self, path: &Path, full_page: bool, timeout: Option<f64>) -> Result<PathBuf> {
        let timeout = or_default(timeout, self.timeout);
        let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
        let data = match self.capture(full_page, deadline) {
            Ok(d) => d,
            Err(e) => {
                // Best effort; don't mask the real error.
                let _ = self.call("Emulation.clearDeviceMetricsOverride", json!({}), Some(5.0));
                return Err(e);
            }
        };
        self.call("Emulation.clearDeviceMetricsOverride", json!({}), None)?;
        let data = b64decode(&data)?;
        let path = absolute(path);
        write_atomic(&path, &data)?;
        Ok(path)
    }

    fn capture(&mut self, full_page: bool, deadline: Instant) -> Result<String> {
        let m = self.call("Page.getLayoutMetrics", json!({}), None)?;
        let vp = m.get("cssLayoutViewport").cloned().unwrap_or(Value::Null);
        let width = vp.get("clientWidth").cloned().unwrap_or(Value::Null);
        let vh = vp.get("clientHeight").and_then(Value::as_f64).unwrap_or(0.0);
        let raw = if full_page { m["cssContentSize"]["height"].as_f64().unwrap_or(0.0) } else { vh };
        let height = (raw.trunc() as i64).max(2);
        // A capture that gets a frame takes 50-200 ms per viewport of height;
        // one that doesn't never answers. Give up early on the first attempt
        // (scaled for tall full-page shots), then back off.
        let first = 0.5 * (height as f64 / vh.max(1.0)).max(1.0);
        let mut attempt = 0i32;
        loop {
            for h in [height - 1, height] {
                self.call(
                    "Emulation.setDeviceMetricsOverride",
                    json!({"width": width, "height": h, "deviceScaleFactor": 0, "mobile": false}),
                    None,
                )?;
            }
            let left = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
            let wait = first * 2f64.powi(attempt);
            match self.call("Page.captureScreenshot", json!({"format": "png"}), Some(left.min(wait).max(1e-6))) {
                Ok(r) => return Ok(r.get("data").and_then(Value::as_str).unwrap_or("").to_string()),
                Err(e) if e.is_timeout() && left > wait => attempt += 1,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Tab<'_, '_> {
    fn drop(&mut self) {
        let _ = self.cdp.call("Target.detachFromTarget", json!({"sessionId": self.session}), None, Some(5.0));
        self.cdp.close();
    }
}

/// Write `data` to `path` via a 0600 temp file in the same directory, so
/// the path appears only once the PNG is complete.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut made = None;
    for _ in 0..100 {
        let name: String = random_bytes(8).iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect();
        let tmp = dir.join(format!(".tab-shot.{name}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
            Ok(f) => {
                made = Some((tmp, f));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let Some((tmp, mut f)) = made else { bail!("could not create a temp file in {}", dir.display()) };
    let r = (|| -> Result<()> {
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if r.is_err() {
        unlink_missing_ok(&tmp);
    }
    r
}

// ------------------------------------------------------------ outputs

/// Serialises creating, placing and removing tab outputs across agents
/// (and across the Python and Rust tools: same file, same flock).
pub struct OutputsLock { _file: std::fs::File }

impl OutputsLock {
    pub fn acquire(ctx: &Ctx) -> Result<OutputsLock> {
        ensure_runtime_dir(ctx)?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(ctx.runtime_dir().join("tab-outputs.lock"))?;
        loop {
            // SAFETY: flock on a fd we own.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(OutputsLock { _file: f });
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }
}

pub fn output_names(ctx: &Ctx, disp: &Disp) -> Result<HashSet<String>> {
    let outs = ctx.sys.sway_json(disp, "get_outputs")?;
    Ok(outs.as_array().into_iter().flatten().filter_map(|o| o["name"].as_str().map(String::from)).collect())
}

/// WIDTHxHEIGHT of the main output's current mode.
pub fn main_output_size(ctx: &Ctx, disp: &Disp) -> Result<String> {
    let outs = ctx.sys.sway_json(disp, "get_outputs")?;
    for o in outs.as_array().into_iter().flatten() {
        if o["name"].as_str() == Some(OUTPUT) {
            let mode = o.get("current_mode").filter(|m| truthy(m)).unwrap_or(&o["rect"]);
            let n = |k: &str| mode[k].as_f64().unwrap_or(0.0) as i64;
            return Ok(format!("{}x{}", n("width"), n("height")));
        }
    }
    bail!("{OUTPUT} is missing");
}

/// {output name: number of windows on it}.
pub fn output_windows(ctx: &Ctx, disp: &Disp) -> Result<BTreeMap<String, usize>> {
    let tree = ctx.sys.sway_json(disp, "get_tree")?;
    let mut out = BTreeMap::new();
    for o in tree.get("nodes").and_then(Value::as_array).into_iter().flatten() {
        if o.get("type").and_then(Value::as_str) == Some("output") {
            let mut w = Vec::new();
            walk(o, &mut w);
            out.insert(o["name"].as_str().unwrap_or("").to_string(), w.len());
        }
    }
    Ok(out)
}

/// {output: tab id} for outputs `tab open` created.
pub fn marked_outputs(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(ctx.tab_outputs_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if valid_tab_output(&name)
                && name != OUTPUT
                && let Ok(text) = std::fs::read_to_string(e.path())
            {
                out.insert(name, text.trim().to_string());
            }
        }
    }
    out
}

/// Unplug tab outputs whose window is gone. Call with OutputsLock held.
/// Only marked `HEADLESS-N` outputs other than HEADLESS-1, and only empty.
pub fn gc_tab_outputs(ctx: &Ctx, disp: &Disp) -> Result<Vec<String>> {
    let counts = output_windows(ctx, disp)?;
    let mut removed = Vec::new();
    for name in marked_outputs(ctx).keys() {
        match counts.get(name) {
            None => unlink_missing_ok(&ctx.tab_outputs_dir().join(name)),
            Some(0) => {
                if ctx.sys.sway_cmd(disp, &format!("output {name} unplug")).is_err() {
                    continue;
                }
                unlink_missing_ok(&ctx.tab_outputs_dir().join(name));
                removed.push(name.clone());
            }
            Some(_) => {}
        }
    }
    Ok(removed)
}

pub fn find_window_real(ctx: &Ctx, disp: &Disp, title: &str, timeout: f64) -> Option<Window> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        let win = windows(ctx, disp).ok().and_then(|ws| ws.into_iter().find(|w| w.title_str().starts_with(title)));
        if win.is_some() || Instant::now() > deadline {
            return win;
        }
        ctx.sys.sleep(WINDOW_POLL);
    }
}

pub fn focused_window(ctx: &Ctx, disp: &Disp) -> Result<Option<i64>> {
    Ok(windows(ctx, disp)?.into_iter().find(|w| w.focused).map(|w| w.id))
}

/// Target.createTarget that never leaves an untracked tab behind. If
/// Chromium is too busy to answer in time it may still create the tab
/// later, and the caller never learns its id: find it by the nonce in its
/// URL and close it before giving up.
pub fn create_target(ctx: &Ctx, cdp: &mut dyn CdpConn, params: Value, nonce: &str, timeout: f64) -> Result<String> {
    let err = match cdp.call("Target.createTarget", params, None, Some(timeout)) {
        Ok(r) => match r.get("targetId").and_then(Value::as_str) {
            Some(id) => return Ok(id.to_string()),
            None => Error::new("Target.createTarget: no targetId"),
        },
        Err(e) => e,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let late: Vec<String> = ctx
            .sys
            .cdp_targets(cdp.port(), TARGETS_TIMEOUT)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.get("url").and_then(Value::as_str).unwrap_or("").contains(nonce))
            .filter_map(|t| t.get("id").and_then(Value::as_str).map(String::from))
            .collect();
        if !late.is_empty() {
            for id in late {
                let _ = cdp.call("Target.closeTarget", json!({"targetId": id}), None, Some(10.0));
            }
            break;
        }
        ctx.sys.sleep(0.2);
    }
    Err(err)
}

/// New background window for a tab, alone on its own output if possible.
/// Returns (target id, output name or None). Without the agent display (or
/// if sway can't add an output) it falls back to a plain window.
pub fn create_tab_window(ctx: &Ctx, cdp: &mut dyn CdpConn) -> Result<(String, Option<String>)> {
    let nonce = hex(&random_bytes(8));
    let title = format!("agent-display-tab-{nonce}");
    let params = json!({"url": format!("data:text/html,<title>{title}</title>"), "newWindow": true, "background": true});
    let Ok(disp) = display(ctx) else {
        return Ok((create_target(ctx, cdp, params, &nonce, 30.0)?, None));
    };
    let _lock = OutputsLock::acquire(ctx)?;
    gc_tab_outputs(ctx, &disp)?;
    let before_focus = focused_window(ctx, &disp)?;
    let mut output: Option<String> = None;
    let made = (|| -> Result<()> {
        let names = output_names(ctx, &disp)?;
        ctx.sys.sway_cmd(&disp, "create_output")?;
        let new: Vec<String> = output_names(ctx, &disp)?.difference(&names).cloned().collect();
        if new.len() == 1 && valid_tab_output(&new[0]) {
            let name = new[0].clone();
            output = Some(name.clone());
            // Mark it at once, so it's cleaned up whatever happens next.
            let dir = ctx.tab_outputs_dir();
            {
                use std::os::unix::fs::DirBuilderExt;
                match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
            }
            write_private(&dir.join(&name), "pending")?;
            // Runtime outputs start at wlroots' default size; match the
            // main output so tab viewports follow `start --size`.
            let size = main_output_size(ctx, &disp)?;
            ctx.sys.sway_cmd(&disp, &format!("output {name} mode --custom {size}"))?;
        }
        Ok(())
    })();
    if made.is_err() {
        if output.is_some() {
            let _ = gc_tab_outputs(ctx, &disp); // still empty: unplug it now
        }
        output = None;
    }
    let tab = match create_target(ctx, cdp, params, &nonce, 30.0) {
        Ok(t) => t,
        Err(e) => {
            let _ = gc_tab_outputs(ctx, &disp); // our output is still empty: unplug it
            return Err(e);
        }
    };
    if let Some(o) = &output {
        write_private(&ctx.tab_outputs_dir().join(o), &tab)?;
    }
    let win = ctx.sys.find_window(ctx, &disp, &title, 10.0);
    match &win {
        None => output = None, // never placed; left empty, the next gc removes it
        Some(w) => {
            if let Some(o) = &output
                && ctx.sys.sway_cmd(&disp, &format!("[con_id={}] move container to output {o}", w.id)).is_err()
            {
                output = None; // left empty: the next gc removes it
            }
        }
    }
    // sway focuses new windows: give the seat back (another agent may be
    // typing with OS input). With nothing focused before, at least keep the
    // seat on the main output, not on the new tab output.
    let _ = (|| -> Result<()> {
        match before_focus {
            Some(id) => {
                if focused_window(ctx, &disp)? != Some(id) {
                    ctx.sys.sway_cmd(&disp, &format!("[con_id={id}] focus"))?;
                }
            }
            None if output.is_some() => ctx.sys.sway_cmd(&disp, &format!("focus output {OUTPUT}"))?,
            None => {}
        }
        Ok(())
    })();
    Ok((tab, output))
}

/// After closing TAB, unplug its output once its window is gone. Best
/// effort: the tab is already closed, and the next `tab open` collects a
/// leftover output, so failures here never fail the command.
pub fn remove_tab_output(ctx: &Ctx, tab: &str, timeout: f64) -> Option<String> {
    let disp = display(ctx).ok()?;
    let output = marked_outputs(ctx).into_iter().find(|(_, t)| t == tab).map(|(o, _)| o)?;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        let busy = output_windows(ctx, &disp).ok()?.get(&output).copied().unwrap_or(0) > 0;
        if !busy || Instant::now() >= deadline {
            break;
        }
        ctx.sys.sleep(WINDOW_POLL);
    }
    let _lock = OutputsLock::acquire(ctx).ok()?;
    gc_tab_outputs(ctx, &disp).ok()?.contains(&output).then_some(output)
}

// ------------------------------------------------------------ commands

fn close_target(ctx: &Ctx, port: u16, tab: &str) -> Result<Value> {
    let mut cdp = ctx.sys.cdp_connect(ctx, port, 10.0)?;
    let r = cdp.call("Target.closeTarget", json!({"targetId": tab}), None, None);
    cdp.close();
    r
}

pub fn cmd_tab_open(ctx: &Ctx, o: &TabOpts, url: Option<&str>) -> Result<i32> {
    let port = tab_port(ctx, o.port_raw)?;
    let (tab, output) = {
        let mut cdp = ctx.sys.cdp_connect(ctx, port, 10.0)?;
        // Own window: Chromium doesn't paint background tabs, so a
        // screenshot of one hangs; activating it would take over another
        // agent's tab. See create_tab_window for the output.
        let r = create_tab_window(ctx, cdp.as_mut());
        cdp.close();
        r?
    };
    let mut out = Map::new();
    out.insert("tab".into(), tab.clone().into());
    out.insert("port".into(), port.into());
    out.insert("output".into(), output.map(Value::from).unwrap_or(Value::Null));
    if let Some(url) = url.filter(|u| !u.is_empty()) {
        let nav = (|| -> Result<Value> {
            let mut t = Tab::open(ctx, &tab, Some(port as i64), o.timeout)?;
            t.goto(url, None, true, None)
        })();
        match nav {
            Ok(Value::Object(info)) => out.extend(info),
            Ok(_) => {}
            Err(e) => {
                // The caller never got the id, so it couldn't close the tab.
                if let Err(ce) = close_target(ctx, port, &tab).map(|_| remove_tab_output(ctx, &tab, 5.0)) {
                    ctx.eprintln(&format!(
                        "agent-display: could not close tab {tab}: {}; close it with `agent-display tab close {tab}`",
                        ce.msg
                    ));
                }
                return Err(e);
            }
        }
    }
    ctx.println(&json::dumps(&Value::Object(out), true));
    Ok(0)
}

pub fn cmd_tab_goto(ctx: &Ctx, o: &TabOpts, tab: &str, url: &str, wait_ms: Option<i64>) -> Result<i32> {
    let info = Tab::open(ctx, tab, o.port_raw, o.timeout)?.goto(url, wait_ms, wait_ms.is_none(), None)?;
    ctx.println(&json::dumps(&info, true));
    Ok(0)
}

pub fn cmd_tab_eval(ctx: &Ctx, o: &TabOpts, tab: &str, js: &str) -> Result<i32> {
    let value = Tab::open(ctx, tab, o.port_raw, o.timeout)?.evaluate(js, None)?;
    ctx.println(&json::dumps(&value, false));
    Ok(0)
}

pub fn cmd_tab_shot(ctx: &Ctx, o: &TabOpts, tab: &str, path: &str, full_page: bool) -> Result<i32> {
    let path = Tab::open(ctx, tab, o.port_raw, o.timeout)?.screenshot(Path::new(path), full_page, None)?;
    ctx.println(&path.to_string_lossy());
    Ok(0)
}

pub fn cmd_tab_info(ctx: &Ctx, o: &TabOpts, tab: &str) -> Result<i32> {
    let info = Tab::open(ctx, tab, o.port_raw, o.timeout)?.info()?;
    let mut out = Map::new();
    out.insert("tab".into(), tab.into());
    if let Value::Object(m) = info {
        out.extend(m);
    }
    ctx.println(&json::dumps(&Value::Object(out), false));
    Ok(0)
}

/// Worth another `tab close` attempt: the browser was slow or the
/// connection broke (also when the lookup skipped a browser for that), not
/// an ownership refusal or a missing tab.
fn retryable(e: &Error) -> bool {
    e.os || e.is_timeout() || e.msg.contains("is not answering CDP") || e.msg.contains("CDP closed")
}

fn unit_for_port(ctx: &Ctx, port: u16) -> Option<String> {
    owned_ports(ctx).into_iter().find(|(p, _)| *p == port as i64).map(|(_, u)| u)
}

/// `tab close`, retried (fix 1): under host load the shared browser can
/// stall for seconds, and a failed close used to leave the tab open.
/// Retries only while the tab's browser unit is still active, so a stopped
/// browser still fails at once. If an earlier attempt sent closeTarget and
/// the tab is now gone, that attempt closed it.
pub fn cmd_tab_close(ctx: &Ctx, o: &TabOpts, tab: &str) -> Result<i32> {
    let mut found_on: Option<u16> = None;
    let mut attempt = 0;
    let port = loop {
        attempt += 1;
        let r = (|| -> Result<u16> {
            let (port, _) = find_tab(ctx, tab, o.port_raw)?;
            found_on = Some(port);
            let ok = close_target(ctx, port, tab)?;
            if ok.get("success") == Some(&Value::Bool(false)) {
                bail!("could not close {tab}");
            }
            Ok(port)
        })();
        match r {
            Ok(port) => break port,
            Err(e) => {
                if let Some(port) = found_on
                    && e.msg.starts_with(&format!("no tab {tab} in an agent-display browser"))
                    && !e.msg.contains("skipped unhealthy")
                {
                    break port; // the earlier closeTarget went through
                }
                if attempt >= CLOSE_ATTEMPTS || !retryable(&e) {
                    return Err(e);
                }
                let units: Vec<String> = match found_on {
                    Some(p) => unit_for_port(ctx, p).into_iter().collect(),
                    None => owned_ports(ctx).into_iter().map(|(_, u)| u).collect(),
                };
                if !units.iter().any(|u| ctx.sys.unit_active(ctx, u)) {
                    return Err(e);
                }
                ctx.sys.sleep(attempt as f64);
            }
        }
    };
    let output = remove_tab_output(ctx, tab, 5.0);
    let mut out = Map::new();
    out.insert("closed".into(), tab.into());
    out.insert("port".into(), port.into());
    out.insert("output".into(), output.map(Value::from).unwrap_or(Value::Null));
    ctx.println(&json::dumps(&Value::Object(out), true));
    Ok(0)
}

pub fn cmd_tab_list(ctx: &Ctx, o: &TabOpts) -> Result<i32> {
    let port = tab_port(ctx, o.port_raw)?;
    let tabs: Vec<Value> = ctx
        .sys
        .cdp_targets(port, TARGETS_TIMEOUT)?
        .into_iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .map(|t| {
            let mut m = Map::new();
            m.insert("tab".into(), t.get("id").cloned().unwrap_or(Value::Null));
            m.insert("url".into(), t.get("url").cloned().unwrap_or(Value::Null));
            m.insert("title".into(), t.get("title").cloned().unwrap_or(Value::Null));
            Value::Object(m)
        })
        .collect();
    let mut out = Map::new();
    out.insert("port".into(), port.into());
    out.insert("tabs".into(), Value::Array(tabs));
    ctx.println(&json::dumps_indent(&Value::Object(out), 2, false));
    Ok(0)
}

/// `CDP: ` prefix for socket errors, as the Python's tab handler does.
pub fn cdp_text(e: &Error) -> String {
    if e.os { format!("CDP: {}", e.msg) } else { e.msg.clone() }
}
