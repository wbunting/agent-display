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
//!
//! Tabs are looked up by id with `Target.getTargetInfo` on the browser
//! endpoint, never by scanning `/json/list`: Chromium leaves a page out of
//! that list while its main frame is being swapped (cross-site navigation,
//! prerender activation) or while it is briefly not in a tab strip, which
//! made a live tab look closed. A lookup ends in one of: found; gone (exit
//! 4, only after two definitive "no such target" answers ~250 ms apart from
//! every running browser); browser down (exit 5); or no definitive answer
//! after internal retries (exit 75). It never guesses "closed".

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::browser::{OWNERSHIP_TIMEOUT, listener_problem};
use crate::cdp::CdpConn;
use crate::cli::TabOpts;
use crate::ctx::*;
use crate::display::{display, ensure_runtime_dir, walk, windows};
use crate::util::{Error, FileLock, Result, absolute, b64decode, exit, fmt_g, hex, py_repr, random_bytes, unlink_missing_ok, write_private};
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
/// A lookup with no definitive answer is retried within this budget
/// (seconds), after LOOKUP_BACKOFF pauses: it rides out the stalls the
/// shared browser has under host load (as the 15 s ownership timeout did).
pub const LOOKUP_BUDGET: f64 = 15.0;
/// Per-step timeout (CDP connect, getTargetInfo) of one lookup attempt.
pub const LOOKUP_STEP_TIMEOUT: f64 = 5.0;
pub const LOOKUP_BACKOFF: &[f64] = &[0.5, 1.0, 2.0, 4.0];
/// Pause before asking again whether a tab that looked gone really is.
const GONE_RECHECK: f64 = 0.25;

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

/// The LISTEN socket on `port` belongs to `unit` (refusal, exit 1, if not).
/// Says nothing about whether the browser answers.
pub fn check_listener(ctx: &Ctx, port: i64, unit: &str) -> Result<u16> {
    if let Some(why) = reserved_reason(port) {
        bail!("port {port} is reserved ({why}); never drive it");
    }
    let Ok(p) = u16::try_from(port) else {
        bail!("CDP on port {port} is not served from {unit}; refusing");
    };
    if let Some(problem) = listener_problem(ctx, p, unit) {
        bail!("{problem}; refusing");
    }
    Ok(p)
}

/// `check_listener`, then the browser must answer CDP. Not answering is a
/// stall, not a refusal: exit 75 (try again).
pub fn check_owned(ctx: &Ctx, port: i64, unit: &str) -> Result<u16> {
    let p = check_listener(ctx, port, unit)?;
    if let Err(e) = ctx.sys.cdp_version(p, OWNERSHIP_TIMEOUT) {
        return Err(Error::new(format!("{unit} is not answering CDP on {p}: {}; try again", cdp_text(&e)))
            .with_code(exit::TEMPFAIL));
    }
    Ok(p)
}

/// An explicit --port: refused (1) if it isn't a registered agent-display
/// browser's, browser down (5) if nothing is registered or listening there.
/// `answering`: also require a CDP answer (`check_owned`, 75 on a stall);
/// otherwise only ownership (`check_listener`), for callers that ride out
/// stalls themselves.
pub(crate) fn explicit_port(ctx: &Ctx, port: i64, answering: bool) -> Result<u16> {
    let unit = owned_ports(ctx).into_iter().find(|(p, _)| *p == port).map(|(_, u)| u);
    match unit {
        Some(u) if reserved_reason(port).is_none() => {
            if answering { check_owned(ctx, port, &u) } else { check_listener(ctx, port, &u) }
        }
        // Nothing registered and nothing listening: that browser is gone
        // (stopped or crashed), not someone else's port.
        None if reserved_reason(port).is_none() && u16::try_from(port).is_ok_and(|p| p >= 1024 && ctx.sys.port_free(p)) => {
            Err(browser_down(&format!("no agent-display browser on port {port} (stopped?)")))
        }
        _ => bail!("port {port} is not an agent-display browser (see `agent-display browser list`); refusing"),
    }
}

