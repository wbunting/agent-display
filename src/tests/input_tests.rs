//! `tab type/click/snapshot`, URL redaction and URLs on stdin, against a
//! fake page behind the fake CDP: frames (a same-process iframe and a
//! cross-origin one with its own session), an accessibility tree, focus,
//! hit testing and field values. Every test that types a secret checks
//! that it never reaches stdout, stderr or any CDP call but Input.*.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde_json::{Value, json};

use super::fakes::*;
use super::tab_tests::Fx;
use crate::util::Error;

const SECRET: &str = "Tést pässwörd 123!";
const OOP_SESSION: &str = "OOPIF-SESSION-1";

#[derive(Clone, Debug)]
pub struct El {
    pub backend: i64,
    pub role: &'static str,
    pub name: String,
    pub tag: &'static str,
    pub typ: &'static str,
    pub autocomplete: &'static str,
    pub visible: bool,
    pub disabled: bool,
    pub value: String,
    pub maxlength: Option<usize>,
    /// x, y, w, h in the session's viewport.
    pub rect: (f64, f64, f64, f64),
    pub css: Vec<&'static str>,
    /// Split one-time-code boxes: a typed key moves focus here.
    pub next_on_key: Option<i64>,
    /// Text nodes and the like inside it (for the hit test).
    pub inner: Vec<i64>,
    /// Hidden from the accessibility tree until this many AX reads.
    pub appears_after: usize,
    /// getContentQuads calls during which it is still moving.
    pub moving: usize,
}

pub fn el(backend: i64, role: &'static str, name: &str) -> El {
    let (tag, typ) = match role {
        "textbox" => ("input", ""),
        "button" => ("button", ""),
        "Iframe" => ("iframe", ""),
        _ => ("div", ""),
    };
    El {
        backend,
        role,
        name: name.to_string(),
        tag,
        typ,
        autocomplete: "",
        visible: true,
        disabled: false,
        value: String::new(),
        maxlength: None,
        rect: (10.0, 10.0 + backend as f64 * 30.0, 200.0, 20.0),
        css: vec![],
        next_on_key: None,
        inner: vec![],
        appears_after: 0,
        moving: 0,
    }
}

pub struct Frame {
    pub session: String,
    pub id: String,
    pub loader: String,
    pub url: String,
    pub title: String,
    /// (session of the parent document, owner element backend there).
    pub owner: Option<(String, i64)>,
    pub els: Vec<El>,
}

pub struct Cover {
    pub session: String,
    pub rect: (f64, f64, f64, f64),
    pub backend: i64,
    pub desc: String,
    /// Gone after this many hit tests (None: stays).
    pub until: Option<usize>,
    /// `pointer-events: none`: hit only with ignorePointerEventsNone.
    pub pe_none: bool,
}

#[derive(Default)]
pub struct PageState {
    pub frames: Vec<Frame>,
    pub active: HashMap<String, i64>,
    pub selected: Option<i64>,
    pub covers: Vec<Cover>,
    pub hit_tests: usize,
    pub ax_reads: usize,
    pub mouse: Vec<(String, String, f64, f64)>,
    pub enters: usize,
    pub focus_emulation: Vec<(String, bool)>,
    /// Scroll offset per session (getNodeForLocation is in document
    /// coordinates; rects here are viewport coordinates).
    pub scroll: HashMap<String, (f64, f64)>,
}

#[derive(Clone)]
pub struct FakePage(pub Rc<RefCell<PageState>>);

