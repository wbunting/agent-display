//! `tab type`, `tab click`, `tab snapshot`: input into one tab over CDP.
//!
//! Elements are found the way a person names them, from Chromium's
//! accessibility tree (role, and the accessible name from `<label>`,
//! `aria-label`, placeholder, title or button text), not by CSS. Every
//! frame is searched: same-process iframes through their own AX trees,
//! cross-origin (out-of-process) iframes through auto-attached sessions.
//!
//! Secrets: `tab type --stdin` reads the value from stdin. It never goes to
//! argv, the environment, stdout, stderr or an error message, and only
//! leaves this process in `Input.insertText`/`Input.dispatchKeyEvent`.
//! Fields are checked by length, never by reading their value back.
//!
//! Tabs from `tab open` sit in windows without OS focus, so
//! `document.hasFocus()` is false. Input commands turn on
//! `Emulation.setFocusEmulationEnabled` while they run (what Playwright
//! does) and switch it off again when the tab session drops.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::browser::OWNERSHIP_TIMEOUT;
use crate::cli::TabOpts;
use crate::ctx::Ctx;
use crate::redact::{Secret, redact_url};
use crate::tab::Tab;
use crate::util::{Error, Result};
use crate::{bail, json};

/// How long `type`/`click` wait for their element to be ready.
pub const ELEMENT_TIMEOUT: f64 = 10.0;
/// Poll interval while a click target settles (cheap: quads and a hit test).
const POLL: f64 = 0.1;
/// Element lookups re-read every frame's accessibility tree, so their poll
/// backs off: 100 ms, doubling up to 1 s.
const FIND_POLL: (f64, f64) = (0.1, 1.0);
/// Out-of-process iframes nested deeper than this are not searched.
const MAX_FRAME_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Locator {
    pub css: Option<String>,
    pub label: Option<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub r#ref: Option<String>,
    pub focused: bool,
    /// 1-based index among the visible matches, in page order.
    pub nth: Option<i64>,
}

impl Locator {
    fn kinds(&self) -> usize {
        [self.css.is_some(), self.label.is_some(), self.role.is_some(), self.text.is_some(), self.r#ref.is_some(), self.focused]
            .iter()
            .filter(|b| **b)
            .count()
    }

    /// Usage problem (exit 2), if any.
    pub fn usage_error(&self, with_text: bool) -> Option<String> {
        let flags = if with_text { "--label, --role, --text, --css, --ref or --focused" } else { "--label, --role, --css, --ref or --focused" };
        if self.kinds() != 1 {
            return Some(format!("give exactly one of {flags}"));
        }
        if self.name.is_some() && self.role.is_none() {
            return Some("--name needs --role".into());
        }
        if self.nth.is_some_and(|n| n < 1) {
            return Some("--nth counts from 1".into());
        }
        if self.nth.is_some() && (self.r#ref.is_some() || self.focused) {
            return Some("--nth goes with --label, --role, --text or --css".into());
        }
        if let Some(r) = &self.r#ref
            && parse_ref(r).is_none()
        {
            return Some(format!("bad ref {r:?}; refs look like k3fq.12 (from `tab snapshot`)"));
        }
        None
    }

    fn describe(&self) -> String {
        if let Some(c) = &self.css {
            return format!("css {c:?}");
        }
        if let Some(l) = &self.label {
            return format!("label {l:?}");
        }
        if let Some(r) = &self.role {
            return match &self.name {
                Some(n) => format!("role {r} named {n:?}"),
                None => format!("role {r}"),
            };
        }
        if let Some(t) = &self.text {
            return format!("text {t:?}");
        }
        if let Some(r) = &self.r#ref {
            return format!("ref {r}");
        }
        "the focused element".into()
    }
}

/// Roles a `--label` can name: things you type into or set.
const FIELD_ROLES: &[&str] = &[
    "textbox", "searchbox", "combobox", "spinbutton", "TextField", "checkbox", "radio", "switch", "slider", "listbox",
    "date", "DateTime", "InputTime",
];
/// Roles `--text` prefers: things you click.
const CLICK_ROLES: &[&str] = &[
    "button", "link", "menuitem", "menuitemcheckbox", "menuitemradio", "tab", "checkbox", "radio", "switch", "option",
    "treeitem", "PopUpButton", "ToggleButton", "summary", "DisclosureTriangle",
];
/// Text-entry roles: `snapshot` prints [filled]/[empty] and never their
/// descendants (Chromium exposes the value as their text).
const VALUE_ROLES: &[&str] = &["textbox", "searchbox", "combobox", "spinbutton", "TextField"];
const TEXT_ROLES: &[&str] = &["StaticText", "InlineTextBox", "LineBreak"];

/// `<tag>.<backend node id>`, tag = 4 base32 chars naming the document.
pub fn parse_ref(r: &str) -> Option<(String, i64)> {
    let r = r.trim_start_matches('@');
    let r = r.strip_prefix("ref=").unwrap_or(r);
    let (tag, n) = r.split_once('.')?;
    let ok_tag = tag.len() == 4 && tag.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b));
    let n: i64 = n.parse().ok().filter(|n| *n > 0)?;
    ok_tag.then(|| (tag.to_string(), n))
}

