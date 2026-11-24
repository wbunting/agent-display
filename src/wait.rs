//! `tab wait`: block until one tab reaches a state (URL, title, text,
//! selector, a DOM expression, or closed), for waits of seconds to hours,
//! such as a sign-in the user approves on their phone. See docs/tab-wait.md.
//!
//! - One flat CDP session on the caller's own tab. Nothing else in the
//!   browser is attached, activated or focused.
//! - The state is computed in an isolated world (`Page.createIsolatedWorld`):
//!   page scripts can't see or patch it, and `Runtime.enable` is never sent.
//! - Event driven: each check is one `Runtime.callFunctionOn` that returns
//!   at once if the state differs from the last one seen, else on the first
//!   DOM mutation, hashchange or popstate that changes it, or on a heartbeat
//!   tick. A navigation destroys the world; the call fails at once and a new
//!   world is made in the new document.
//! - The tab is found with the shared lookup (`tab::locate`: by id with
//!   `Target.getTargetInfo`), so `tab wait` and the other `tab` commands
//!   agree on found, gone and "no definitive answer".
//! - Failures are classified, never guessed. Closed needs evidence: the
//!   lookup's two definitive misses ~250 ms apart, and the browser still
//!   answering afterwards. Crashed: `Inspector.targetCrashed`. Browser
//!   down: its browser is unregistered or its unit confirmed stopped (an
//!   unknown unit state is not "down"). Anything else (timeouts, refused
//!   connections, a lookup without a definitive answer) is a stall:
//!   reported once, retried with backoff. A stall never ends the wait
//!   early; if the deadline passes during one, the exit is 75 (no
//!   definitive answer), not 3.
//! - JavaScript dialogs are answered as they open (`--dialog`, default
//!   dismiss); see `cdp::Dialog`.
//! - Output never contains page text: only URL, title, which conditions
//!   hold, and dialog kinds. Every line goes through `Waiter::print`.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::cdp::{CdpConn, DETACHED};
use crate::cli::{TabOpts, WaitSpec};
use crate::ctx::*;
use crate::tab::{
    DETACH_TIMEOUT, DIALOG_BARRIER, Found, LOOKUP_STEP_TIMEOUT, detach_answering_dialogs, exception_text, explicit_port,
    locate_why, owned_ports, valid_tab_id,
};
use crate::util::{Error, Result, exit};
use crate::{bail, json};

/// Longest any one setup step (attach, enable, frame tree, world) may take:
/// the shared browser can pause for seconds. Never past the deadline
/// (`budget`). Lookups use the shared LOOKUP_STEP_TIMEOUT per step.
pub const CALL_SLACK: f64 = 15.0;
/// Shortest timeout a step gets near the deadline: it fails fast, the
/// stall is noted and the deadline ends the wait.
pub const MIN_STEP: f64 = 0.05;
/// Cleanup (dialog barrier, detach) may run this long past the deadline,
/// so the whole wait ends within `--timeout` + CLEANUP_GRACE.
pub const CLEANUP_GRACE: f64 = 1.0;
/// Slack on top of the heartbeat for one check. Past it the check is
/// reported as a stall and asked again on the same session; the late
/// answer is ignored. Only a notice: a stall never ends the wait.
pub const CHECK_SLACK: f64 = 5.0;
/// Isolated world name. Scoped to our session; gone when it detaches.
pub const WORLD: &str = "agent-display-wait";
/// Pause between reconnect attempts while the browser is stalled.
pub const BACKOFF: [f64; 5] = [0.5, 1.0, 2.0, 4.0, 5.0];
/// After a tab disappears: how long to tell `tab close` (the browser keeps
/// answering) from `browser stop` (Browser.close closes every target, then
/// the unit stops).
pub const SETTLE_PAUSE: f64 = 0.5;
/// `/json/version` probe while settling.
pub const SETTLE_PROBE: f64 = 5.0;
pub const SETTLE_POLLS: u32 = 20;
/// Navigations mid-check can leave no document to put a world in yet.
const WORLD_RETRIES: u32 = 50;