/// Port to open or list tabs on: --port, else the persistent browser.
pub fn tab_port(ctx: &Ctx, port: Option<i64>) -> Result<u16> {
    let owned = owned_ports(ctx);
    if let Some(port) = port {
        return explicit_port(ctx, port, true);
    }
    if let Some((p, unit)) = owned.iter().find(|(_, u)| u == BROWSER_UNIT) {
        return check_owned(ctx, *p, unit);
    }
    match owned.len() {
        1 => check_owned(ctx, owned[0].0, &owned[0].1),
        0 => Err(browser_down("no agent-display browser is running; start one with `agent-display browser start`")),
        _ => bail!("several agent browsers are running; pass --port"),
    }
}

fn browser_down(msg: &str) -> Error {
    Error::new(msg).with_code(exit::BROWSER_DOWN)
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "None".into(),
        Some(other) => other.to_string(),
    }
}

/// A tab found by id: its browser's port, `Target.getTargetInfo`'s
/// `targetInfo`, and the open browser connection it was found on.
pub struct Found {
    pub port: u16,
    pub info: Value,
    pub cdp: Box<dyn CdpConn>,
}

/// One browser's answer about a tab id.
enum Probe {
    Found(Value, Box<dyn CdpConn>),
    /// Definitive: the browser answered and has no such target.
    Missing,
    /// No answer we can trust (stalled, not ready, connection failed).
    Unavailable(String),
}

pub(crate) fn no_such_target(e: &Error) -> bool {
    e.msg.contains("No target with given id")
}

/// Ask the browser on `port` (`unit`) for `tab` by id. Chromium creates
/// DevTools hosts lazily, so a miss is repeated after `Target.getTargets`
/// (which creates hosts for every live frame) before it counts.
fn probe(ctx: &Ctx, port: u16, unit: &str, tab: &str, step: f64) -> Probe {
    if let Some(problem) = listener_problem(ctx, port, unit) {
        return Probe::Unavailable(problem);
    }
    let mut cdp = match ctx.sys.cdp_connect(ctx, port, step) {
        Ok(c) => c,
        Err(e) => return Probe::Unavailable(format!("{unit} is not answering CDP on {port}: {}", cdp_text(&e))),
    };
    let ask = |cdp: &mut Box<dyn CdpConn>| cdp.call("Target.getTargetInfo", json!({"targetId": tab}), None, Some(step));
    let mut r = ask(&mut cdp);
    if r.as_ref().is_err_and(no_such_target) && cdp.call("Target.getTargets", json!({}), None, Some(step)).is_ok() {
        r = ask(&mut cdp);
    }
    match r {
        Ok(v) => match v.get("targetInfo") {
            Some(info) if info.is_object() => Probe::Found(info.clone(), cdp),
            _ => Probe::Unavailable(format!("{unit}: Target.getTargetInfo: no targetInfo")),
        },
        Err(e) if no_such_target(&e) => Probe::Missing,
        Err(e) => Probe::Unavailable(format!("{unit} on {port}: {}", cdp_text(&e))),
    }
}

/// One pass over the candidate browsers.
enum Scan {
    Found(Found),
    /// Every running candidate said "no such target".
    Missing,
    Unavailable(Vec<String>),
    /// No browser registered (or the given one isn't).
    NoBrowser,
}

fn candidates(ctx: &Ctx, port: Option<i64>) -> Vec<(u16, String)> {
    let mut v: Vec<(u16, String)> = owned_ports(ctx)
        .into_iter()
        .filter(|(p, _)| port.is_none_or(|q| q == *p))
        .filter_map(|(p, u)| u16::try_from(p).ok().map(|p| (p, u)))
        .collect();
    v.sort();
    v
}

fn scan(ctx: &Ctx, tab: &str, port: Option<i64>, step: f64) -> Scan {
    let cands = candidates(ctx, port);
    if cands.is_empty() {
        return Scan::NoBrowser;
    }
    let mut problems = Vec::new();
    for (p, unit) in cands {
        match probe(ctx, p, &unit, tab, step) {
            Probe::Found(info, cdp) => return Scan::Found(Found { port: p, info, cdp }),
            Probe::Missing => {}
            Probe::Unavailable(why) => problems.push(format!("{p} ({why})")),
        }
    }
    if problems.is_empty() { Scan::Missing } else { Scan::Unavailable(problems) }
}