/// Document tag for refs: changes whenever the frame loads a new document
/// (new loaderId), so refs from an old page never hit a node of the new one
/// (backend node ids restart in a new renderer process).
pub fn doc_tag(frame_id: &str, loader_id: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in frame_id.bytes().chain([0]).chain(loader_id.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    (0..4).map(|i| A[((h >> (i * 5)) & 31) as usize] as char).collect()
}

fn trunc(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn ax_str(n: &Value, k: &str) -> String {
    match n.get(k).and_then(|v| v.get("value")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

// ------------------------------------------------------------ page model

/// One CDP session's documents: the page itself, or an out-of-process
/// iframe. `frames` are the frames rendered in that process (root first).
#[derive(Debug, Clone)]
struct Doc {
    session: String,
    root: String,
    frames: Vec<String>,
    tag: String,
    parent: Option<usize>,
    depth: usize,
    alive: bool,
}

#[derive(Debug, Clone)]
struct Cand {
    doc: usize,
    frame: String,
    backend: i64,
    role: String,
    name: String,
}

struct Page {
    docs: Vec<Doc>,
    attached: HashSet<String>,
    /// frame id -> origin, for reporting which iframe an element is in.
    origins: HashMap<String, String>,
}

impl Page {
    fn new(t: &mut Tab) -> Result<Page> {
        let main = t.session().to_string();
        let mut p = Page {
            docs: vec![Doc { session: main, root: String::new(), frames: vec![], tag: String::new(), parent: None, depth: 0, alive: true }],
            attached: HashSet::new(),
            origins: HashMap::new(),
        };
        p.update(t)?;
        Ok(p)
    }

    fn main_frame(&self) -> &str {
        &self.docs[0].root
    }

    /// Re-read every session's frame tree and pick up new out-of-process
    /// iframes. The page's own session must work; iframe sessions that went
    /// away are marked dead.
    fn update(&mut self, t: &mut Tab) -> Result<()> {
        let mut i = 0;
        while i < self.docs.len() {
            if !self.docs[i].alive {
                i += 1;
                continue;
            }
            let sess = self.docs[i].session.clone();
            match on(t, &sess, |t| t.call("Page.getFrameTree", json!({}), None)) {
                Ok(r) => {
                    let tree = &r["frameTree"];
                    let root = tree["frame"]["id"].as_str().unwrap_or("").to_string();
                    let loader = tree["frame"]["loaderId"].as_str().unwrap_or("");
                    let mut frames = Vec::new();
                    collect_frames(tree, &mut frames, &mut self.origins);
                    let d = &mut self.docs[i];
                    d.tag = doc_tag(&root, loader);
                    d.root = root;
                    d.frames = frames;
                }
                Err(e) if i == 0 => return Err(e),
                Err(_) => {
                    self.docs[i].alive = false;
                    i += 1;
                    continue;
                }
            }
            if self.docs[i].depth < MAX_FRAME_DEPTH {
                if !self.attached.contains(&sess) {
                    let p = json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true});
                    if on(t, &sess, |t| t.call("Target.setAutoAttach", p, Some(5.0))).is_ok() {
                        self.attached.insert(sess.clone());
                    }
                }
                // Chromium sends attachedToTarget for existing iframes before
                // it answers setAutoAttach, and later ones as they appear.
                while let Ok(ev) = on(t, &sess, |t| t.wait("Target.attachedToTarget", Some(0.0))) {
                    let p = &ev["params"];
                    let (Some(child), Some(target)) = (p["sessionId"].as_str(), p["targetInfo"]["targetId"].as_str()) else {
                        continue;
                    };
                    if p["targetInfo"]["type"] != "iframe" || self.docs.iter().any(|d| d.session == child) {
                        continue;
                    }
                    let depth = self.docs[i].depth + 1;
                    self.docs.push(Doc {
                        session: child.to_string(),
                        root: target.to_string(),
                        frames: vec![],
                        tag: String::new(),
                        parent: Some(i),
                        depth,
                        alive: true,
                    });
                }
            }
            i += 1;
        }
        Ok(())
    }

    fn cand_json(&self, c: &Cand) -> Value {
        let mut m = Map::new();
        m.insert("role".into(), c.role.clone().into());
        m.insert("name".into(), trunc(&c.name, 80).into());
        m.insert("ref".into(), format!("{}.{}", self.docs[c.doc].tag, c.backend).into());
        if c.frame != self.main_frame() {
            let origin = self.origins.get(&c.frame).cloned().unwrap_or_else(|| "iframe".into());
            m.insert("frame".into(), origin.into());
        }
        Value::Object(m)
    }

    /// Every frame's AX nodes: (doc, frame, nodes).
    fn ax_trees(&self, t: &mut Tab) -> Vec<(usize, String, Vec<Value>)> {
        let mut out = Vec::new();
        for (i, d) in self.docs.iter().enumerate().filter(|(_, d)| d.alive) {
            for f in &d.frames {
                let r = on(t, &d.session, |t| t.call("Accessibility.getFullAXTree", json!({"frameId": f}), None));
                if let Ok(r) = r {
                    out.push((i, f.clone(), r["nodes"].as_array().cloned().unwrap_or_default()));
                }
            }
        }
        out
    }

    /// Owner element of every child frame: (doc, owner backend) -> (doc, frame).
    fn frame_owners(&self, t: &mut Tab) -> HashMap<(usize, i64), (usize, String)> {
        let mut out = HashMap::new();
        for (i, d) in self.docs.iter().enumerate().filter(|(_, d)| d.alive) {
            for f in d.frames.iter().skip(1) {
                if let Ok(b) = owner_backend(t, &d.session, f) {
                    out.insert((i, b), (i, f.clone()));
                }
            }
            if let Some(p) = d.parent
                && let Ok(b) = owner_backend(t, &self.docs[p].session, &d.root)
            {
                out.insert((p, b), (i, d.root.clone()));
            }
        }
        out
    }
}

fn collect_frames(tree: &Value, out: &mut Vec<String>, origins: &mut HashMap<String, String>) {
    if let Some(id) = tree["frame"]["id"].as_str() {
        out.push(id.to_string());
        let origin = tree["frame"]["securityOrigin"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| {
            let u = tree["frame"]["url"].as_str().unwrap_or("");
            match u.split_once("://") {
                Some((s, rest)) => format!("{s}://{}", rest.split(['/', '?', '#']).next().unwrap_or("")),
                None => redact_url(u),
            }
        });
        origins.insert(id.to_string(), origin);
    }
    for c in tree["childFrames"].as_array().into_iter().flatten() {
        collect_frames(c, out, origins);
    }
}

/// Run `f` with the tab's calls going to `session`.
fn on<T>(t: &mut Tab, session: &str, f: impl FnOnce(&mut Tab) -> Result<T>) -> Result<T> {
    if t.session() == session {
        return f(t);
    }
    let prev = t.swap_session(session.to_string());
    let r = f(t);
    t.swap_session(prev);
    r
}

fn owner_backend(t: &mut Tab, session: &str, frame: &str) -> Result<i64> {
    let r = on(t, session, |t| t.call("DOM.getFrameOwner", json!({"frameId": frame}), Some(5.0)))?;
    r["backendNodeId"].as_i64().ok_or_else(|| Error::new("DOM.getFrameOwner: no backendNodeId"))
}

fn cands_of(trees: &[(usize, String, Vec<Value>)]) -> Vec<Cand> {
    let mut out = Vec::new();
    for (doc, frame, nodes) in trees {
        for n in nodes {
            if n.get("ignored") == Some(&Value::Bool(true)) {
                continue;
            }
            let Some(backend) = n.get("backendDOMNodeId").and_then(Value::as_i64) else { continue };
            out.push(Cand { doc: *doc, frame: frame.clone(), backend, role: ax_str(n, "role"), name: ax_str(n, "name") });
        }
    }
    out
}

/// Matches in order of preference: exact name, then the same name ignoring
/// case and spacing, then names containing it.
fn name_tiers(all: &[Cand], want: &str, role_ok: impl Fn(&str) -> bool) -> Vec<Vec<Cand>> {
    let w = norm(want);
    let pool: Vec<&Cand> = all.iter().filter(|c| role_ok(&c.role)).collect();
    vec![
        pool.iter().filter(|c| c.name == want).map(|c| (*c).clone()).collect(),
        pool.iter().filter(|c| c.name != want && norm(&c.name) == w).map(|c| (*c).clone()).collect(),
        pool.iter().filter(|c| !w.is_empty() && norm(&c.name) != w && norm(&c.name).contains(&w)).map(|c| (*c).clone()).collect(),
    ]
}

/// Deepest active element, through shadow roots and same-origin frames.
const ACTIVE_FN: &str = "(() => { let a = document.activeElement; \
    for (;;) { if (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement; \
    else if (a && a.contentDocument && a.contentDocument.activeElement) a = a.contentDocument.activeElement; \
    else break; } return a && a !== document.body && a !== document.documentElement ? a : null; })()";

/// Field kind only: the value itself never leaves the page.
const FIELD_INFO_FN: &str = "function field_info() { const e = this; \
    const tag = (e.localName || '').toLowerCase(); const type = ((e.getAttribute && e.getAttribute('type')) || '').toLowerCase(); \
    const ac = ((e.getAttribute && e.getAttribute('autocomplete')) || '').toLowerCase().split(/\\s+/); \
    const text = tag === 'textarea' || (tag === 'input' && !['button','submit','reset','checkbox','radio','file','image','hidden','range','color'].includes(type)); \
    const secret = (tag === 'input' && type === 'password') || ac.some(t => ['current-password','new-password','one-time-code','cc-number','cc-csc'].includes(t)); \
    return { tag, type, secret, editable: text || !!e.isContentEditable, \
      disabled: !!e.disabled || (e.getAttribute && e.getAttribute('aria-disabled') === 'true'), readonly: !!e.readOnly }; }";

const IS_ACTIVE_FN: &str = "function is_active() { let a = this.ownerDocument.activeElement; \
    while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement; return a === this; }";

const SELECT_ALL_FN: &str = "function select_all() { if (typeof this.select === 'function') { this.select(); return; } \
    const r = this.ownerDocument.createRange(); r.selectNodeContents(this); const s = this.ownerDocument.getSelection(); s.removeAllRanges(); s.addRange(r); }";

/// Value length only (UTF-16 units, like `value.length`).
const VALUE_LEN_FN: &str =
    "function value_len() { return typeof this.value === 'string' ? this.value.length : (this.textContent || '').length; }";

const ENABLED_FN: &str = "function enabled() { const t = this.nodeType === 3 ? this.parentElement : this; \
    return !(t.disabled || (t.closest && t.closest('[aria-disabled=true],fieldset[disabled]'))); }";

/// `this` is the click target; `h` the node hit at the click point.
const CONTAINS_FN: &str = "function contains(h) { const t = this.nodeType === 3 ? this.parentElement : this; \
    for (let e = h; e; e = e.parentNode || e.host) if (e === t) return true; return false; }";

/// Short description of the element covering the target.
const COVER_FN: &str = "function cover() { const e = this.nodeType === 3 ? this.parentElement : this; if (!e) return 'something'; \
    const id = e.id ? '#' + e.id : ''; const cls = typeof e.className === 'string' && e.className.trim() ? '.' + e.className.trim().split(/\\s+/).slice(0, 2).join('.') : ''; \
    return '<' + e.localName + id + cls + '> ' + JSON.stringify((e.innerText || '').trim().slice(0, 60)); }";

impl Tab<'_, '_> {
    fn object(&mut self, backend: i64) -> Result<String> {
        let r = self.call("DOM.resolveNode", json!({"backendNodeId": backend}), None)?;
        r["object"]["objectId"].as_str().map(String::from).ok_or_else(|| Error::new("DOM.resolveNode: no object"))
    }

    fn call_on(&mut self, obj: &str, f: &str, args: Value) -> Result<Value> {
        let r = self.call(
            "Runtime.callFunctionOn",
            json!({"objectId": obj, "functionDeclaration": f, "arguments": args, "returnByValue": true, "awaitPromise": true}),
            None,
        )?;
        if r.get("exceptionDetails").is_some() {
            let name = f.trim_start_matches("function ").split('(').next().unwrap_or("helper");
            bail!("page script {name} failed");
        }
        Ok(r["result"]["value"].clone())
    }

    fn release(&mut self, obj: &str) {
        let _ = self.call("Runtime.releaseObject", json!({"objectId": obj}), Some(5.0));
    }

    fn visible(&mut self, backend: i64) -> bool {
        self.call("DOM.getContentQuads", json!({"backendNodeId": backend}), Some(5.0))
            .ok()
            .and_then(|q| q["quads"].as_array().map(|a| !a.is_empty()))
            .unwrap_or(false)
    }

    fn key(&mut self, key: &str, code: &str, vk: i64, text: Option<&str>) -> Result<()> {
        let mut down = json!({"type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk});
        if let Some(t) = text {
            down["text"] = t.into();
            down["unmodifiedText"] = t.into();
        }
        self.call("Input.dispatchKeyEvent", down, None)?;
        let up = json!({"type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk});
        self.call("Input.dispatchKeyEvent", up, None)?;
        Ok(())
    }
}

/// Role and name of one node, for refs and `--css`/`--focused` matches.
fn describe_backend(t: &mut Tab, page: &Page, doc: usize, backend: i64) -> Result<Option<Cand>> {
    let d = &page.docs[doc];
    let Ok(node) = on(t, &d.session, |t| t.call("DOM.describeNode", json!({"backendNodeId": backend}), None)) else {
        return Ok(None);
    };
    let tag = node["node"]["localName"].as_str().unwrap_or("").to_string();
    let ax = on(t, &d.session, |t| {
        t.call("Accessibility.getPartialAXTree", json!({"backendNodeId": backend, "fetchRelatives": false}), None)
    })
    .ok();
    let n = ax.as_ref().and_then(|a| a["nodes"].as_array()).and_then(|n| n.first().cloned()).unwrap_or(Value::Null);
    let role = ax_str(&n, "role");
    // (describeNode's frameId is the *content* frame of a frame owner, so
    // it can't name the element's own frame; report the session's.)
    Ok(Some(Cand { doc, frame: d.root.clone(), backend, role: if role.is_empty() { tag } else { role }, name: ax_str(&n, "name") }))
}

/// Backend ids of `sel` matches in one frame's document. `top`: the
/// session's own document; otherwise a same-process child frame, whose
/// document is reached through its owner element (querySelectorAll never
/// enters iframes).
fn css_query(t: &mut Tab, frame: &str, top: bool, sel: &str) -> Result<Vec<i64>> {
    let d = t.call("DOM.getDocument", json!({"depth": 0}), None)?;
    let root = if top {
        d["root"]["nodeId"].as_i64().unwrap_or(0)
    } else {
        let owner = t.call("DOM.getFrameOwner", json!({"frameId": frame}), Some(5.0))?;
        let ob = owner["backendNodeId"].as_i64().ok_or_else(|| Error::new("DOM.getFrameOwner: no backendNodeId"))?;
        let n = t.call("DOM.describeNode", json!({"backendNodeId": ob, "depth": 1, "pierce": true}), None)?;
        let Some(db) = n["node"]["contentDocument"]["backendNodeId"].as_i64() else { bail!("frame has no document") };
        let p = t.call("DOM.pushNodesByBackendIdsToFrontend", json!({"backendNodeIds": [db]}), None)?;
        p["nodeIds"][0].as_i64().filter(|n| *n != 0).ok_or_else(|| Error::new("frame document not pushed"))?
    };
    let r = t.call("DOM.querySelectorAll", json!({"nodeId": root, "selector": sel}), None)?;
    let mut backends = Vec::new();
    for id in r["nodeIds"].as_array().into_iter().flatten().filter_map(Value::as_i64) {
        let n = t.call("DOM.describeNode", json!({"nodeId": id}), None)?;
        backends.extend(n["node"]["backendNodeId"].as_i64());
    }
    Ok(backends)
}

/// Role and name for a match found by CSS (others already have them).
fn fill(t: &mut Tab, page: &Page, c: &mut Cand) {
    if !c.role.is_empty() {
        return;
    }
    if let Ok(Some(d)) = describe_backend(t, page, c.doc, c.backend) {
        c.role = d.role;
        c.name = d.name;
    }
}

/// Candidate tiers for LOC, best first.
fn candidates(t: &mut Tab, page: &Page, loc: &Locator) -> Result<Vec<Vec<Cand>>> {
    if let Some(r) = &loc.r#ref {
        let Some((tag, backend)) = parse_ref(r) else { bail!("bad ref {r:?}") };
        let Some(doc) = page.docs.iter().position(|d| d.alive && d.tag == tag) else {
            bail!("ref {r} is from a page that is no longer loaded (navigated?); take a new `tab snapshot`");
        };
        return match describe_backend(t, page, doc, backend)? {
            Some(c) => Ok(vec![vec![c]]),
            None => bail!("ref {r} is gone (the page changed); take a new `tab snapshot`"),
        };
    }
    if loc.focused {
        let mut doc = 0;
        loop {
            let sess = page.docs[doc].session.clone();
            let r = on(t, &sess, |t| t.call("Runtime.evaluate", json!({"expression": ACTIVE_FN}), None))?;
            let Some(obj) = r["result"]["objectId"].as_str().map(String::from) else { return Ok(vec![]) };
            let node = on(t, &sess, |t| t.call("DOM.describeNode", json!({"objectId": obj}), None));
            on(t, &sess, |t| {
                t.release(&obj);
                Ok(())
            })?;
            let node = node?;
            let backend = node["node"]["backendNodeId"].as_i64().unwrap_or(0);
            // Focus inside a cross-origin iframe: continue in its session.
            let child = page.docs.iter().position(|d| {
                d.alive && d.parent == Some(doc) && owner_backend(t, &sess, &d.root).ok() == Some(backend)
            });
            match child {
                Some(c) => doc = c,
                None => return Ok(describe_backend(t, page, doc, backend)?.into_iter().map(|c| vec![c]).collect()),
            }
        }
    }
    if let Some(sel) = &loc.css {
        // Role and name are filled in later, only for the matches reported:
        // each poll costs one query per frame plus one call per match.
        let mut out = Vec::new();
        for doc in (0..page.docs.len()).filter(|i| page.docs[*i].alive) {
            let d = page.docs[doc].clone();
            for (k, frame) in d.frames.iter().enumerate() {
                let found = on(t, &d.session, |t| css_query(t, frame, k == 0, sel));
                let backends = match found {
                    Ok(b) => b,
                    Err(e) if doc == 0 && k == 0 => return Err(e),
                    Err(_) => continue, // a frame that went away, or its document isn't there yet
                };
                out.extend(backends.into_iter().map(|backend| Cand {
                    doc,
                    frame: frame.clone(),
                    backend,
                    role: String::new(),
                    name: String::new(),
                }));
            }
        }
        return Ok(vec![out]);
    }
    let all = cands_of(&page.ax_trees(t));
    if let Some(label) = &loc.label {
        return Ok(name_tiers(&all, label, |r| FIELD_ROLES.contains(&r)));
    }
    if let Some(role) = &loc.role {
        let role_ok = |r: &str| r.eq_ignore_ascii_case(role);
        return Ok(match &loc.name {
            Some(n) => name_tiers(&all, n, role_ok),
            None => vec![all.iter().filter(|c| role_ok(&c.role)).cloned().collect()],
        });
    }
    if let Some(text) = &loc.text {
        let click = name_tiers(&all, text, |r| CLICK_ROLES.contains(&r));
        let other = name_tiers(&all, text, |r| !CLICK_ROLES.contains(&r) && !TEXT_ROLES.contains(&r) && r != "RootWebArea");
        let static_text = name_tiers(&all, text, |r| r == "StaticText");
        return Ok(vec![
            click[0].clone(),
            click[1].clone(),
            other[0].clone(),
            other[1].clone(),
            static_text[0].clone(),
            static_text[1].clone(),
            click[2].clone(),
        ]);
    }
    bail!("no element locator given");
}

/// Exactly one visible match for LOC, polling until `timeout` (fake-clock
/// aware: deadlines follow `Sys::now`/`Sys::sleep`).
fn resolve(t: &mut Tab, page: &mut Page, loc: &Locator, timeout: f64) -> Result<Cand> {
    if let Some(e) = loc.usage_error(true) {
        bail!("{e}");
    }
    let deadline = t.ctx.sys.now() + Duration::from_secs_f64(timeout.max(0.0));
    let mut first = true;
    let mut poll = FIND_POLL.0;
    loop {
        if !first {
            page.update(t)?;
        }
        first = false;
        // The best tier with any match decides, even if all its matches are
        // hidden for now: never fall back to a looser name (and type a
        // secret into another field) because the right one isn't shown yet.
        let tier = candidates(t, page, loc)?.into_iter().find(|t| !t.is_empty()).unwrap_or_default();
        let mut hidden = 0;
        let mut vis = Vec::new();
        for c in tier {
            let sess = page.docs[c.doc].session.clone();
            // --focused: an element can be focused and still have no box.
            if loc.focused || on(t, &sess, |t| Ok(t.visible(c.backend)))? {
                vis.push(c);
            } else {
                hidden += 1;
            }
        }
        let chosen = (!vis.is_empty()).then_some(vis);
        if let Some(mut pool) = chosen {
            match loc.nth {
                Some(n) if (n as usize) <= pool.len() => {
                    let mut c = pool.swap_remove(n as usize - 1);
                    fill(t, page, &mut c);
                    return Ok(c);
                }
                Some(_) => {}
                None if pool.len() == 1 => {
                    let mut c = pool.pop().unwrap();
                    fill(t, page, &mut c);
                    return Ok(c);
                }
                None => {
                    for c in pool.iter_mut().take(10) {
                        fill(t, page, c);
                    }
                    let list: Vec<String> =
                        pool.iter().take(10).enumerate().map(|(i, c)| format!("{}. {}", i + 1, page.cand_json(c))).collect();
                    let more = if pool.len() > 10 { format!("\n  … and {} more", pool.len() - 10) } else { String::new() };
                    bail!(
                        "{} matches {} elements; pass --nth N (1 = first) or a --ref:\n  {}{more}",
                        loc.describe(),
                        pool.len(),
                        list.join("\n  ")
                    );
                }
            }
        }
        if t.ctx.sys.now() >= deadline {
            let why = match (loc.nth, hidden) {
                (Some(n), _) => format!("no visible element #{n} for {}", loc.describe()),
                (None, 0) => format!("no element matches {}", loc.describe()),
                (None, h) => format!("{} matches only hidden elements ({h})", loc.describe()),
            };
            bail!("{why} (waited {}s); see `agent-display tab snapshot`", crate::util::fmt_g(timeout));
        }
        let left = deadline.saturating_duration_since(t.ctx.sys.now()).as_secs_f64();
        t.ctx.sys.sleep(poll.min(left).max(0.001));
        poll = (poll * 2.0).min(FIND_POLL.1);
    }
}

fn us_key(c: char) -> (String, i64) {
    if c.is_ascii_digit() {
        return (format!("Digit{c}"), c as i64);
    }
    if c.is_ascii_alphabetic() {
        return (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as i64);
    }
    if c == ' ' {
        return ("Space".into(), 32);
    }
    (String::new(), 0)
}

pub struct TypeArgs<'s> {
    pub loc: Locator,
    pub value: Option<&'s str>,
    pub stdin: bool,
    pub keys: bool,
    pub append: bool,
    pub submit: bool,
    pub keep_newline: bool,
}

pub fn cmd_tab_type(ctx: &Ctx, o: &TabOpts, tab: &str, a: &TypeArgs) -> Result<i32> {
    let secret = match (a.stdin, a.value) {
        (true, None) => {
            ctx.sys.no_core_dumps();
            Secret::from_stdin(ctx, a.keep_newline)?
        }
        (false, Some(v)) => Secret::literal(v),
        (true, Some(_)) => bail!("give TEXT or --stdin, not both"),
        (false, None) => bail!("give TEXT, or --stdin for secrets"),
    };
    let mut t = Tab::open(ctx, tab, o.port_raw, o.timeout.max(OWNERSHIP_TIMEOUT))?;
    t.emulate_focus();
    let mut page = Page::new(&mut t)?;
    let target = resolve(&mut t, &mut page, &a.loc, o.timeout)?;
    let sess = page.docs[target.doc].session.clone();
    let (verified, submitted) = on(&mut t, &sess, |t| {
        t.emulate_focus();
        type_into(t, &page, &target, &secret, a)
    })?;
    drop(secret);
    let mut out = Map::new();
    out.insert("tab".into(), tab.into());
    out.insert("typed".into(), page.cand_json(&target));
    out.insert("from".into(), (if a.stdin { "stdin" } else { "argv" }).into());
    out.insert("verified".into(), verified.clone());
    out.insert("submitted".into(), submitted.into());
    drop(t); // focus emulation off before we report
    ctx.println(&json::dumps(&Value::Object(out), false));
    if verified == Value::Bool(false) {
        let not_sent = if a.submit { "; not submitted" } else { "" };
        ctx.eprintln(&format!(
            "agent-display: the field's length doesn't match what was typed (maxlength, an input mask or a script \
             changed it){not_sent}; check with `tab shot`, or retry with --keys"
        ));
        return Ok(crate::util::exit::MISMATCH);
    }
    Ok(0)
}

/// (verified: true/false, or null when it can't be told; submitted).
fn type_into(t: &mut Tab, page: &Page, target: &Cand, secret: &Secret, a: &TypeArgs) -> Result<(Value, bool)> {
    let who = page.cand_json(target);
    let _ = t.call("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": target.backend}), None);
    let obj = t.object(target.backend)?;
    let r = (|| -> Result<(Value, bool)> {
        let info = t.call_on(&obj, FIELD_INFO_FN, json!([]))?;
        if info["editable"] != Value::Bool(true) {
            let kind = format!("{} {}", info["tag"].as_str().unwrap_or(""), info["type"].as_str().unwrap_or(""));
            bail!("{who} is not a text field ({})", kind.trim());
        }
        if info["disabled"] == Value::Bool(true) || info["readonly"] == Value::Bool(true) {
            bail!("{who} is disabled or read-only");
        }
        if info["secret"] == Value::Bool(true) && !a.stdin {
            bail!("{who} is a password or one-time-code field: pipe the value with --stdin, never put it on the command line");
        }
        t.call("DOM.focus", json!({"backendNodeId": target.backend}), None)?;
        if t.call_on(&obj, IS_ACTIVE_FN, json!([]))? != Value::Bool(true) {
            bail!("could not focus {who}");
        }
        if !a.append {
            t.call_on(&obj, SELECT_ALL_FN, json!([]))?;
            if a.keys {
                t.key("Delete", "Delete", 46, None)?; // insertText replaces the selection by itself
            }
        }
        let len = |t: &mut Tab| -> Result<usize> { Ok(t.call_on(&obj, VALUE_LEN_FN, json!([]))?.as_u64().unwrap_or(0) as usize) };
        let before = if a.append { len(t)? } else { 0 };
        if a.keys {
            for c in secret.expose().chars() {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                let (code, vk) = us_key(c);
                if vk == 0 {
                    t.call("Input.insertText", json!({"text": &*s}), None)?;
                } else {
                    t.key(s, &code, vk, Some(s))?;
                }
            }
        } else {
            t.call("Input.insertText", json!({"text": secret.expose()}), None)?;
        }
        // With --keys, split one-time-code boxes move focus on; then the
        // first box alone says nothing, so report null.
        let still_here = !a.keys || t.call_on(&obj, IS_ACTIVE_FN, json!([]))? == Value::Bool(true);
        let verified = if still_here { Value::Bool(len(t)? == before + secret.js_len()) } else { Value::Null };
        // Never submit a value the page cut short (maxlength, a mask): a
        // truncated password is a failed login, maybe a locked account.
        let submit = a.submit && verified != Value::Bool(false);
        if submit {
            t.key("Enter", "Enter", 13, Some("\r"))?;
        }
        Ok((verified, submit))
    })();
    t.release(&obj);
    r
}

pub fn cmd_tab_click(ctx: &Ctx, o: &TabOpts, tab: &str, loc: &Locator, count: i64) -> Result<i32> {
    let mut t = Tab::open(ctx, tab, o.port_raw, o.timeout.max(OWNERSHIP_TIMEOUT))?;
    t.emulate_focus();
    let mut page = Page::new(&mut t)?;
    let start = ctx.sys.now();
    let target = resolve(&mut t, &mut page, loc, o.timeout)?;
    // One budget for finding the element and for it becoming clickable.
    let deadline = start + Duration::from_secs_f64(o.timeout.max(0.0));
    let (x, y) = click_point(&mut t, &page, &target, deadline)?;
    t.call("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y}), None)?;
    for n in 1..=count.max(1) {
        for kind in ["mousePressed", "mouseReleased"] {
            let buttons = if kind == "mousePressed" { 1 } else { 0 };
            t.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": n}),
                None,
            )?;
        }
    }
    let mut out = Map::new();
    out.insert("tab".into(), tab.into());
    out.insert("clicked".into(), page.cand_json(&target));
    drop(t);
    ctx.println(&json::dumps(&Value::Object(out), false));
    Ok(0)
}

/// One level of the frame chain from the target up to the page: the node
/// expected under the click point in that document, and the point in that
/// document's viewport.
struct Level {
    doc: usize,
    expect: i64,
    x: f64,
    y: f64,
}

/// Click point of the target in every document from its own up to the
/// page (out-of-process iframes have their own viewport). None: no box.
fn levels(t: &mut Tab, page: &Page, c: &Cand) -> Result<Option<Vec<Level>>> {
    let sess = page.docs[c.doc].session.clone();
    let Ok(q) = on(t, &sess, |t| t.call("DOM.getContentQuads", json!({"backendNodeId": c.backend}), None)) else {
        return Ok(None);
    };
    let q: Vec<f64> = q["quads"][0].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
    if q.len() != 8 {
        return Ok(None);
    }
    let (mut x, mut y) = ((q[0] + q[2] + q[4] + q[6]) / 4.0, (q[1] + q[3] + q[5] + q[7]) / 4.0);
    let mut out = vec![Level { doc: c.doc, expect: c.backend, x, y }];
    let mut doc = c.doc;
    while let Some(p) = page.docs[doc].parent {
        let psess = page.docs[p].session.clone();
        let owner = owner_backend(t, &psess, &page.docs[doc].root)?;
        let bm = on(t, &psess, |t| t.call("DOM.getBoxModel", json!({"backendNodeId": owner}), None))?;
        let content = &bm["model"]["content"];
        x += content[0].as_f64().unwrap_or(0.0);
        y += content[1].as_f64().unwrap_or(0.0);
        out.push(Level { doc: p, expect: owner, x, y });
        doc = p;
    }
    Ok(Some(out))
}

/// What is wrong with clicking at this level's point, if anything.
fn hit_problem(t: &mut Tab, page: &Page, l: &Level) -> Result<Option<String>> {
    let sess = page.docs[l.doc].session.clone();
    on(t, &sess, |t| {
        // getNodeForLocation takes document coordinates: add the scroll.
        // Elements with pointer-events: none are skipped (the default), as
        // the mouse event will be: a toast layer doesn't block a click.
        let m = t.call("Page.getLayoutMetrics", json!({}), Some(5.0)).unwrap_or(Value::Null);
        let vp = if m["cssVisualViewport"].is_object() { &m["cssVisualViewport"] } else { &m["cssLayoutViewport"] };
        let (sx, sy) = (vp["pageX"].as_f64().unwrap_or(0.0), vp["pageY"].as_f64().unwrap_or(0.0));
        let p = json!({"x": (l.x + sx).floor(), "y": (l.y + sy).floor()});
        let Ok(hit) = t.call("DOM.getNodeForLocation", p, Some(5.0)) else {
            return Ok(Some("is outside the visible page".to_string()));
        };
        let Some(h) = hit["backendNodeId"].as_i64() else { return Ok(Some("is outside the visible page".into())) };
        if h == l.expect {
            return Ok(None);
        }
        let target = t.object(l.expect)?;
        let hobj = t.object(h)?;
        // A node in another frame (another JS world) is never inside.
        let inside = t.call_on(&target, CONTAINS_FN, json!([{"objectId": hobj}])).unwrap_or(Value::Bool(false));
        let r = if inside == Value::Bool(true) {
            None
        } else {
            let what = t.call_on(&hobj, COVER_FN, json!([])).ok().and_then(|v| v.as_str().map(String::from));
            Some(format!("is covered by {}", what.unwrap_or_else(|| "another element".into())))
        };
        t.release(&target);
        t.release(&hobj);
        Ok(r)
    })
}

/// Wait until the target is visible, still, enabled and on top at its
/// centre in every frame up to the page; returns that point in page
/// viewport coordinates.
fn click_point(t: &mut Tab, page: &Page, c: &Cand, deadline: Instant) -> Result<(f64, f64)> {
    let sess = page.docs[c.doc].session.clone();
    let obj = on(t, &sess, |t| t.object(c.backend))?;
    let r = (|| loop {
        let _ = on(t, &sess, |t| t.call("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": c.backend}), None));
        let a = levels(t, page, c)?;
        t.ctx.sys.sleep(0.05);
        let b = levels(t, page, c)?;
        let problem = match (a, b) {
            (Some(a), Some(b)) => {
                let (pa, pb) = (a.last().unwrap(), b.last().unwrap());
                if (pa.x - pb.x).abs() > 0.5 || (pa.y - pb.y).abs() > 0.5 {
                    Some("is still moving".to_string())
                } else if on(t, &sess, |t| t.call_on(&obj, ENABLED_FN, json!([])))? != Value::Bool(true) {
                    Some("is disabled".to_string())
                } else {
                    let mut p = None;
                    for l in b.iter().rev() {
                        p = hit_problem(t, page, l)?;
                        if p.is_some() {
                            break;
                        }
                    }
                    match p {
                        None => return Ok((pb.x, pb.y)),
                        some => some,
                    }
                }
            }
            _ => Some("has no box (hidden?)".to_string()),
        };
        if t.ctx.sys.now() >= deadline {
            let problem = problem.unwrap_or_default();
            return Err(Error::new(format!("{} {problem}; not clicking", page.cand_json(c))));
        }
        t.ctx.sys.sleep(POLL);
    })();
    let _ = on(t, &sess, |t| {
        t.release(&obj);
        Ok(())
    });
    r
}