impl FakePage {
    pub fn st(&self) -> std::cell::RefMut<'_, PageState> {
        self.0.borrow_mut()
    }

    pub fn el(&self, backend: i64) -> El {
        self.0.borrow().frames.iter().flat_map(|f| f.els.iter()).find(|e| e.backend == backend).unwrap().clone()
    }

    pub fn value(&self, backend: i64) -> String {
        self.el(backend).value
    }

    fn with_el<R>(&self, session: &str, backend: i64, f: impl FnOnce(&mut El) -> R) -> Option<R> {
        let mut st = self.0.borrow_mut();
        st.frames.iter_mut().filter(|fr| fr.session == session).flat_map(|fr| fr.els.iter_mut()).find(|e| e.backend == backend).map(f)
    }

    fn is_cover(&self, session: &str, backend: i64) -> bool {
        self.0.borrow().covers.iter().any(|c| c.session == session && c.backend == backend)
    }

    fn frame_of(&self, session: &str, backend: i64) -> Option<String> {
        let st = self.0.borrow();
        st.frames.iter().find(|f| f.session == session && f.els.iter().any(|e| e.backend == backend)).map(|f| f.id.clone())
    }

    fn tree(&self, session: &str, id: &str) -> Value {
        let st = self.0.borrow();
        let f = st.frames.iter().find(|f| f.id == id).unwrap();
        let kids: Vec<String> = st
            .frames
            .iter()
            .filter(|c| c.session == session && c.owner.as_ref().is_some_and(|(s, _)| s == session) && c.id != id)
            .filter(|c| {
                // Direct children: owner element lives in this frame.
                let owner = c.owner.as_ref().unwrap().1;
                f.els.iter().any(|e| e.backend == owner)
            })
            .map(|c| c.id.clone())
            .collect();
        let frame = json!({"id": f.id, "loaderId": f.loader, "url": f.url,
                           "securityOrigin": f.url.split('/').take(3).collect::<Vec<_>>().join("/")});
        drop(st);
        json!({"frame": frame, "childFrames": kids.iter().map(|k| self.tree(session, k)).collect::<Vec<_>>()})
    }

    fn ax(e: &El, parent: &str) -> Value {
        let mut n = json!({"nodeId": format!("n{}", e.backend), "parentId": parent, "ignored": false,
            "role": {"type": "role", "value": e.role}, "name": {"type": "computedString", "value": e.name},
            "backendDOMNodeId": e.backend, "childIds": [], "properties": []});
        if e.role == "textbox" {
            n["value"] = json!({"type": "string", "value": e.value});
            if !e.value.is_empty() {
                // Chromium exposes the value as text inside the field.
                n["childIds"] = json!([format!("t{}", e.backend)]);
            }
            if e.disabled {
                n["properties"] = json!([{"name": "disabled", "value": {"type": "boolean", "value": true}}]);
            }
        }
        n
    }

    fn handle(&self, method: &str, p: &Value, session: Option<&str>, b: &FakeBrowser) -> Hook {
        let s = session.unwrap_or("").to_string();
        if !self.0.borrow().frames.iter().any(|f| f.session == s) {
            return Hook::Pass;
        }
        let ok = |v: Value| Hook::Return(Ok(v));
        let err = |m: &str| Hook::Return(Err(Error::new(m.to_string())));
        let backend = p["backendNodeId"].as_i64().or_else(|| {
            p["objectId"].as_str().and_then(|o| o.strip_prefix("obj-")).and_then(|n| n.parse().ok())
        });
        match method {
            "Page.getFrameTree" => {
                let root = {
                    let st = self.0.borrow();
                    st.frames.iter().find(|f| f.session == s && f.owner.as_ref().is_none_or(|(ps, _)| *ps != s)).unwrap().id.clone()
                };
                ok(json!({"frameTree": self.tree(&s, &root)}))
            }
            "Target.setAutoAttach" => {
                let kids: Vec<(String, String)> = self
                    .0
                    .borrow()
                    .frames
                    .iter()
                    .filter(|f| f.session != s && f.owner.as_ref().is_some_and(|(ps, _)| *ps == s))
                    .map(|f| (f.session.clone(), f.id.clone()))
                    .collect();
                for (cs, id) in kids {
                    b.push_event(
                        "Target.attachedToTarget",
                        json!({"sessionId": cs, "targetInfo": {"type": "iframe", "targetId": id}, "waitingForDebugger": false}),
                        Some(&s),
                    );
                }
                ok(json!({}))
            }
            "Accessibility.getFullAXTree" => {
                let mut st = self.st();
                st.ax_reads += 1;
                let reads = st.ax_reads;
                let fid = p["frameId"].as_str().unwrap_or("");
                let Some(f) = st.frames.iter().find(|f| f.session == s && f.id == fid) else {
                    return err("Frame with the given frameId is not found.");
                };
                let els: Vec<&El> = f.els.iter().filter(|e| e.appears_after < reads).collect();
                let mut nodes = vec![json!({"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
                    "name": {"value": f.title}, "backendDOMNodeId": 900,
                    "childIds": els.iter().map(|e| format!("n{}", e.backend)).collect::<Vec<_>>()})];
                for e in els {
                    nodes.push(Self::ax(e, "root"));
                    if e.role == "textbox" && !e.value.is_empty() {
                        nodes.push(json!({"nodeId": format!("t{}", e.backend), "parentId": format!("n{}", e.backend),
                            "ignored": false, "role": {"value": "StaticText"}, "name": {"value": e.value}, "childIds": []}));
                    }
                }
                ok(json!({"nodes": nodes}))
            }
            "Accessibility.getPartialAXTree" => match backend.and_then(|b| self.with_el(&s, b, |e| Self::ax(e, "x"))) {
                Some(n) => ok(json!({"nodes": [n]})),
                None => err("No node with given id found"),
            },
            "DOM.getDocument" => ok(json!({"root": {"nodeId": 1, "backendNodeId": 1}})),
            // Document node ids: 1 is the session's own document, 1000+i
            // the document of frames[i] (after pushNodesByBackendIds).
            "DOM.querySelectorAll" => {
                let sel = p["selector"].as_str().unwrap();
                let node = p["nodeId"].as_i64().unwrap();
                let st = self.0.borrow();
                let ids: Vec<i64> = st
                    .frames
                    .iter()
                    .enumerate()
                    .filter(|(i, f)| {
                        f.session == s
                            && if node == 1 { f.owner.as_ref().is_none_or(|(ps, _)| *ps != s) } else { node == 1000 + *i as i64 }
                    })
                    .flat_map(|(_, f)| f.els.iter())
                    .filter(|e| e.css.contains(&sel))
                    .map(|e| e.backend)
                    .collect();
                ok(json!({"nodeIds": ids}))
            }
            "DOM.pushNodesByBackendIdsToFrontend" => {
                let b = p["backendNodeIds"][0].as_i64().unwrap();
                ok(json!({"nodeIds": [if b >= 5000 { 1000 + (b - 5000) } else { 0 }]}))
            }
            "DOM.describeNode" if p["pierce"] == true => {
                // An iframe owner's content document (backend 5000+i).
                let b = backend.unwrap();
                let st = self.0.borrow();
                match st.frames.iter().position(|f| f.session == s && f.owner == Some((s.clone(), b))) {
                    Some(i) => ok(json!({"node": {"backendNodeId": b, "localName": "iframe",
                        "contentDocument": {"backendNodeId": 5000 + i as i64, "nodeType": 9}}})),
                    None => ok(json!({"node": {"backendNodeId": b, "localName": "div"}})),
                }
            }
            "DOM.describeNode" => {
                let b = backend.or_else(|| p["nodeId"].as_i64());
                match b.and_then(|b| self.with_el(&s, b, |e| (e.backend, e.tag))) {
                    Some((b, tag)) => ok(json!({"node": {"backendNodeId": b, "localName": tag}})),
                    None => err("No node with given id found"),
                }
            }
            "DOM.resolveNode" => match backend {
                Some(b) if self.with_el(&s, b, |_| ()).is_some() || self.is_cover(&s, b) || b > 100 => {
                    ok(json!({"object": {"type": "object", "objectId": format!("obj-{b}")}}))
                }
                _ => err("No node with given id found"),
            },
            "DOM.getContentQuads" => {
                let q = backend.and_then(|b| {
                    self.with_el(&s, b, |e| {
                        let (mut x, y, w, h) = e.rect;
                        if e.moving > 0 {
                            e.moving -= 1;
                            x += 10.0 * e.moving as f64;
                        }
                        e.visible.then(|| json!([[x, y, x + w, y, x + w, y + h, x, y + h]]))
                    })
                });
                match q {
                    Some(Some(q)) => ok(json!({"quads": q})),
                    Some(None) => err("Could not compute content quads."),
                    None => err("No node with given id found"),
                }
            }
            "DOM.getBoxModel" => match backend.and_then(|b| self.with_el(&s, b, |e| e.rect)) {
                Some((x, y, w, h)) => ok(json!({"model": {"content": [x, y, x + w, y, x + w, y + h, x, y + h]}})),
                None => err("No node with given id found"),
            },
            "DOM.scrollIntoViewIfNeeded" => ok(json!({})),
            "DOM.getFrameOwner" => {
                let st = self.0.borrow();
                match st.frames.iter().find(|f| f.id == p["frameId"].as_str().unwrap_or("")) {
                    Some(f) if f.owner.as_ref().is_some_and(|(ps, _)| *ps == s) => {
                        ok(json!({"backendNodeId": f.owner.as_ref().unwrap().1}))
                    }
                    _ => err("Frame with the given id does not belong to the target."),
                }
            }
            "Page.getLayoutMetrics" => {
                let (sx, sy) = self.0.borrow().scroll.get(&s).copied().unwrap_or((0.0, 0.0));
                ok(json!({"cssLayoutViewport": {"pageX": sx, "pageY": sy, "clientWidth": 1280, "clientHeight": 800},
                          "cssVisualViewport": {"pageX": sx, "pageY": sy, "clientWidth": 1280, "clientHeight": 800},
                          "cssContentSize": {"width": 1280, "height": 3000}}))
            }
            "DOM.getNodeForLocation" => {
                let (sx, sy) = self.0.borrow().scroll.get(&s).copied().unwrap_or((0.0, 0.0));
                let (x, y) = (p["x"].as_f64().unwrap() - sx, p["y"].as_f64().unwrap() - sy);
                let inside = |r: (f64, f64, f64, f64)| x >= r.0 && y >= r.1 && x < r.0 + r.2 && y < r.1 + r.3;
                let mut st = self.st();
                st.hit_tests += 1;
                let n = st.hit_tests;
                let pe = p["ignorePointerEventsNone"].as_bool().unwrap_or(false);
                if let Some(c) = st
                    .covers
                    .iter()
                    .find(|c| c.session == s && inside(c.rect) && c.until.is_none_or(|u| n <= u) && (pe || !c.pe_none))
                {
                    return ok(json!({"backendNodeId": c.backend, "frameId": "F"}));
                }
                let hit = st
                    .frames
                    .iter()
                    .filter(|f| f.session == s)
                    .flat_map(|f| f.els.iter())
                    .rfind(|e| e.visible && inside(e.rect))
                    .map(|e| e.inner.first().copied().unwrap_or(e.backend));
                match hit {
                    Some(h) => ok(json!({"backendNodeId": h, "frameId": "F"})),
                    None => err("No node found at given location"),
                }
            }
            "DOM.focus" => {
                if let Some(b) = backend {
                    self.st().active.insert(s, b);
                }
                ok(json!({}))
            }
            "Emulation.setFocusEmulationEnabled" => {
                self.st().focus_emulation.push((s, p["enabled"].as_bool().unwrap()));
                ok(json!({}))
            }
            "Runtime.releaseObject" => ok(json!({})),
            "Runtime.evaluate" if p["expression"].as_str().unwrap_or("").starts_with("(() => { let a = document.activeElement") => {
                match self.0.borrow().active.get(&s) {
                    Some(b) => ok(json!({"result": {"type": "object", "objectId": format!("obj-{b}")}})),
                    None => ok(json!({"result": {"type": "object", "subtype": "null", "value": null}})),
                }
            }
            "Runtime.callFunctionOn" => {
                let decl = p["functionDeclaration"].as_str().unwrap();
                let fname = decl.trim_start_matches("function ").split('(').next().unwrap().to_string();
                let Some(b) = backend else { return err("no object") };
                let arg = p["arguments"][0]["objectId"].as_str().and_then(|o| o.strip_prefix("obj-")).and_then(|n| n.parse::<i64>().ok());
                let cover_desc = self.0.borrow().covers.iter().find(|c| c.backend == b).map(|c| c.desc.clone());
                let active = self.0.borrow().active.get(&s).copied();
                let fr_target = self.frame_of(&s, b);
                let fr_arg = arg.and_then(|a| self.frame_of(&s, a));
                let Some(e) = self.with_el(&s, b, |e| e.clone()) else {
                    if fname == "cover" {
                        return ok(json!({"result": {"type": "string", "value": cover_desc.unwrap_or_default()}}));
                    }
                    if fname == "contains" {
                        return ok(json!({"result": {"type": "boolean", "value": false}}));
                    }
                    return err("Could not find object with given id");
                };
                let v = match fname.as_str() {
                    "field_info" => {
                        let secret = e.typ == "password" || ["one-time-code", "current-password"].contains(&e.autocomplete);
                        json!({"tag": e.tag, "type": e.typ, "secret": secret, "editable": e.tag == "input",
                               "disabled": e.disabled, "readonly": false})
                    }
                    "is_active" => json!(active == Some(b)),
                    "select_all" => {
                        self.st().selected = Some(b);
                        Value::Null
                    }
                    "value_len" => json!(e.value.encode_utf16().count()),
                    "enabled" => json!(!e.disabled),
                    "contains" => {
                        if fr_arg.is_some() && fr_arg != fr_target {
                            return err("Argument should belong to the same JavaScript world as target object");
                        }
                        json!(arg.is_some_and(|a| a == b || e.inner.contains(&a)))
                    }
                    "cover" => json!(format!("<{}> {:?}", e.tag, e.name)),
                    other => panic!("unexpected page function {other}"),
                };
                ok(json!({"result": {"type": "object", "value": v}}))
            }
            "Input.insertText" => {
                let text = p["text"].as_str().unwrap().to_string();
                let active = self.0.borrow().active.get(&s).copied();
                let selected = self.st().selected.take();
                if let Some(a) = active {
                    self.with_el(&s, a, |e| {
                        if selected == Some(e.backend) {
                            e.value.clear();
                        }
                        e.value.push_str(&text);
                        if let Some(m) = e.maxlength {
                            e.value = e.value.chars().take(m).collect();
                        }
                    });
                }
                ok(json!({}))
            }
            "Input.dispatchKeyEvent" => {
                let key = p["key"].as_str().unwrap_or("").to_string();
                if p["type"] == "keyDown" && key == "Enter" {
                    self.st().enters += 1;
                    return ok(json!({}));
                }
                let active = self.0.borrow().active.get(&s).copied();
                if p["type"] == "rawKeyDown" && key == "Delete" {
                    let sel = self.st().selected.take();
                    if let Some(a) = active.filter(|a| sel == Some(*a)) {
                        self.with_el(&s, a, |e| e.value.clear());
                    }
                }
                if p["type"] == "keyDown"
                    && let Some(t) = p["text"].as_str()
                    && let Some(a) = active
                {
                    let next = self.with_el(&s, a, |e| {
                        if e.maxlength.is_none_or(|m| e.value.chars().count() < m) {
                            e.value.push_str(t);
                        }
                        e.next_on_key
                    });
                    if let Some(Some(n)) = next {
                        self.st().active.insert(s, n);
                    }
                }
                ok(json!({}))
            }
            "Input.dispatchMouseEvent" => {
                let ty = p["type"].as_str().unwrap().to_string();
                self.st().mouse.push((s, ty, p["x"].as_f64().unwrap(), p["y"].as_f64().unwrap()));
                ok(json!({}))
            }
            _ => Hook::Pass,
        }
    }
}