/// Is TAB still in a running agent-display browser (or the one on `port`)?
/// Some(Some(found)) with an open connection to the tab's browser,
/// Some(None) gone (two definitive misses ~250 ms apart, or no browser),
/// None no definitive answer (a browser didn't answer). One scan per pass,
/// `step` per CDP step; no retries beyond the recheck.
pub fn locate(ctx: &Ctx, tab: &str, port: Option<i64>, step: f64) -> Option<Option<Found>> {
    locate_why(ctx, tab, port, step).ok()
}

/// `locate`, saying why there is no definitive answer (`tab wait` reports
/// it in its `stall` events).
pub fn locate_why(ctx: &Ctx, tab: &str, port: Option<i64>, step: f64) -> std::result::Result<Option<Found>, String> {
    for pass in 0..2 {
        match scan(ctx, tab, port, step) {
            Scan::Found(f) => return Ok(Some(f)),
            Scan::NoBrowser => return Ok(None),
            Scan::Unavailable(problems) => return Err(problems.join(", ")),
            Scan::Missing if pass == 0 => ctx.sys.sleep(GONE_RECHECK),
            Scan::Missing => {}
        }
    }
    Ok(None)
}

/// Find TAB in the agent-display browsers (or the one on `port`). Errors
/// carry the exit status: 4 gone (confirmed twice), 5 browser down, 75 no
/// definitive answer within LOOKUP_BUDGET, 1 for refusals and non-tabs.
pub fn lookup(ctx: &Ctx, tab: &str, port: Option<i64>) -> Result<Found> {
    if !valid_tab_id(tab) {
        bail!("invalid tab id {}", py_repr(tab));
    }
    if let Some(p) = port {
        // Refusals (foreign, reserved, hijacked port) and a stopped browser
        // are definitive: no retries. Whether it answers is left to the
        // retry loop below, as without --port: a stall exits 75, not 1.
        explicit_port(ctx, p, false)?;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(LOOKUP_BUDGET);
    let mut tries = 0usize;
    let mut missed_once = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
        match scan(ctx, tab, port, left.clamp(1.0, LOOKUP_STEP_TIMEOUT)) {
            Scan::Found(f) => {
                let kind = str_of(f.info.get("type"));
                if kind != "page" {
                    bail!("{tab} is a {kind}, not a tab");
                }
                return Ok(f);
            }
            Scan::NoBrowser => {
                let msg = match port {
                    Some(p) => format!("no agent-display browser on port {p} (stopped?); tab {tab} went with it"),
                    None => format!("no agent-display browser is running; tab {tab} went with it"),
                };
                return Err(browser_down(&msg));
            }
            Scan::Missing if missed_once => {
                return Err(Error::new(format!(
                    "no tab {tab} in an agent-display browser: it was closed or crashed (checked twice)"
                ))
                .with_code(exit::GONE));
            }
            Scan::Missing => {
                missed_once = true;
                ctx.sys.sleep(GONE_RECHECK);
            }
            Scan::Unavailable(problems) => {
                missed_once = false;
                let pause = LOOKUP_BACKOFF.get(tries).copied();
                tries += 1;
                let left = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
                match pause {
                    Some(pause) if pause < left => ctx.sys.sleep(pause),
                    _ => {
                        return Err(Error::new(format!(
                            "tab {tab}: no definitive answer after {tries} attempts (browser busy or restarting); \
                             try again: {}",
                            problems.join(", ")
                        ))
                        .with_code(exit::TEMPFAIL));
                    }
                }
            }
        }
    }
}

/// Ids of every target in every running agent-display browser, from one
/// `Target.getTargets` per browser (which also creates DevTools hosts for
/// every live frame). None if any browser didn't answer: then nothing can
/// be called gone.
pub fn live_targets(ctx: &Ctx) -> Option<HashSet<String>> {
    let mut ids = HashSet::new();
    for (port, unit) in candidates(ctx, None) {
        if listener_problem(ctx, port, &unit).is_some() {
            return None;
        }
        let mut cdp = ctx.sys.cdp_connect(ctx, port, LOOKUP_STEP_TIMEOUT).ok()?;
        let r = cdp.call("Target.getTargets", json!({}), None, Some(LOOKUP_STEP_TIMEOUT)).ok()?;
        let infos = r.get("targetInfos")?.as_array()?;
        ids.extend(infos.iter().filter_map(|t| t.get("targetId").and_then(Value::as_str).map(String::from)));
    }
    Some(ids)
}