/// Accessibility outline of every frame, with refs. Never prints field
/// values: text-entry fields show only [filled]/[empty], and their
/// descendants (Chromium's copy of the value) are skipped.
pub fn cmd_tab_snapshot(ctx: &Ctx, o: &TabOpts, tab: &str, all: bool) -> Result<i32> {
    let mut t = Tab::open(ctx, tab, o.port_raw, o.timeout)?;
    let page = Page::new(&mut t)?;
    let trees = page.ax_trees(&mut t);
    let owners = page.frame_owners(&mut t);
    let info = t.info()?;
    let url = info["url"].as_str().unwrap_or("");
    let url = if o.show_url { url.to_string() } else { redact_url(url) };
    let mut lines = vec![format!("# {} — {url}", info["title"].as_str().unwrap_or(""))];
    let by_frame: HashMap<(usize, &str), &Vec<Value>> = trees.iter().map(|(d, f, n)| ((*d, f.as_str()), n)).collect();
    let main = page.main_frame().to_string();
    let mut w = Walker { page: &page, by_frame: &by_frame, owners: &owners, all, lines: &mut lines, seen: HashSet::new() };
    w.frame(0, &main, 0);
    drop(t);
    ctx.println(&lines.join("\n"));
    Ok(0)
}

struct Walker<'p> {
    page: &'p Page,
    by_frame: &'p HashMap<(usize, &'p str), &'p Vec<Value>>,
    owners: &'p HashMap<(usize, i64), (usize, String)>,
    all: bool,
    lines: &'p mut Vec<String>,
    seen: HashSet<(usize, String)>,
}