/// A login page in tab `tab`: email (no type attribute), a hidden
/// duplicate, password, code (no type attr), split OTP, a shadow-DOM
/// member field, a same-process iframe and a cross-origin iframe, and
/// "Sign in"/"Accept all" buttons.
pub fn login_page(f: &Fx, tab: &str) -> FakePage {
    let main = format!("S-{tab}");
    let mut email = el(2, "textbox", "Email address");
    email.css = vec!["#em", "input"];
    let mut ghost = el(3, "textbox", "Email address");
    ghost.visible = false;
    let mut pw = el(4, "textbox", "Password");
    pw.typ = "password";
    pw.css = vec!["input"];
    let mut code = el(5, "textbox", "Verification code");
    code.css = vec!["input"];
    let mut digits: Vec<El> = (0..6).map(|i| el(6 + i, "textbox", &format!("Digit {}", i + 1))).collect();
    for (i, d) in digits.iter_mut().enumerate() {
        d.maxlength = Some(1);
        if i < 5 {
            d.next_on_key = Some(7 + i as i64);
        }
    }
    let member = el(12, "textbox", "Member number");
    let mut sign_in = el(14, "button", "Sign in");
    sign_in.inner = vec![141];
    let accept = el(44, "button", "Accept all");
    let mut same = el(60, "Iframe", "");
    same.rect = (100.0, 600.0, 400.0, 150.0);
    let mut cross = el(61, "Iframe", "");
    cross.rect = (100.0, 800.0, 400.0, 150.0);
    let mut els = vec![email, ghost, pw, code];
    els.extend(digits);
    els.extend([member, sign_in, accept, same, cross]);
    let mut inner_field = el(70, "textbox", "Inner field");
    inner_field.css = vec!["#i", "input"];
    inner_field.rect = (120.0, 620.0, 100.0, 20.0);
    let mut bank_pw = el(4, "textbox", "Bank password"); // same backend id as the page's: other process
    bank_pw.typ = "password";
    bank_pw.rect = (5.0, 5.0, 200.0, 20.0);
    let mut bank_btn = el(13, "button", "Continue");
    bank_btn.rect = (5.0, 40.0, 100.0, 20.0);
    let page = FakePage(Rc::new(RefCell::new(PageState {
        frames: vec![
            Frame {
                session: main.clone(),
                id: "MAINFRAME".into(),
                loader: "LOADER-1".into(),
                url: "http://127.0.0.1:1/login.html?token=SECRET-ONE-TIME".into(),
                title: "Fake Bank login".into(),
                owner: None,
                els,
            },
            Frame {
                session: main.clone(),
                id: "CHILDFRAME".into(),
                loader: "LOADER-C".into(),
                url: "http://127.0.0.1:1/child.html".into(),
                title: "child".into(),
                owner: Some((main.clone(), 60)),
                els: vec![inner_field],
            },
            Frame {
                session: OOP_SESSION.into(),
                id: "OOPFRAME".into(),
                loader: "LOADER-O".into(),
                url: "https://bank.example/embedded?sid=abc".into(),
                title: "bank".into(),
                owner: Some((main, 61)),
                els: vec![bank_pw, bank_btn],
            },
        ],
        ..Default::default()
    })));
    let p = page.clone();
    let b = f.b.clone();
    f.b.set_session_hook(move |m, params, s| p.handle(m, params, s, &b));
    // The tab shows the login page (with a one-time token in its URL).
    let url = page.0.borrow().frames[0].url.clone();
    for (id, t) in f.b.st().targets.iter_mut() {
        if id == tab {
            t["url"] = url.clone().into();
            t["title"] = "Fake Bank login".into();
        }
    }
    page
}