/// `timeout or default`, as the Python's falsy check does.
fn or_default(t: Option<f64>, default: f64) -> f64 {
    match t {
        Some(x) if x != 0.0 => x,
        _ => default,
    }
}

/// Seconds the detach barrier (see `detach_answering_dialogs`) waits for
/// the page.
pub const DIALOG_BARRIER: f64 = 1.0;
/// `Target.detachFromTarget` timeout when a tab session ends.
pub const DETACH_TIMEOUT: f64 = 5.0;

/// End a tab session without losing a JavaScript dialog: one round trip to
/// the page first (the barrier), then detach. A dialog opened by what just
/// ran (a load handler, a short setTimeout) blocks the renderer, so its
/// event arrives before the barrier's reply and the connection answers it
/// (see `cdp::Dialog`) before we detach. One that opens between that reply
/// and the detach (a ~1 ms window) is Chromium's to handle afterwards (see
/// docs/tab-wait.md, "JavaScript dialogs"). Both steps together take at
/// most `limit` s (normally DIALOG_BARRIER + 5 s). Best effort: errors are
/// ignored, the session is going away anyway.
pub fn detach_answering_dialogs(ctx: &Ctx, cdp: &mut dyn CdpConn, session: &str, limit: f64) {
    let start = ctx.sys.now();
    let left = |ctx: &Ctx| (limit - ctx.sys.now().saturating_duration_since(start).as_secs_f64()).max(0.001);
    let _ = cdp.call("Runtime.evaluate", json!({"expression": "0"}), Some(session), Some(DIALOG_BARRIER.min(left(ctx))));
    let _ = cdp.call("Target.detachFromTarget", json!({"sessionId": session}), None, Some(DETACH_TIMEOUT.min(left(ctx))));
}

/// stderr note for each JavaScript dialog a command answered (kind only:
/// the message is page text).
pub fn report_dialogs(ctx: &Ctx, tab: &str, dialogs: &[crate::cdp::Dialog]) {
    for d in dialogs {
        let verb = if d.accepted { "accepted" } else { "dismissed" };
        ctx.eprintln(&format!("agent-display: {verb} a JavaScript {} dialog in tab {tab}", d.kind));
    }
}

/// Message of a CDP `exceptionDetails`: the exception's description, else
/// the text, else "exception". Used by `eval` and `tab wait`.
pub fn exception_text(d: &Value) -> String {
    let desc = d.get("exception").and_then(|e| e.get("description")).filter(|v| truthy(v));
    match desc.or_else(|| d.get("text").filter(|v| truthy(v))) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "exception".into(),
    }
}

/// Flat CDP session on one tab; detaches (never closes) on drop.
///
/// JavaScript dialogs that open while it is attached are dismissed as they
/// open (see `cdp::Dialog` for why they must be answered) and noted on
/// stderr, so a page's alert()/confirm() can't wedge the tab.
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
        let Found { port, mut cdp, .. } = lookup(ctx, tab, port)?;
        cdp.set_default_timeout(timeout);
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
        cdp.set_dialog_policy(false);
        let mut t = Tab { ctx, port, tab: tab.to_string(), timeout, cdp, session };
        // Page events report dialogs (and the load event `goto` waits for).
        t.call("Page.enable", json!({}), None)?;
        Ok(t)
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
            bail!("eval threw: {}", exception_text(d));
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
        detach_answering_dialogs(self.ctx, self.cdp.as_mut(), &self.session, DIALOG_BARRIER + DETACH_TIMEOUT);
        report_dialogs(self.ctx, &self.tab, &self.cdp.take_dialogs());
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
pub struct OutputsLock {
    _lock: FileLock,
}