impl Walker<'_> {
    fn frame(&mut self, doc: usize, frame: &str, depth: usize) {
        if !self.seen.insert((doc, frame.to_string())) {
            return;
        }
        let Some(nodes) = self.by_frame.get(&(doc, frame)).copied() else {
            self.lines.push(format!("{}- (frame not available)", "  ".repeat(depth)));
            return;
        };
        let by_id: HashMap<&str, &Value> = nodes.iter().filter_map(|n| Some((n["nodeId"].as_str()?, n))).collect();
        let root = nodes.iter().find(|n| n.get("parentId").is_none()).or(nodes.first());
        if let Some(root) = root {
            self.node(doc, root, &by_id, depth);
        }
    }

    fn node(&mut self, doc: usize, n: &Value, by_id: &HashMap<&str, &Value>, depth: usize) {
        let role = ax_str(n, "role");
        let name = ax_str(n, "name");
        let ignored = n["ignored"] == Value::Bool(true);
        let boring = ignored
            || matches!(role.as_str(), "InlineTextBox" | "LineBreak")
            || (matches!(
                role.as_str(),
                "generic" | "none" | "LabelText" | "paragraph" | "group" | "list" | "listitem" | "LayoutTable" | "LayoutTableRow"
                    | "LayoutTableCell" | "Section" | "div"
            ) && name.is_empty())
            || (!self.all && role == "StaticText");
        let backend = n["backendDOMNodeId"].as_i64();
        let child_frame = backend.and_then(|b| self.owners.get(&(doc, b))).cloned();
        let mut d = depth;
        if !boring || child_frame.is_some() {
            let mut line = format!("{}- {}", "  ".repeat(depth), role);
            if !name.is_empty() {
                line.push_str(&format!(" {}", json!(trunc(&name, 100))));
            }
            for p in n["properties"].as_array().into_iter().flatten() {
                let pn = p["name"].as_str().unwrap_or("");
                let pv = &p["value"]["value"];
                if matches!(pn, "focused" | "disabled" | "required" | "expanded" | "checked" | "selected" | "invalid")
                    && pv != &Value::Bool(false)
                    && pv != "false"
                {
                    if pv == &Value::Bool(true) || pv == "true" {
                        line.push_str(&format!(" [{pn}]"));
                    } else {
                        line.push_str(&format!(" [{pn}={}]", pv.as_str().map(String::from).unwrap_or(pv.to_string())));
                    }
                }
            }
            if VALUE_ROLES.contains(&role.as_str()) {
                line.push_str(if ax_str(n, "value").is_empty() { " [empty]" } else { " [filled]" });
            }
            if let Some((_, f)) = &child_frame
                && let Some(origin) = self.page.origins.get(f)
            {
                line.push_str(&format!(" [frame={origin}]"));
            }
            if let Some(b) = backend {
                line.push_str(&format!(" [ref={}.{b}]", self.page.docs[doc].tag));
            }
            self.lines.push(line);
            d += 1;
        }
        if let Some((cdoc, f)) = child_frame {
            self.frame(cdoc, &f, d);
        }
        if VALUE_ROLES.contains(&role.as_str()) {
            return;
        }
        for c in n["childIds"].as_array().into_iter().flatten() {
            if let Some(child) = c.as_str().and_then(|id| by_id.get(id)) {
                self.node(doc, child, by_id, d);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_parse_and_tags_are_stable() {
        assert_eq!(parse_ref("k3fq.12"), Some(("k3fq".into(), 12)));
        assert_eq!(parse_ref("@k3fq.12"), Some(("k3fq".into(), 12)));
        assert_eq!(parse_ref("ref=k3fq.12"), Some(("k3fq".into(), 12)));
        for bad in ["e12", "k3fq.", "k3fq.0", "K3FQ.1", "k3f.1", "k3f1.1", "k3fq.x"] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
        let t = doc_tag("FRAME", "LOADER1");
        assert_eq!(t, doc_tag("FRAME", "LOADER1"));
        assert_ne!(t, doc_tag("FRAME", "LOADER2"));
        assert!(parse_ref(&format!("{t}.5")).is_some());
    }

    #[test]
    fn locator_usage_errors() {
        let l = |f: fn(&mut Locator)| {
            let mut l = Locator::default();
            f(&mut l);
            l.usage_error(true)
        };
        assert!(l(|_| {}).unwrap().contains("exactly one"));
        assert!(l(|l| {
            l.label = Some("a".into());
            l.css = Some("b".into());
        })
        .unwrap()
        .contains("exactly one"));
        assert_eq!(l(|l| l.name = Some("x".into())).unwrap(), "give exactly one of --label, --role, --text, --css, --ref or --focused");
        assert_eq!(
            l(|l| {
                l.label = Some("x".into());
                l.name = Some("y".into());
            }),
            Some("--name needs --role".into())
        );
        assert_eq!(
            l(|l| {
                l.label = Some("x".into());
                l.nth = Some(0);
            }),
            Some("--nth counts from 1".into())
        );
        assert!(l(|l| l.r#ref = Some("e12".into())).unwrap().starts_with("bad ref"));
        assert_eq!(l(|l| l.focused = true), None);
    }
}