fn fx() -> (Fx, String, FakePage) {
    let f = Fx::new(false);
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    let page = login_page(&f, &tab);
    (f, tab, page)
}

fn stdin(f: &Fx, data: &[u8]) {
    *f.sys.stdin.borrow_mut() = Some(data.to_vec());
}

/// The secret is nowhere but in Input.* calls.
fn assert_no_leak(f: &Fx, out: &str, err: &str, secret: &str) {
    assert!(!out.contains(secret), "stdout leaks the secret: {out}");
    assert!(!err.contains(secret), "stderr leaks the secret: {err}");
    for (m, p, _) in f.calls() {
        if !m.starts_with("Input.") {
            assert!(!p.to_string().contains(secret), "{m} carries the secret: {p}");
        }
    }
}

fn run(f: &Fx, argv: &[&str]) -> (i32, String, String) {
    f.cli(argv)
}

fn json_out(out: &str) -> Value {
    serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"))
}

// ------------------------------------------------------------ locators

#[test]
fn label_finds_field_without_type_attribute_and_skips_hidden_duplicate() {
    let (f, tab, page) = fx();
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Email address", "agent@example.com"]);
    assert_eq!(code, 0, "{err}");
    let v = json_out(&out);
    assert_eq!(v["typed"]["name"], "Email address");
    assert_eq!(v["verified"], true);
    assert_eq!(v["from"], "argv");
    assert_eq!(page.value(2), "agent@example.com");
    assert_eq!(page.value(3), ""); // the hidden duplicate
    // Loose label: case, spacing and substring.
    stdin(&f, b"483920\n");
    let (code, out, err) = run(&f, &["type", &tab, "--label", "verification", "--stdin"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["typed"]["name"], "Verification code");
    assert_eq!(page.value(5), "483920");
    assert_no_leak(&f, &out, &err, "483920");
}

#[test]
fn role_and_name_case_insensitive_role() {
    let (f, tab, page) = fx();
    let (code, out, err) = run(&f, &["type", &tab, "--role", "TextBox", "--name", "member NUMBER", "M-778"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["typed"]["name"], "Member number");
    assert_eq!(page.value(12), "M-778");
}

#[test]
fn css_ref_and_focused() {
    let (f, tab, page) = fx();
    let (code, _, err) = run(&f, &["type", &tab, "--css", "#em", "a@b.c"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(2), "a@b.c");
    // --css that matches several (the page's three visible inputs and the
    // same-process iframe's): ambiguity, then --nth.
    let (code, _, err) = run(&f, &["type", &tab, "--css", "input", "x", "--timeout", "1"]);
    assert_eq!(code, 1);
    assert!(err.contains("matches 4 elements"), "{err}");
    assert!(err.contains("4. {\"role\":\"textbox\",\"name\":\"Inner field\""), "{err}");
    let partial = |f: &Fx| f.methods().iter().filter(|m| *m == "Accessibility.getPartialAXTree").count();
    let before = partial(&f);
    let (code, _, err) = run(&f, &["type", &tab, "--css", "input", "--nth", "3", "999"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(5), "999");
    // Only the chosen match gets its role and name looked up.
    assert_eq!(partial(&f) - before, 1);
    // --ref from a snapshot line.
    let (_, snap, _) = run(&f, &["snapshot", &tab]);
    let line = snap.lines().find(|l| l.contains("\"Member number\"")).unwrap();
    let r = line.split("[ref=").nth(1).unwrap().trim_end_matches(']');
    let (code, out, err) = run(&f, &["type", &tab, "--ref", r, "M-1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["typed"]["ref"], r);
    assert_eq!(page.value(12), "M-1");
    // --focused types into the element that has focus.
    page.st().active.insert(format!("S-{tab}"), 5);
    let (code, out, err) = run(&f, &["type", &tab, "--focused", "1234"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["typed"]["name"], "Verification code");
    assert_eq!(page.value(5), "1234");
}

#[test]
fn stale_and_foreign_refs_fail_cleanly() {
    let (f, tab, page) = fx();
    let (_, snap, _) = run(&f, &["snapshot", &tab]);
    let line = snap.lines().find(|l| l.contains("\"Password\"") && !l.contains("frame")).unwrap();
    let r = line.split("[ref=").nth(1).unwrap().trim_end_matches(']').to_string();
    // The page loads a new document (cross-process navigation): backend
    // ids start over, so the old ref must not hit the new node 4.
    page.st().frames[0].loader = "LOADER-2".into();
    let (code, _, err) = run(&f, &["type", &tab, "--ref", &r, "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("no longer loaded"), "{err}");
    assert_eq!(page.value(4), "");
    // Same document, node removed.
    let (_, snap, _) = run(&f, &["snapshot", &tab]);
    let line = snap.lines().find(|l| l.contains("\"Member number\"")).unwrap();
    let r = line.split("[ref=").nth(1).unwrap().trim_end_matches(']').to_string();
    page.st().frames[0].els.retain(|e| e.backend != 12);
    let (code, _, err) = run(&f, &["type", &tab, "--ref", &r, "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("is gone"), "{err}");
}

#[test]
fn text_matching_tiers_ambiguity_and_nth() {
    let (f, tab, page) = fx();
    let (code, out, err) = run(&f, &["click", &tab, "--text", "sign in"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["clicked"]["name"], "Sign in");
    let downs: Vec<_> = page.st().mouse.iter().filter(|m| m.1 == "mousePressed").cloned().collect();
    assert_eq!(downs.len(), 1);
    let sign = page.el(14);
    assert_eq!((downs[0].2, downs[0].3), (sign.rect.0 + 100.0, sign.rect.1 + 10.0));
    // Two buttons with the same name: listed, numbered, nothing clicked.
    let mut dup = el(45, "button", "Sign in");
    dup.rect = (300.0, 300.0, 50.0, 20.0);
    page.st().frames[0].els.push(dup);
    let before = page.st().mouse.len();
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Sign in", "--timeout", "1"]);
    assert_eq!(code, 1);
    assert!(err.contains("matches 2 elements; pass --nth N (1 = first) or a --ref"), "{err}");
    assert!(err.contains("1. {\"role\":\"button\",\"name\":\"Sign in\""), "{err}");
    assert_eq!(page.st().mouse.len(), before);
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Sign in", "--nth", "2"]);
    assert_eq!(code, 0, "{err}");
    let last = page.st().mouse.last().cloned().unwrap();
    assert_eq!((last.2, last.3), (325.0, 310.0));
}

#[test]
fn waits_for_an_element_that_appears_on_the_fake_clock() {
    let (f, tab, page) = fx();
    let mut late = el(50, "textbox", "Late field");
    late.appears_after = 5;
    page.st().frames[0].els.push(late);
    let (code, _, err) = run(&f, &["type", &tab, "--label", "Late field", "x"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(50), "x");
    // Never appears: fails at --timeout on the fake clock, no real waiting.
    let started = std::time::Instant::now();
    let (code, _, err) = run(&f, &["type", &tab, "--label", "No such field", "x", "--timeout", "3"]);
    assert_eq!(code, 1);
    assert!(err.contains("no element matches label \"No such field\" (waited 3s)"), "{err}");
    assert!(started.elapsed().as_secs_f64() < 2.0);
    let slept: f64 = f.sys.sleeps.borrow().iter().sum();
    assert!(slept >= 3.0, "{slept}");
}

#[test]
fn hidden_only_matches_are_reported_as_hidden() {
    let (f, tab, page) = fx();
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 4).unwrap().visible = false;
    stdin(&f, SECRET.as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin", "--timeout", "0.5"]);
    assert_eq!(code, 1);
    assert!(err.contains("label \"Password\" matches only hidden elements (1)"), "{err}");
    // Not typed into "Bank password" (a looser, containing match) instead.
    assert_eq!(page.frame_value(OOP_SESSION, 4), "");
    assert_no_leak(&f, &out, &err, SECRET);
}

// ------------------------------------------------------------ secrets

#[test]
fn password_from_stdin_is_typed_verified_and_never_echoed() {
    let (f, tab, page) = fx();
    stdin(&f, format!("{SECRET}\n").as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "password", "--stdin"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(4), SECRET); // one trailing newline dropped
    let v = json_out(&out);
    assert_eq!((v["from"].as_str(), v["verified"].as_bool()), (Some("stdin"), Some(true)));
    assert!(*f.sys.hardened.borrow(), "no PR_SET_DUMPABLE 0 while holding a secret");
    let inserts: Vec<_> = f.calls().into_iter().filter(|c| c.0 == "Input.insertText").collect();
    assert_eq!(inserts.len(), 1);
    assert_no_leak(&f, &out, &err, SECRET);
}

#[test]
fn secret_fields_refuse_argv_values() {
    let (f, tab, page) = fx();
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Password", "hunter2"]);
    assert_eq!(code, 1);
    assert!(err.contains("is a password or one-time-code field: pipe the value with --stdin"), "{err}");
    assert!(!out.contains("hunter2") && !err.contains("hunter2"));
    assert_eq!(page.value(4), "");
    assert!(!f.methods().iter().any(|m| m.starts_with("Input.")));
    // autocomplete=one-time-code counts too.
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 5).unwrap().autocomplete = "one-time-code";
    let (code, _, err) = run(&f, &["type", &tab, "--label", "Verification code", "123456"]);
    assert_eq!(code, 1);
    assert!(err.contains("one-time-code field"), "{err}");
}

#[test]
fn stdin_refusals() {
    let (mut f, tab, page) = fx();
    // Empty (a failed `op read` in a pipe without pipefail).
    stdin(&f, b"");
    let (code, _, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin"]);
    assert_eq!(code, 1);
    assert!(err.contains("stdin was empty"), "{err}");
    stdin(&f, b"\n");
    let (code, _, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin"]);
    assert_eq!(code, 1);
    assert!(err.contains("stdin was empty"), "{err}");
    // Oversize.
    let big = "s".repeat(crate::redact::Secret::MAX + 1);
    stdin(&f, big.as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin"]);
    assert_eq!(code, 1);
    assert!(err.contains("larger than 65536 bytes"), "{err}");
    assert!(!err.contains("sss") && !out.contains("sss"));
    // Not UTF-8.
    stdin(&f, b"\xff\xfepw");
    let (code, _, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin"]);
    assert_eq!(code, 1);
    assert!(err.contains("not UTF-8"), "{err}");
    // A terminal would echo it.
    f.sys.stdin_tty = true;
    stdin(&f, SECRET.as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin"]);
    assert_eq!(code, 1);
    assert!(err.contains("stdin is a terminal"), "{err}");
    assert_no_leak(&f, &out, &err, SECRET);
    assert_eq!(page.value(4), "");
    assert!(!f.methods().iter().any(|m| m.starts_with("Input.")));
}

#[test]
fn keep_newline_keeps_it() {
    let (f, tab, page) = fx();
    stdin(&f, b"line\n");
    let (code, _, err) = run(&f, &["type", &tab, "--label", "Email address", "--stdin", "--keep-newline"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(2), "line\n");
}

#[test]
fn length_mismatch_exits_6_without_echoing() {
    let (f, tab, page) = fx();
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 4).unwrap().maxlength = Some(6);
    stdin(&f, SECRET.as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin"]);
    assert_eq!(code, crate::util::exit::MISMATCH);
    assert_eq!(code, 6);
    assert_eq!(json_out(&out)["verified"], false);
    assert!(err.contains("length doesn't match"), "{err}");
    assert_eq!(page.value(4).chars().count(), 6);
    assert_no_leak(&f, &out, &err, SECRET);
    assert_no_leak(&f, &out, &err, &page.value(4));
}

#[test]
fn submit_is_skipped_when_the_length_does_not_match() {
    let (f, tab, page) = fx();
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 4).unwrap().maxlength = Some(6);
    stdin(&f, SECRET.as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Password", "--stdin", "--submit"]);
    assert_eq!(code, 6);
    let v = json_out(&out);
    assert_eq!((v["verified"].as_bool(), v["submitted"].as_bool()), (Some(false), Some(false)));
    assert!(err.contains("; not submitted;"), "{err}");
    assert_eq!(page.st().enters, 0, "Enter pressed after a truncated password");
    assert!(!f.calls().iter().any(|c| c.0 == "Input.dispatchKeyEvent" && c.1["key"] == "Enter"));
    assert_no_leak(&f, &out, &err, SECRET);
    // --keys can't verify (null): it still submits.
    stdin(&f, b"112233");
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Digit 1", "--stdin", "--keys", "--submit"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["submitted"], true);
    assert_eq!(page.st().enters, 1);
}

#[test]
fn split_otp_with_keys_moves_through_the_boxes() {
    let (f, tab, page) = fx();
    stdin(&f, b"112233");
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Digit 1", "--stdin", "--keys"]);
    assert_eq!(code, 0, "{err}");
    let otp: String = (6..12).map(|b| page.value(b)).collect();
    assert_eq!(otp, "112233");
    // Focus moved on, so the first box alone can't verify the length.
    assert_eq!(json_out(&out)["verified"], Value::Null);
    let keys: Vec<_> = f.calls().into_iter().filter(|c| c.0 == "Input.dispatchKeyEvent" && c.1["type"] == "keyDown").collect();
    assert_eq!(keys.len(), 6);
    assert_eq!(keys[0].1["code"], "Digit1");
    assert_no_leak(&f, &out, &err, "112233");
}

#[test]
fn append_and_submit() {
    let (f, tab, page) = fx();
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 2).unwrap().value = "agent".into();
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Email address", "@example.com", "--append", "--submit"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(2), "agent@example.com");
    assert_eq!(json_out(&out)["submitted"], true);
    assert_eq!(page.st().enters, 1);
}

#[test]
fn errors_never_contain_the_secret() {
    // Every refusal or failure after the secret was read.
    let (f, tab, page) = fx();
    page.st().frames[0].els.push({
        let mut d = el(80, "textbox", "Locked");
        d.disabled = true;
        d
    });
    for argv in [
        vec!["--label", "Locked"],
        vec!["--label", "Nope", "--timeout", "0.2"],
        vec!["--role", "textbox", "--timeout", "0.2"],
        vec!["--role", "button", "--name", "Sign in"],
        vec!["--ref", "aaaa.4"],
    ] {
        stdin(&f, SECRET.as_bytes());
        let mut full = vec!["type", tab.as_str()];
        full.extend(argv.iter().copied());
        full.push("--stdin");
        let (code, out, err) = run(&f, &full);
        assert_eq!(code, 1, "{argv:?}: {err}");
        assert!(!err.is_empty());
        assert_no_leak(&f, &out, &err, SECRET);
    }
}

// ------------------------------------------------------------ click

#[test]
fn covered_click_is_refused_and_disappearing_cover_is_waited_out() {
    let (f, tab, page) = fx();
    let main = format!("S-{tab}");
    let sign = page.el(14);
    page.st().covers.push(Cover {
        session: main.clone(),
        rect: (0.0, 0.0, 2000.0, 2000.0),
        backend: 99,
        desc: "<div#modal2> \"Session expired\"".into(),
        until: None,
        pe_none: false,
    });
    let (code, out, err) = run(&f, &["click", &tab, "--text", "Sign in", "--timeout", "1.5"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("is covered by <div#modal2> \"Session expired\"; not clicking"), "{err}");
    assert!(!page.st().mouse.iter().any(|m| m.1 == "mousePressed"));
    // A modal that goes away after a few checks: the click waits for it.
    let n = page.st().hit_tests;
    page.st().covers[0].until = Some(n + 3);
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Sign in"]);
    assert_eq!(code, 0, "{err}");
    let presses: Vec<_> = page.st().mouse.iter().filter(|m| m.1 == "mousePressed").cloned().collect();
    assert_eq!(presses.len(), 1);
    assert_eq!((presses[0].2, presses[0].3), (sign.rect.0 + 100.0, sign.rect.1 + 10.0));
    assert_eq!(presses[0].0, main);
}

#[test]
fn hit_test_uses_document_coordinates_when_scrolled() {
    let (f, tab, page) = fx();
    let main = format!("S-{tab}");
    // Scrolled 500 px down (the iframe's document scrolled too).
    page.st().scroll.insert(main.clone(), (0.0, 500.0));
    page.st().scroll.insert(OOP_SESSION.into(), (0.0, 30.0));
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Sign in", "--timeout", "1"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Continue", "--timeout", "1"]);
    assert_eq!(code, 0, "{err}");
    let hits: Vec<(f64, f64, Option<String>)> = f
        .calls()
        .into_iter()
        .filter(|c| c.0 == "DOM.getNodeForLocation")
        .map(|c| (c.1["x"].as_f64().unwrap(), c.1["y"].as_f64().unwrap(), c.2))
        .collect();
    let sign = page.el(14);
    assert!(hits.contains(&(sign.rect.0 + 100.0, sign.rect.1 + 10.0 + 500.0, Some(main.clone()))), "{hits:?}");
    // Iframe level: its own scroll; page level: the page's.
    assert!(hits.contains(&(55.0, 50.0 + 30.0, Some(OOP_SESSION.into()))), "{hits:?}");
    assert!(hits.contains(&(155.0, 850.0 + 500.0, Some(main))), "{hits:?}");
    // Mouse events stay in viewport coordinates.
    let press = page.st().mouse.iter().rfind(|m| m.1 == "mousePressed").cloned().unwrap();
    assert_eq!((press.2, press.3), (155.0, 850.0));
}

#[test]
fn pointer_events_none_layers_do_not_block_clicks() {
    let (f, tab, page) = fx();
    page.st().covers.push(Cover {
        session: format!("S-{tab}"),
        rect: (0.0, 0.0, 2000.0, 2000.0),
        backend: 97,
        desc: "<div#toasts> \"\"".into(),
        until: None,
        pe_none: true,
    });
    let (code, out, err) = run(&f, &["click", &tab, "--text", "Sign in", "--timeout", "1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["clicked"]["name"], "Sign in");
    for c in f.calls().into_iter().filter(|c| c.0 == "DOM.getNodeForLocation") {
        assert_ne!(c.1["ignorePointerEventsNone"], true, "{}", c.1);
    }
}

#[test]
fn element_poll_backs_off_to_one_second() {
    let (f, tab, _page) = fx();
    let n = f.sys.sleeps.borrow().len();
    let reads = |f: &Fx| f.methods().iter().filter(|m| *m == "Accessibility.getFullAXTree").count();
    let r0 = reads(&f);
    let (code, _, _) = run(&f, &["type", &tab, "--label", "No such field", "x", "--timeout", "10"]);
    assert_eq!(code, 1);
    let sleeps: Vec<f64> = f.sys.sleeps.borrow()[n..].to_vec();
    assert!((sleeps.iter().sum::<f64>() - 10.0).abs() < 1e-6, "{sleeps:?}");
    for (got, want) in sleeps.iter().zip([0.1, 0.2, 0.4, 0.8, 1.0, 1.0]) {
        assert!((got - want).abs() < 1e-9, "{sleeps:?}");
    }
    assert!(sleeps.iter().all(|s| *s <= 1.0 + 1e-9), "{sleeps:?}");
    // 0.1+0.2+0.4+0.8, eight 1 s waits, then the 0.5 s left: 14 polls in
    // 10 s (was 100 at a fixed 100 ms), each reading the 3 frames' trees
    // (page, same-process iframe, cross-origin iframe).
    assert_eq!(sleeps.len(), 13, "{sleeps:?}");
    assert_eq!(reads(&f) - r0, 3 * 14);
}

#[test]
fn click_waits_for_movement_and_refuses_disabled() {
    let (f, tab, page) = fx();
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 14).unwrap().moving = 4;
    let (code, _, err) = run(&f, &["click", &tab, "--role", "button", "--name", "Sign in"]);
    assert_eq!(code, 0, "{err}");
    page.st().frames[0].els.iter_mut().find(|e| e.backend == 44).unwrap().disabled = true;
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Accept all", "--timeout", "0.5"]);
    assert_eq!(code, 1);
    assert!(err.contains("is disabled; not clicking"), "{err}");
}

#[test]
fn double_click_sends_click_counts() {
    let (f, tab, page) = fx();
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Accept all", "--count", "2"]);
    assert_eq!(code, 0, "{err}");
    let counts: Vec<i64> = f
        .calls()
        .into_iter()
        .filter(|c| c.0 == "Input.dispatchMouseEvent" && c.1["type"] == "mousePressed")
        .map(|c| c.1["clickCount"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, vec![1, 2]);
    assert_eq!(page.st().mouse.iter().filter(|m| m.1 == "mouseReleased").count(), 2);
}

// ------------------------------------------------------------ iframes

#[test]
fn cross_origin_iframe_type_and_click() {
    let (f, tab, page) = fx();
    let main = format!("S-{tab}");
    // The bank iframe's password field is backend 4 too (another process).
    stdin(&f, SECRET.as_bytes());
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Bank password", "--stdin"]);
    assert_eq!(code, 0, "{err}");
    let v = json_out(&out);
    assert_eq!(v["typed"]["frame"], "https://bank.example");
    assert_eq!(page.frame_value(OOP_SESSION, 4), SECRET);
    assert_eq!(page.value(4), "", "the page's own password field is untouched");
    let ins = f.calls().into_iter().find(|c| c.0 == "Input.insertText").unwrap();
    assert_eq!(ins.2.as_deref(), Some(OOP_SESSION));
    assert_no_leak(&f, &out, &err, SECRET);
    // Click inside it: page coordinates = iframe content origin + local.
    let (code, out, err) = run(&f, &["click", &tab, "--text", "Continue"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["clicked"]["frame"], "https://bank.example");
    let press = page.st().mouse.iter().find(|m| m.1 == "mousePressed").cloned().unwrap();
    assert_eq!(press.0, main, "mouse events go to the page, not the iframe session");
    assert_eq!((press.2, press.3), (100.0 + 5.0 + 50.0, 800.0 + 40.0 + 10.0));
    // A modal in the *page* covering the iframe blocks the click.
    page.st().covers.push(Cover { session: main, rect: (0.0, 700.0, 1000.0, 400.0), backend: 98, desc: "<div#overlay> \"\"".into(), until: None, pe_none: false });
    let (code, _, err) = run(&f, &["click", &tab, "--text", "Continue", "--timeout", "0.5"]);
    assert_eq!(code, 1);
    assert!(err.contains("is covered by <div#overlay>"), "{err}");
}

#[test]
fn iframe_refs_never_collide_with_page_refs() {
    let (f, tab, page) = fx();
    let (code, snap, err) = run(&f, &["snapshot", &tab]);
    assert_eq!(code, 0, "{err}");
    let refs: Vec<String> = snap
        .lines()
        .filter(|l| l.contains("assword\""))
        .map(|l| l.split("[ref=").nth(1).unwrap().trim_end_matches(']').to_string())
        .collect();
    assert_eq!(refs.len(), 2, "{snap}");
    assert_ne!(refs[0], refs[1]);
    assert!(refs.iter().all(|r| r.ends_with(".4")), "both are backend 4 in their own process: {refs:?}");
    stdin(&f, b"bank-secret");
    let (code, out, err) = run(&f, &["type", &tab, "--ref", &refs[1], "--stdin"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_out(&out)["typed"]["frame"], "https://bank.example");
    assert_eq!(page.frame_value(OOP_SESSION, 4), "bank-secret");
    assert_eq!(page.value(4), "", "the page's own password field is untouched");
}

#[test]
fn css_runs_in_same_process_iframe_documents() {
    let (f, tab, page) = fx();
    let (code, out, err) = run(&f, &["type", &tab, "--css", "#i", "by-css"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(70), "by-css");
    let v = json_out(&out);
    assert_eq!(v["typed"]["name"], "Inner field");
    assert_eq!(v["typed"]["frame"], "http://127.0.0.1:1");
    let pushed = f.calls().into_iter().find(|c| c.0 == "DOM.pushNodesByBackendIdsToFrontend").unwrap();
    assert_eq!(pushed.1["backendNodeIds"], json!([5001]));
}

#[test]
fn same_process_iframe_field_is_found() {
    let (f, tab, page) = fx();
    let (code, out, err) = run(&f, &["type", &tab, "--label", "Inner field", "hello"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(page.value(70), "hello");
    assert_eq!(json_out(&out)["typed"]["frame"], "http://127.0.0.1:1");
}

#[test]
fn snapshot_shows_frames_and_filled_state_but_never_values() {
    let (f, tab, page) = fx();
    stdin(&f, SECRET.as_bytes());
    assert_eq!(run(&f, &["type", &tab, "--label", "Password", "--nth", "1", "--stdin"]).0, 0);
    run(&f, &["type", &tab, "--label", "Email address", "agent@example.com"]);
    for argv in [vec!["snapshot", tab.as_str()], vec!["snapshot", tab.as_str(), "--all"]] {
        let (code, out, err) = run(&f, &argv);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("- textbox \"Email address\" [filled]"), "{out}");
        assert!(out.contains("- textbox \"Verification code\" [empty]"), "{out}");
        assert!(out.contains("- button \"Sign in\""), "{out}");
        assert!(out.contains("[frame=http://127.0.0.1:1]"), "{out}");
        assert!(out.contains("[frame=https://bank.example]"), "{out}");
        assert!(out.contains("textbox \"Inner field\" [empty]"), "{out}");
        assert!(out.contains("button \"Continue\""), "{out}");
        assert!(!out.contains("agent@example.com"), "{out}");
        assert!(out.contains("?token=…"), "{out}");
        assert!(!out.contains("SECRET-ONE-TIME"), "{out}");
        assert_no_leak(&f, &out, &err, SECRET);
        // Iframe content is nested under its Iframe node.
        let lines: Vec<&str> = out.lines().collect();
        let i = lines.iter().position(|l| l.contains("[frame=https://bank.example]")).unwrap();
        let indent = |l: &str| l.len() - l.trim_start().len();
        assert!(indent(lines[i + 1]) > indent(lines[i]), "{out}");
    }
    let (_, out, _) = run(&f, &["snapshot", &tab, "--show-url"]);
    assert!(out.lines().next().unwrap().contains("token=SECRET-ONE-TIME"), "{out}");
    let _ = page;
}

#[test]
fn focus_emulation_is_switched_off_again_even_on_errors() {
    let (f, tab, page) = fx();
    run(&f, &["type", &tab, "--label", "Email address", "x"]);
    let (code, _, _) = run(&f, &["click", &tab, "--text", "Nope", "--timeout", "0.2"]);
    assert_eq!(code, 1);
    let main = format!("S-{tab}");
    let fe = page.st().focus_emulation.clone();
    let on = fe.iter().filter(|(s, e)| *s == main && *e).count();
    let off = fe.iter().filter(|(s, e)| *s == main && !*e).count();
    assert_eq!((on, off), (2, 2), "{fe:?}");
    assert!(!fe.last().unwrap().1);
    // In the iframe session too.
    stdin(&f, b"pw");
    assert_eq!(run(&f, &["type", &tab, "--label", "Bank password", "--stdin"]).0, 0);
    let fe = page.st().focus_emulation.clone();
    assert!(fe.contains(&(OOP_SESSION.to_string(), true)) && fe.contains(&(OOP_SESSION.to_string(), false)), "{fe:?}");
    // The off calls come before the session detaches.
    let methods = f.methods();
    let last_off = methods.iter().rposition(|m| m == "Emulation.setFocusEmulationEnabled").unwrap();
    let last_detach = methods.iter().rposition(|m| m == "Target.detachFromTarget").unwrap();
    assert!(last_off < last_detach);
}

impl FakePage {
    pub fn frame_value(&self, session: &str, backend: i64) -> String {
        let st = self.0.borrow();
        st.frames.iter().filter(|f| f.session == session).flat_map(|f| f.els.iter()).find(|e| e.backend == backend).unwrap().value.clone()
    }
}

// ------------------------------------------------------------ URLs

const TOKEN_URL: &str = "https://bank.example/device/approve?token=SECRET-ONE-TIME-abc123&lang=en";

#[test]
fn urls_are_redacted_in_open_goto_info_and_list() {
    let f = Fx::new(false);
    let opened = f.open(&[TOKEN_URL]);
    assert_eq!(opened["url"], "https://bank.example/device/approve?token=…&lang=…");
    let tab = opened["tab"].as_str().unwrap().to_string();
    let (code, out, _) = f.cli(&["goto", &tab, TOKEN_URL]);
    assert_eq!(code, 0);
    assert!(!out.contains("SECRET-ONE-TIME") && out.contains("token=\\u2026"), "{out}");
    let (_, out, _) = f.cli(&["info", &tab]);
    assert!(!out.contains("SECRET-ONE-TIME") && out.contains("token=…"), "{out}");
    let (_, out, _) = f.cli(&["list"]);
    assert!(!out.contains("SECRET-ONE-TIME") && out.contains("token=…"), "{out}");
    // --show-url prints them in full.
    for argv in [vec!["info", tab.as_str(), "--show-url"], vec!["list", "--show-url"], vec!["goto", tab.as_str(), TOKEN_URL, "--show-url"]] {
        let (_, out, _) = f.cli(&argv);
        assert!(out.contains("SECRET-ONE-TIME-abc123"), "{argv:?}: {out}");
    }
}

#[test]
fn list_redacts_titles_that_are_the_url_fallback() {
    let f = Fx::new(false);
    // Chromium's /json/list for pages without <title>: the URL, formatted.
    f.b.st().targets.push((
        "NOTITLE000000001".into(),
        json!({"type": "page", "url": TOKEN_URL, "title": "bank.example/device/approve?token=SECRET-ONE-TIME-abc123&amp;lang=en"}),
    ));
    f.b.st().targets.push((
        "DATAURL000000001".into(),
        json!({"type": "page", "url": "data:text/html,<p>SECRET-IN-DATA</p>", "title": "data:text/html,&lt;p&gt;SECRET-IN-DATA&lt;/p&gt;"}),
    ));
    let (code, out, err) = f.cli(&["list"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("SECRET"), "{out}");
    let v = json_out(&out);
    let title = |id: &str| v["tabs"].as_array().unwrap().iter().find(|t| t["tab"] == id).unwrap()["title"].clone();
    assert_eq!(title("NOTITLE000000001"), "bank.example/device/approve?token=…&lang=…");
    assert_eq!(title("DATAURL000000001"), "data:text/html,…");
    assert_eq!(title("OTHERTAB00000001"), "theirs");
    let (_, out, _) = f.cli(&["list", "--show-url"]);
    assert!(out.contains("token=SECRET-ONE-TIME-abc123&amp;lang=en"), "{out}");
}

#[test]
fn windows_redacts_url_titles_of_browser_windows() {
    let f = Fx::new(true);
    {
        let mut st = f.sw().st();
        st.wins.insert(
            201,
            Win { title: "bank.example/device/approve?token=SECRET-ONE-TIME-abc123&lang=en - Chromium".into(), output: "HEADLESS-1".into(), tab: None },
        );
        st.wins.insert(202, Win { title: "data:text/html,<p>SECRET in data</p> - Chromium".into(), output: "HEADLESS-1".into(), tab: None });
        st.wins.insert(203, Win { title: "Fake Bank login - Chromium".into(), output: "HEADLESS-1".into(), tab: None });
        // %20 in the query is shown as a space: not caught by "no spaces".
        st.wins.insert(
            204,
            Win { title: "bank.example/approve?scope=openid profile&token=SECRET-SPACED - Chromium".into(), output: "HEADLESS-1".into(), tab: None },
        );
    }
    let (code, out, err) = f.t.cli(&f.sys, &["windows"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("SECRET"), "{out}");
    let v = json_out(&out);
    let title = |id: i64| v.as_array().unwrap().iter().find(|w| w["id"] == id).unwrap()["title"].clone();
    assert_eq!(title(201), "bank.example/device/approve?token=…&lang=… - Chromium");
    assert_eq!(title(202), "data:text/html,… - Chromium");
    assert_eq!(title(203), "Fake Bank login - Chromium");
    assert_eq!(title(204), "bank.example/approve?scope=…&token=… - Chromium");
    assert_eq!(title(5), "theirs");
    // The window list is otherwise the Python's (id, app_id, title, pid, ...).
    let keys: Vec<&String> = v[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "app_id", "title", "pid", "focused", "rect"]);
    let (code, out, _) = f.t.cli(&f.sys, &["windows", "--show-url"]);
    assert_eq!(code, 0);
    assert!(out.contains("token=SECRET-ONE-TIME-abc123&lang=en - Chromium"), "{out}");
}

#[test]
fn navigation_errors_redact_the_url() {
    let f = Fx::new(false);
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.b.set_hook(|m, _| match m {
        "Page.navigate" => Hook::Return(Ok(json!({"errorText": "net::ERR_NAME_NOT_RESOLVED"}))),
        _ => Hook::Pass,
    });
    // Errors are redacted even with --show-url (it only governs output).
    let (code, out, err) = f.cli(&["goto", &tab, TOKEN_URL, "--show-url"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("navigate https://bank.example/device/approve?token=…&lang=…: net::ERR_NAME_NOT_RESOLVED"), "{err}");
    assert!(!err.contains("SECRET-ONE-TIME"));
    // `open URL` whose navigation fails: closes its tab, error redacted.
    let (code, out, err) = f.cli(&["open", TOKEN_URL]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(!err.contains("SECRET-ONE-TIME") && err.contains("token=…"), "{err}");
}

#[test]
fn urls_from_stdin_stay_out_of_argv_and_output() {
    let f = Fx::new(false);
    *f.sys.stdin.borrow_mut() = Some(format!("{TOKEN_URL}\n").into_bytes());
    let (code, out, err) = f.cli(&["open", "-"]);
    assert_eq!(code, 0, "{err}");
    let v = json_out(&out);
    assert_eq!(v["url"], "https://bank.example/device/approve?token=…&lang=…");
    let tab = v["tab"].as_str().unwrap().to_string();
    let nav = f.calls().into_iter().rfind(|c| c.0 == "Page.navigate").unwrap();
    assert_eq!(nav.1["url"], TOKEN_URL);
    *f.sys.stdin.borrow_mut() = Some(b"https://x.example/cb#access_token=zzz\n".to_vec());
    let (code, out, err) = f.cli(&["goto", &tab, "-"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("access_token=\\u2026") && !out.contains("zzz"), "{out}");
    let nav = f.calls().into_iter().rfind(|c| c.0 == "Page.navigate").unwrap();
    assert_eq!(nav.1["url"], "https://x.example/cb#access_token=zzz");
    // Two URLs, or none: refused.
    *f.sys.stdin.borrow_mut() = Some(b"https://a.example/ https://b.example/".to_vec());
    let (code, _, err) = f.cli(&["goto", &tab, "-"]);
    assert_eq!(code, 1);
    assert!(err.contains("exactly one URL"), "{err}");
    *f.sys.stdin.borrow_mut() = None;
    let (code, _, err) = f.cli(&["goto", &tab, "-"]);
    assert_eq!(code, 1);
    assert!(err.contains("stdin was empty"), "{err}");
}