impl OutputsLock {
    pub fn acquire(ctx: &Ctx) -> Result<OutputsLock> {
        ensure_runtime_dir(ctx)?;
        Ok(OutputsLock { _lock: FileLock::acquire(&ctx.runtime_dir().join("tab-outputs.lock"))? })
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

/// The output `tab open` gave TAB (from its marker), if any.
pub fn tab_output(ctx: &Ctx, tab: &str) -> Option<String> {
    marked_outputs(ctx).into_iter().find(|(_, t)| t == tab).map(|(o, _)| o)
}

/// Unplug tab outputs whose window is gone. Call with OutputsLock held.
/// Only marked `HEADLESS-N` outputs other than HEADLESS-1, only empty, and
/// only once the marked tab is confirmed gone (`live_targets`, twice): an output that
/// is briefly empty while its live tab's window is remapped (fullscreen, a
/// dialog, a crashed GPU surface) stays. Its viewer (`tab show`) goes too.
pub fn gc_tab_outputs(ctx: &Ctx, disp: &Disp) -> Result<Vec<String>> {
    gc_outputs(ctx, disp, None)
}

/// `gc_tab_outputs`, knowing that tab `closed` was just closed (no need to
/// ask the browser about it). The browsers are asked at most twice per gc
/// (all suspect tabs at once, then once more ~250 ms later), and only when
/// an empty output's tab isn't known to be gone already.
fn gc_outputs(ctx: &Ctx, disp: &Disp, closed: Option<&str>) -> Result<Vec<String>> {
    let counts = output_windows(ctx, disp)?;
    let mut gone: Vec<String> = Vec::new();
    let mut suspects: Vec<(String, String)> = Vec::new();
    for (name, tab) in marked_outputs(ctx) {
        match counts.get(&name) {
            None => {
                // Best effort: a viewer that can't be stopped now keeps its
                // files and exits by its own timer.
                let _ = crate::show::stop_viewer(ctx, &name, crate::show::Stop::Any);
                unlink_missing_ok(&ctx.tab_outputs_dir().join(&name));
            }
            // "pending": its `tab open` died before creating the tab
            // (markers are only pending while the lock is held).
            Some(0) if tab == "pending" || closed == Some(tab.as_str()) => gone.push(name),
            Some(0) => suspects.push((name, tab)),
            Some(_) => {}
        }
    }
    for pass in 0..2 {
        if suspects.is_empty() {
            break;
        }
        if pass == 1 {
            ctx.sys.sleep(GONE_RECHECK);
        }
        // A browser that doesn't answer: keep every suspect's output.
        let Some(live) = live_targets(ctx) else {
            suspects.clear();
            break;
        };
        suspects.retain(|(_, tab)| !live.contains(tab));
    }
    gone.extend(suspects.into_iter().map(|(name, _)| name));
    let mut removed = Vec::new();
    for name in gone {
        let _ = crate::show::stop_viewer(ctx, &name, crate::show::Stop::Any);
        if ctx.sys.sway_cmd(disp, &format!("output {name} unplug")).is_err() {
            continue;
        }
        unlink_missing_ok(&ctx.tab_outputs_dir().join(&name));
        removed.push(name);
    }
    Ok(removed)
}

/// Unplug our own new output while it is still empty (with the lock held),
/// e.g. when the tab's window never showed up.
fn drop_own_output(ctx: &Ctx, disp: &Disp, name: &str) {
    let empty = output_windows(ctx, disp).ok().is_some_and(|c| c.get(name).copied().unwrap_or(0) == 0);
    if empty && ctx.sys.sway_cmd(disp, &format!("output {name} unplug")).is_ok() {
        unlink_missing_ok(&ctx.tab_outputs_dir().join(name));
    }
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
    Err(stalled(err))
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
            crate::util::mkdir_private(&dir)?;
            write_private(&dir.join(&name), "pending")?;
            // Runtime outputs start at wlroots' default size; match the
            // main output so tab viewports follow `start --size`.
            let size = main_output_size(ctx, &disp)?;
            ctx.sys.sway_cmd(&disp, &format!("output {name} mode --custom {size}"))?;
        }
        Ok(())
    })();
    if made.is_err()
        && let Some(o) = output.take()
    {
        drop_own_output(ctx, &disp, &o); // still empty: unplug it now
    }
    let tab = match create_target(ctx, cdp, params, &nonce, 30.0) {
        Ok(t) => t,
        Err(e) => {
            if let Some(o) = &output {
                drop_own_output(ctx, &disp, o); // still empty: unplug it now
            }
            return Err(e);
        }
    };
    if let Some(o) = &output {
        write_private(&ctx.tab_outputs_dir().join(o), &tab)?;
    }
    let win = ctx.sys.find_window(ctx, &disp, &title, 10.0);
    let placed = match (&win, &output) {
        (Some(w), Some(o)) => ctx.sys.sway_cmd(&disp, &format!("[con_id={}] move container to output {o}", w.id)).is_ok(),
        _ => false,
    };
    if !placed && let Some(o) = output.take() {
        drop_own_output(ctx, &disp, &o); // never used: don't leave it around
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
    let output = tab_output(ctx, tab)?;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        let busy = output_windows(ctx, &disp).ok()?.get(&output).copied().unwrap_or(0) > 0;
        if !busy || Instant::now() >= deadline {
            break;
        }
        ctx.sys.sleep(WINDOW_POLL);
    }
    let _lock = OutputsLock::acquire(ctx).ok()?;
    gc_outputs(ctx, &disp, Some(tab)).ok()?.contains(&output).then_some(output)
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
        let mut cdp = ctx.sys.cdp_connect(ctx, port, 10.0).map_err(stalled)?;
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
/// connection broke, not a refusal or a missing tab.
fn retryable(e: &Error) -> bool {
    e.os || e.is_timeout() || e.msg.contains("is not answering CDP") || e.msg.contains("CDP closed")
}