/// Runs in the isolated world. Returns `{state, key}` at once if the state
/// differs from `last` (a key it returned before), else waits for a change
/// or the tick. `__USER_JS__` is replaced by the `--js` expression wrapped
/// in a function (compiled with the rest, so page CSP can't block it), or
/// `null`.
pub const CHECK_FN: &str = r#"async function (S, last, tickMs) {
  const userJs = __USER_JS__;
  const re = (s) => {
    const m = /^\/(.*)\/([a-z]*)$/s.exec(s);
    return m ? new RegExp(m[1], m[2].replace(/[gy]/g, '')) : new RegExp(s);
  };
  const R = {};
  for (const k of ['url', 'url_not', 'title', 'text', 'text_not']) if (S[k] != null) R[k] = re(S[k]);
  const check = () => {
    const href = location.href, title = document.title;
    let text;
    const t = () => (text ??= (document.body ? document.body.innerText : (document.documentElement?.textContent ?? '')));
    const m = {};
    if (R.url) m.url = R.url.test(href);
    if (R.url_not) m.url_not = !R.url_not.test(href);
    if (R.title) m.title = R.title.test(title);
    if (R.text) m.text = R.text.test(t());
    if (R.text_not) m.text_not = !R.text_not.test(t());
    if (S.selector != null) m.selector = document.querySelector(S.selector) !== null;
    if (S.selector_not != null) m.selector_not = document.querySelector(S.selector_not) === null;
    if (userJs) m.js = !!userJs();
    const state = { url: href, title, matched: m };
    return { state, key: JSON.stringify(state) };
  };
  const now = check();
  if (now.key !== last) return now;
  return await new Promise((resolve, reject) => {
    let pending = 0, tick = 0;
    const done = (r) => {
      mo.disconnect(); clearTimeout(pending); clearTimeout(tick);
      removeEventListener('hashchange', poke); removeEventListener('popstate', poke);
      document.removeEventListener('readystatechange', poke);
      resolve(r);
    };
    const safe = (f) => () => { try { f(); } catch (e) { mo.disconnect(); clearTimeout(pending); clearTimeout(tick); reject(e); } };
    // Throttled, not debounced: a page that mutates constantly still gets
    // checked every 100 ms.
    const poke = () => {
      if (pending) return;
      pending = setTimeout(safe(() => { pending = 0; const r = check(); if (r.key !== last) done(r); }), 100);
    };
    const mo = new MutationObserver(poke);
    mo.observe(document, { subtree: true, childList: true, characterData: true, attributes: true });
    addEventListener('hashchange', poke); addEventListener('popstate', poke);
    document.addEventListener('readystatechange', poke);
    tick = setTimeout(safe(() => done(check())), tickMs);
  });
}"#;

/// CHECK_FN with the `--js` expression compiled in.
pub fn check_fn(js: Option<&str>) -> String {
    let user = match js {
        Some(js) => format!("(() => (\n{js}\n))"),
        None => "null".into(),
    };
    CHECK_FN.replace("__USER_JS__", &user)
}

#[derive(Debug)]
enum Outcome {
    Matched(Value),
    /// Evidence the tab is gone: "closed" or "crashed".
    Gone(&'static str),
    BrowserDown(String),
    Timeout,
    /// Not conclusive: reconnect after a backoff.
    Stall(Error),
    /// Our session detached: look the tab up again now (closed, or a
    /// spurious detach); not a stall.
    Recheck,
}

struct Waiter<'a, 'c> {
    ctx: &'a Ctx<'c>,
    tab: String,
    spec: WaitSpec,
    /// `--show-url`: print url and title in full.
    show_url: bool,
    start: Instant,
    deadline: Option<Instant>,
    port: Option<u16>,
    unit: Option<String>,
    last: Option<Value>,
    last_key: String,
    match_since: Option<Instant>,
    stalled: Option<String>,
    backoff: usize,
    /// Rechecks in a row without a page state in between (a detach that
    /// keeps happening is paced like a stall).
    rechecks: u32,
    /// This round's session is known to be gone (it detached).
    detached: bool,
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.clamp(0.0, 1e9))
}