/// A CDP step that timed out or lost its connection on an owned browser is
/// a stall: exit 75 (try again), not 1.
fn stalled(e: Error) -> Error {
    if retryable(&e) { e.with_code(exit::TEMPFAIL) } else { e }
}

fn unit_for_port(ctx: &Ctx, port: u16) -> Option<String> {
    owned_ports(ctx).into_iter().find(|(p, _)| *p == port as i64).map(|(_, u)| u)
}

/// `tab close`, retried (fix 1): under host load the shared browser can
/// stall for seconds, and a failed close used to leave the tab open.
/// Retries only while the tab's browser unit is still active, so a stopped
/// browser fails at once (exit 5). If an earlier attempt sent closeTarget
/// and the tab is now confirmed gone (by the lookup, or by closeTarget
/// itself answering "no such target"), that attempt closed it. A close that
/// keeps stalling exits 75; a tab that was already gone (closed by someone
/// else between the lookup and closeTarget) exits 4, its output removed.
pub fn cmd_tab_close(ctx: &Ctx, o: &TabOpts, tab: &str) -> Result<i32> {
    let mut sent_on: Option<u16> = None;
    let mut attempt = 0;
    let port = loop {
        attempt += 1;
        let mut found = match lookup(ctx, tab, o.port_raw) {
            Ok(f) => f,
            Err(e) if e.code == exit::GONE && sent_on.is_some() => break sent_on.unwrap_or_default(),
            Err(e) => return Err(e),
        };
        sent_on = Some(found.port);
        let r = found.cdp.call("Target.closeTarget", json!({"targetId": tab}), None, Some(10.0));
        found.cdp.close();
        match r {
            Ok(ok) if ok.get("success") == Some(&Value::Bool(false)) => bail!("could not close {tab}"),
            Ok(_) => break found.port,
            // An earlier closeTarget of ours went through late: closed.
            Err(e) if no_such_target(&e) && attempt > 1 => break found.port,
            // Closed by someone else since our lookup found it.
            Err(e) if no_such_target(&e) => {
                remove_tab_output(ctx, tab, 5.0);
                return Err(Error::new(format!(
                    "no tab {tab} in an agent-display browser: it was closed just before this close ({})",
                    e.msg
                ))
                .with_code(exit::GONE));
            }
            Err(e) if !retryable(&e) => return Err(e),
            Err(e) => {
                // Browser down (5) only if unregistered or confirmed stopped.
                // Unknown (systemctl failing during the same stall) or in
                // transition: keep retrying, 75 after the last attempt.
                let state = unit_for_port(ctx, found.port).map(|u| crate::systemd::unit_state(ctx, &u));
                if matches!(state, None | Some(Some(false))) {
                    return Err(e.with_code(exit::BROWSER_DOWN));
                }
                if attempt >= CLOSE_ATTEMPTS {
                    return Err(e.with_code(exit::TEMPFAIL));
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
        .cdp_targets(port, TARGETS_TIMEOUT)
        .map_err(stalled)?
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