fn is_context_gone(msg: &str) -> bool {
    [
        "Cannot find context with specified id",
        "Execution context was destroyed",
        "Inspected target navigated or closed",
        "Cannot find default execution context",
    ]
    .iter()
    .any(|s| msg.contains(s))
}

impl<'a, 'c> Waiter<'a, 'c> {
    fn now(&self) -> Instant {
        self.ctx.sys.now()
    }

    fn elapsed(&self) -> f64 {
        (self.now().saturating_duration_since(self.start).as_secs_f64() * 10.0).round() / 10.0
    }

    fn left(&self) -> Option<f64> {
        let now = self.now();
        self.deadline.map(|d| d.saturating_duration_since(now).as_secs_f64())
    }

    fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| self.now() >= d)
    }

    /// Seconds until `extra` s past the deadline (None: no deadline).
    fn until(&self, extra: f64) -> Option<f64> {
        let now = self.now();
        self.deadline.map(|d| (d + secs(extra)).saturating_duration_since(now).as_secs_f64())
    }

    /// Timeout for one blocking step: `t`, but never more than `grace` s
    /// past the deadline (at least MIN_STEP). Steps of the wait itself use
    /// grace 0 (`budget`); a check uses half CLEANUP_GRACE (its tick ends by
    /// the deadline, so the page answers by then, and the slack keeps that
    /// answer from racing it); cleanup uses CLEANUP_GRACE.
    fn within(&self, t: f64, grace: f64) -> f64 {
        self.until(grace).map_or(t, |l| t.min(l).max(MIN_STEP))
    }

    fn budget(&self, t: f64) -> f64 {
        self.within(t, 0.0)
    }

    /// Sleep `t` s, or until the deadline if that is sooner.
    fn pause(&self, t: f64) {
        self.ctx.sys.sleep(self.until(0.0).map_or(t, |l| t.min(l)));
    }

    /// Every line `tab wait` prints goes through here. `url` and `title`
    /// are the only page-derived values: unless --show-url, the URL is
    /// redacted like every `tab` command's (a one-time sign-in link is often
    /// exactly what a wait watches), and so is a title that is URL-like.
    fn print(&self, m: Map<String, Value>) {
        let mut v = Value::Object(m);
        if !self.show_url {
            let url = v.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(Value::String(t)) = v.get_mut("title") {
                *t = crate::redact::redact_title(t, &url);
            }
            crate::redact::redact_value(&mut v, false);
        }
        self.ctx.println(&json::dumps(&v, false));
    }

    /// url, title and matched of a page state (or nulls).
    fn page_fields(state: Option<&Value>, m: &mut Map<String, Value>) {
        for k in ["url", "title", "matched"] {
            m.insert(k.into(), state.and_then(|s| s.get(k)).cloned().unwrap_or(Value::Null));
        }
    }

    fn emit(&self, event: &str, extra: Map<String, Value>) {
        if !self.spec.follow {
            return;
        }
        let mut m = Map::new();
        m.insert("event".into(), event.into());
        m.insert("tab".into(), self.tab.clone().into());
        m.insert("t".into(), json!(self.elapsed()));
        m.extend(extra);
        self.print(m);
    }

    fn stall(&mut self, why: &str) {
        if self.stalled.is_none() {
            let mut m = Map::new();
            m.insert("error".into(), why.into());
            self.emit("stall", m);
        }
        self.stalled = Some(why.to_string());
    }

    fn recovered(&mut self) {
        self.backoff = 0;
        if self.stalled.take().is_some() {
            self.emit("resumed", Map::new());
        }
    }

    fn dialogs(&mut self, cdp: &mut dyn CdpConn) {
        for d in cdp.take_dialogs() {
            let mut m = Map::new();
            m.insert("type".into(), d.kind.clone().into());
            self.emit("dialog", m.clone());
            m.insert("handled".into(), (if d.accepted { "accepted" } else { "dismissed" }).into());
            self.emit("dialog-closed", m);
        }
    }

    /// A state from the page: report changes, track the condition.
    fn observe(&mut self, state: Value, key: String) -> Option<Value> {
        if key != self.last_key {
            let mut m = Map::new();
            Self::page_fields(Some(&state), &mut m);
            self.emit("state", m);
            self.last_key = key;
        }
        let conds: Vec<bool> =
            state["matched"].as_object().map(|m| m.values().map(|v| v == &Value::Bool(true)).collect()).unwrap_or_default();
        let ok = !conds.is_empty() && if self.spec.any { conds.iter().any(|b| *b) } else { conds.iter().all(|b| *b) };
        self.last = Some(state.clone());
        self.rechecks = 0;
        if !ok {
            self.match_since = None;
            return None;
        }
        let now = self.now();
        let since = *self.match_since.get_or_insert(now);
        let held = now.saturating_duration_since(since).as_secs_f64() * 1000.0;
        (held >= self.spec.stable_ms as f64).then_some(state)
    }

    /// Heartbeat for the next check: shorter while a match must hold, and
    /// never past the deadline.
    fn tick_ms(&self) -> i64 {
        let mut t = self.spec.tick_ms.max(50);
        if let Some(since) = self.match_since {
            let held = self.now().saturating_duration_since(since).as_millis() as i64;
            t = t.min((self.spec.stable_ms - held).max(50));
        }
        if let Some(left) = self.left() {
            t = t.min(((left * 1000.0) as i64).max(1));
        }
        t
    }

    /// The browsers this wait may find the tab in: the one it was found in
    /// (or `--port`'s), else all registered ones.
    fn scope(&self) -> Option<i64> {
        self.port.map(i64::from).or(self.spec.port)
    }

    /// Why the tab's browser is down, or None. Down means its registration
    /// is gone (dropped only once systemd confirms the unit stopped) or
    /// systemd confirms the unit stopped; an unknown state (systemctl
    /// failing or timing out, a unit mid-transition) is not down.
    fn units_down(&self) -> Option<String> {
        let owned = owned_ports(self.ctx);
        let units: Vec<String> = match &self.unit {
            Some(u) => vec![u.clone()],
            None => owned.iter().filter(|(p, _)| self.scope().is_none_or(|q| q == *p)).map(|(_, u)| u.clone()).collect(),
        };
        if units.is_empty() {
            return self.no_browser();
        }
        let registered: Vec<String> = units.iter().filter(|u| owned.iter().any(|(_, o)| o == *u)).cloned().collect();
        let states = self.ctx.sys.units_active(self.ctx, &registered);
        // Unregistered: dropped once systemd confirmed the unit stopped.
        let down = states.iter().all(|s| *s == Some(false));
        down.then(|| format!("{} is not running", units.join(", ")))
    }

    /// Look the tab up (the shared lookup), then watch it until something
    /// conclusive happens.
    fn round(&mut self) -> Result<Outcome> {
        // Nothing registered to look in: the browser went away.
        if let Some(why) = self.no_browser() {
            return Ok(Outcome::BrowserDown(why));
        }
        let found = match locate_why(self.ctx, &self.tab, self.scope(), self.budget(LOOKUP_STEP_TIMEOUT)) {
            Ok(Some(f)) => f,
            // Two definitive misses ~250 ms apart (settle_gone tells a
            // closed tab from a browser that went away).
            Ok(None) => return Ok(Outcome::Gone("closed")),
            Err(why) => {
                if let Some(down) = self.units_down() {
                    return Ok(Outcome::BrowserDown(down));
                }
                return Ok(Outcome::Stall(Error::new(format!("no definitive answer: {why}"))));
            }
        };
        let Found { port, info, mut cdp } = found;
        let kind = info.get("type").and_then(Value::as_str).unwrap_or("None");
        if kind != "page" {
            cdp.close();
            bail!("{} is a {kind}, not a tab", self.tab);
        }
        self.port = Some(port);
        if self.unit.is_none() {
            self.unit = owned_ports(self.ctx).into_iter().find(|(p, _)| *p == port as i64).map(|(_, u)| u);
        }
        let r = self.watch(cdp.as_mut());
        cdp.close();
        r
    }

    /// Why there is no browser to look in (None if there is one).
    fn no_browser(&self) -> Option<String> {
        let scope = self.scope();
        if owned_ports(self.ctx).iter().any(|(p, _)| scope.is_none_or(|q| q == *p)) {
            return None;
        }
        Some(match (&self.unit, scope) {
            (Some(u), _) => format!("{u} is not running"),
            (None, Some(p)) => format!("no agent-display browser on port {p} (stopped?)"),
            (None, None) => "no agent-display browser is running".into(),
        })
    }

    fn watch(&mut self, cdp: &mut dyn CdpConn) -> Result<Outcome> {
        let attach = json!({"targetId": self.tab, "flatten": true});
        let session = match cdp.call("Target.attachToTarget", attach, None, Some(self.budget(CALL_SLACK))) {
            Ok(r) => r["sessionId"].as_str().unwrap_or("").to_string(),
            Err(e) => return Ok(Outcome::Stall(e)),
        };
        cdp.set_dialog_policy(self.spec.dialog_accept);
        self.detached = false;
        let r = self.watch_session(cdp, &session);
        // Unless the session is known to be gone (it detached, the tab
        // closed or crashed, the browser went down), end it without losing
        // a dialog, whatever ended the round: a stall reconnects, and a
        // dialog left unanswered on this session would wedge the tab for
        // the next round and every other client. Errors are ignored and
        // the time is capped, so on a dead session this is cheap.
        let gone = self.detached || matches!(r, Ok(Outcome::Gone(_) | Outcome::BrowserDown(_)));
        if !gone {
            let limit = self.within(DIALOG_BARRIER + DETACH_TIMEOUT, CLEANUP_GRACE);
            detach_answering_dialogs(self.ctx, cdp, &session, limit);
            self.dialogs(cdp);
        }
        r
    }

    fn watch_session(&mut self, cdp: &mut dyn CdpConn, session: &str) -> Result<Outcome> {
        let s = Some(session);
        let setup = (|| -> Result<String> {
            // Page events report dialogs; Inspector reports crashes.
            cdp.call("Page.enable", json!({}), s, Some(self.budget(CALL_SLACK)))?;
            cdp.call("Inspector.enable", json!({}), s, Some(self.budget(CALL_SLACK)))?;
            let tree = cdp.call("Page.getFrameTree", json!({}), s, Some(self.budget(CALL_SLACK)))?;
            Ok(tree["frameTree"]["frame"]["id"].as_str().unwrap_or(&self.tab).to_string())
        })();
        let frame = match setup {
            Ok(f) => f,
            Err(e) if e.msg.contains(DETACHED) => return Ok(self.session_gone()),
            Err(e) => return Ok(Outcome::Stall(e)),
        };
        self.recovered();
        // --js is compiled into the function instead (see check_fn).
        let mut spec_js = self.spec.page_spec();
        if let Some(m) = spec_js.as_object_mut() {
            m.remove("js");
        }
        let function = check_fn(self.spec.js.as_deref());
        let mut world: Option<i64> = None;
        let mut world_failures = 0u32;
        loop {
            self.dialogs(cdp);
            for ev in cdp.take_events() {
                if let Some(o) = self.event(&ev, session) {
                    return Ok(match o {
                        Outcome::Recheck => self.session_gone(),
                        o => o,
                    });
                }
            }
            if self.expired() {
                return Ok(Outcome::Timeout);
            }
            let ctx_id = match world {
                Some(id) => id,
                None => match cdp.call(
                    "Page.createIsolatedWorld",
                    json!({"frameId": frame, "worldName": WORLD, "grantUniveralAccess": false}),
                    s,
                    Some(self.budget(CALL_SLACK)),
                ) {
                    Ok(r) => {
                        let id = r["executionContextId"].as_i64().unwrap_or(0);
                        world = Some(id);
                        id
                    }
                    Err(e) if e.msg.contains(DETACHED) => return Ok(self.session_gone()),
                    Err(e) if e.is_timeout() => {
                        self.stall(&e.msg);
                        continue;
                    }
                    Err(e) => {
                        world_failures += 1;
                        if world_failures > WORLD_RETRIES {
                            return Ok(Outcome::Stall(e));
                        }
                        self.ctx.sys.sleep(0.1);
                        continue;
                    }
                },
            };
            let tick = self.tick_ms();
            let r = cdp.call(
                "Runtime.callFunctionOn",
                json!({
                    "functionDeclaration": function,
                    "executionContextId": ctx_id,
                    "arguments": [{"value": spec_js}, {"value": self.last_key}, {"value": tick}],
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
                s,
                Some(self.within(tick as f64 / 1000.0 + CHECK_SLACK, CLEANUP_GRACE / 2.0)),
            );
            // Dialogs answered while the check ran came before its result.
            self.dialogs(cdp);
            match r {
                Ok(res) => {
                    if let Some(d) = res.get("exceptionDetails") {
                        let text = exception_text(d).lines().next().unwrap_or("exception").to_string();
                        if is_context_gone(&text) {
                            world = None;
                            continue;
                        }
                        bail!("tab wait: the condition threw: {text}");
                    }
                    world_failures = 0;
                    self.recovered();
                    let v = &res["result"]["value"];
                    let key = v["key"].as_str().unwrap_or("").to_string();
                    if let Some(state) = self.observe(v["state"].clone(), key) {
                        return Ok(Outcome::Matched(state));
                    }
                }
                Err(e) if e.msg.contains(DETACHED) => return Ok(self.session_gone()),
                Err(e) if is_context_gone(&e.msg) => world = None,
                Err(e) if e.is_timeout() => {
                    // Renderer or browser busy. Not evidence of anything;
                    // keep the session and ask again.
                    self.stall(&e.msg);
                }
                Err(e) => return Ok(Outcome::Stall(e)),
            }
        }
    }

    /// Session events that end this round: crashes and detaches.
    fn event(&mut self, ev: &Value, session: &str) -> Option<Outcome> {
        let method = ev["method"].as_str().unwrap_or("");
        let ours = ev.get("sessionId").and_then(Value::as_str) == Some(session);
        match method {
            "Inspector.targetCrashed" if ours => Some(Outcome::Gone("crashed")),
            "Inspector.detached" if ours => Some(Outcome::Recheck),
            "Target.detachedFromTarget" if ev["params"]["sessionId"].as_str() == Some(session) => Some(Outcome::Recheck),
            _ => None,
        }
    }

    /// Our session detached: the tab closed, or (rarely) only the session
    /// went. The next round's lookup decides (two definitive misses ~250 ms
    /// apart, or found and reattached).
    fn session_gone(&mut self) -> Outcome {
        self.detached = true;
        Outcome::Recheck
    }

    /// The tab is gone. `browser stop` (Browser.close) closes every target
    /// before the browser exits, so check the browser outlived it. Bounded
    /// by the deadline: once it passes, the unit's state decides (one
    /// systemctl call, no CDP).
    fn settle_gone(&mut self, reason: &'static str) -> Outcome {
        let Some(port) = self.port else { return Outcome::Gone(reason) };
        self.pause(SETTLE_PAUSE);
        if !self.expired() && self.ctx.sys.cdp_version(port, self.budget(SETTLE_PROBE)).is_ok() {
            return Outcome::Gone(reason);
        }
        for _ in 0..SETTLE_POLLS {
            if let Some(why) = self.units_down() {
                return Outcome::BrowserDown(why);
            }
            if self.expired() {
                break;
            }
            self.pause(SETTLE_PAUSE);
        }
        Outcome::Gone(reason)
    }

    fn finish(&self, result: &str, state: Option<&Value>, extra: Map<String, Value>) {
        let mut m = Map::new();
        m.insert("tab".into(), self.tab.clone().into());
        m.insert("result".into(), result.into());
        Self::page_fields(state.or(self.last.as_ref()), &mut m);
        m.insert("elapsed".into(), json!(self.elapsed()));
        m.extend(extra);
        self.print(m);
    }
}

fn one(k: &str, v: impl Into<Value>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(k.into(), v.into());
    m
}

/// Option combinations are checked by the parser (`cli::check_wait`, exit 2).
pub fn cmd_tab_wait(ctx: &Ctx, o: &TabOpts, tab: &str, spec: &WaitSpec) -> Result<i32> {
    if !valid_tab_id(tab) {
        bail!("invalid tab id {}", crate::util::py_repr(tab));
    }
    let start = ctx.sys.now();
    let mut spec = spec.clone();
    spec.port = o.port_raw;
    let mut w = Waiter {
        ctx,
        tab: tab.to_string(),
        spec,
        show_url: o.show_url,
        start,
        deadline: (o.timeout > 0.0).then(|| start + secs(o.timeout)),
        port: None,
        unit: None,
        last: None,
        last_key: String::new(),
        match_since: None,
        stalled: None,
        backoff: 0,
        rechecks: 0,
        detached: false,
    };
    // An explicit --port is checked once, as by the other tab commands: a
    // foreign, reserved or hijacked port is refused (exit 1, never retried);
    // nothing registered or listening there is a browser that went away.
    // Whether it answers is ridden out below.
    let first = match o.port_raw.map(|p| explicit_port(ctx, p, false)) {
        Some(Err(e)) if e.code == exit::BROWSER_DOWN => Some(Outcome::BrowserDown(e.msg)),
        Some(Err(e)) => return Err(e),
        _ => None,
    };
    let mut first = first;
    loop {
        let outcome = match first.take() {
            Some(o) => o,
            None if w.expired() => Outcome::Timeout,
            None => w.round()?,
        };
        let outcome = match outcome {
            Outcome::Gone(reason) => w.settle_gone(reason),
            o => o,
        };
        match outcome {
            Outcome::Matched(state) => {
                w.finish("matched", Some(&state), Map::new());
                return Ok(0);
            }
            Outcome::Gone(reason) => {
                let code = if w.spec.gone { 0 } else { exit::GONE };
                w.finish(if w.spec.gone { "matched" } else { "gone" }, None, one("reason", reason));
                return Ok(code);
            }
            Outcome::BrowserDown(why) => {
                // The tab went with its browser: that is what --gone asked.
                if w.spec.gone {
                    let mut m = one("reason", "browser-down");
                    m.insert("detail".into(), why.into());
                    w.finish("matched", None, m);
                    return Ok(0);
                }
                w.finish("browser-down", None, one("reason", why));
                return Ok(exit::BROWSER_DOWN);
            }
            Outcome::Timeout => {
                // Stalled at the deadline: the page's state is unknown, so
                // "no definitive answer, try again" (75), not "it didn't
                // happen" (3).
                let stalled = w.stalled.clone();
                let mut m = one("stalled", stalled.clone().map(Value::from).unwrap_or(Value::Null));
                m.insert("timeout".into(), json!(o.timeout));
                if stalled.is_some() {
                    w.finish("stalled", None, m);
                    return Ok(exit::TEMPFAIL);
                }
                w.finish("timeout", None, m);
                return Ok(exit::TIMEOUT);
            }
            Outcome::Recheck if w.rechecks == 0 => w.rechecks += 1,
            Outcome::Recheck | Outcome::Stall(_) => {
                let e = match outcome {
                    Outcome::Stall(e) => e,
                    _ => Error::new("the tab's session keeps detaching; reattaching"),
                };
                w.stall(&crate::tab::cdp_text(&e));
                let mut pause = BACKOFF[w.backoff.min(BACKOFF.len() - 1)];
                if let Some(left) = w.left() {
                    pause = pause.min(left);
                }
                w.backoff += 1;
                ctx.sys.sleep(pause);
            }
        }
    }
}
