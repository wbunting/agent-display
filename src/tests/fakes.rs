//! Stubs: a temp HOME/runtime dir, `FakeSys`, a fake Chromium CDP and a
//! fake sway (outputs, windows, focus), mirroring the Python test doubles.

use std::cell::{RefCell, RefMut};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use crate::cdp::CdpConn;
use crate::ctx::*;
use crate::net::Owner;
use crate::show::{VncEvent, VncEvents};
use crate::util::{Environ, Error, Result, b64encode};

// ------------------------------------------------------------ temp home

pub struct TempHome {
    pub root: PathBuf,
    pub home: PathBuf,
    pub rt: PathBuf,
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

impl TempHome {
    pub fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let root = std::env::temp_dir().join(format!("adt-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(root.join("run")).unwrap();
        TempHome { home: root.join("home"), rt: root.join("run"), root }
    }

    pub fn env(&self) -> Environ {
        Environ(vec![
            ("HOME".into(), self.home.to_string_lossy().into()),
            ("XDG_RUNTIME_DIR".into(), self.rt.to_string_lossy().into()),
            ("PATH".into(), self.path()),
        ])
    }

    /// PATH: `bin/` here (fake tools from `fake_tool`) before the real one.
    pub fn path(&self) -> String {
        format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_else(|_| "/usr/bin".into()))
    }

    /// An executable stub named `name` on PATH (never run by the tests).
    pub fn fake_tool(&self, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn ctx<'a>(&self, sys: &'a FakeSys) -> Ctx<'a> {
        Ctx::new(self.env(), sys, Box::new(std::io::sink()), Box::new(std::io::sink()))
    }

    /// Run the CLI; (exit code, stdout, stderr).
    pub fn cli(&self, sys: &FakeSys, argv: &[&str]) -> (i32, String, String) {
        let out = Buf::default();
        let err = Buf::default();
        let ctx = Ctx::new(self.env(), sys, Box::new(out.clone()), Box::new(err.clone()));
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let code = crate::main_with(&ctx, &argv);
        drop(ctx);
        (code, out.text(), err.text())
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Default)]
pub struct Buf(Rc<RefCell<Vec<u8>>>);
impl Buf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.borrow()).into_owned()
    }
}
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------ FakeSys

type ListenerFn = Box<dyn Fn(u16, &str) -> bool>;
type UnitFn = Box<dyn Fn(&str) -> bool>;
type HookFn = Box<dyn FnMut(&str, &Value) -> Hook>;
type OwnerFn = Box<dyn Fn(&Owner) -> bool>;

/// `None` fields fall back to inert defaults (nothing active, no
/// listener), never to the real system.
#[derive(Default)]
pub struct FakeSys {
    pub active: Option<bool>,
    pub active_seq: RefCell<VecDeque<bool>>,
    pub active_fn: Option<UnitFn>,
    pub browsers: RefCell<Option<Vec<BrowserInfo>>>,
    pub listener: Option<ListenerFn>,
    /// `Some`: unit_cgroup answers this and listener_in_unit runs for real.
    pub cgroup: Option<Option<String>>,
    pub pid_in_unit: Option<bool>,
    pub pid_checks: RefCell<Vec<(u32, String)>>,
    pub port_free: Option<bool>,
    pub port_owner: Option<String>,
    pub version_calls: RefCell<usize>,
    pub version_timeouts: RefCell<Vec<f64>>,
    pub targets_timeouts: RefCell<Vec<f64>>,
    pub version_fail: Option<Box<dyn Fn(u16) -> Option<Error>>>,
    pub browser: Option<FakeBrowser>,
    pub sway: Option<FakeSway>,
    pub display: Option<Result<Disp>>,
    pub runs: RefCell<Vec<(Vec<String>, Option<Environ>)>>,
    pub systemd_runs: RefCell<Vec<Vec<String>>>,
    pub stopped: RefCell<Vec<Vec<String>>>,
    pub sleeps: RefCell<Vec<f64>>,
    pub hide_window: RefCell<bool>,
    pub mkdtemp_in: Option<PathBuf>,
    pub made: RefCell<Vec<PathBuf>>,
    /// systemctl can't answer: every unit's state is unknown.
    pub states_unknown: RefCell<bool>,
    /// Answer to `owner_valid` (the /proc re-check); default: never valid.
    pub owner_ok: Option<OwnerFn>,
    pub owner_checks: RefCell<usize>,
    /// Full listener scans (systemd + cgroup), i.e. cache misses.
    pub listener_scans: RefCell<usize>,
    /// Scripted wayvnc viewer events (then Closed); `vnc_events` calls.
    pub vnc_script: RefCell<VecDeque<VncEvent>>,
    pub vnc_opened: RefCell<Vec<PathBuf>>,
    /// Unix sockets "wayvnc" made (systemd_run of a --unix-socket argv).
    pub sockets: RefCell<Vec<std::os::unix::net::UnixListener>>,
}

pub fn info(unit: &str, port: i64) -> BrowserInfo {
    let Value::Object(m) = json!({"unit": unit, "port": port}) else { unreachable!() };
    BrowserInfo(m)
}

impl Sys for FakeSys {
    fn run(&self, _ctx: &Ctx, argv: &[String], env: Option<&Environ>, _t: Option<f64>, _check: bool) -> Result<String> {
        self.runs.borrow_mut().push((argv.to_vec(), env.cloned()));
        Ok(String::new())
    }
    fn unit_active(&self, _ctx: &Ctx, unit: &str) -> bool {
        if let Some(v) = self.active_seq.borrow_mut().pop_front() {
            return v;
        }
        if let Some(f) = &self.active_fn {
            return f(unit);
        }
        self.active.unwrap_or(false)
    }
    fn units_active(&self, ctx: &Ctx, units: &[String]) -> Vec<Option<bool>> {
        if *self.states_unknown.borrow() {
            return vec![None; units.len()];
        }
        units.iter().map(|u| Some(self.unit_active(ctx, u))).collect()
    }
    fn stop_units(&self, _ctx: &Ctx, units: &[String]) -> Result<()> {
        if !units.is_empty() {
            self.stopped.borrow_mut().push(units.to_vec());
        }
        Ok(())
    }
    fn systemd_run(&self, _: &Ctx, unit: &str, argv: &[String], _: bool, _: Option<&[String]>, _: Option<&str>) -> Result<()> {
        let mut v = vec![unit.to_string()];
        v.extend(argv.iter().cloned());
        self.systemd_runs.borrow_mut().push(v);
        // A started wayvnc makes its Unix socket.
        if let Some(i) = argv.iter().position(|a| a == "--unix-socket") {
            let l = std::os::unix::net::UnixListener::bind(&argv[i + 1]).unwrap();
            self.sockets.borrow_mut().push(l);
        }
        Ok(())
    }
    fn browsers(&self, ctx: &Ctx) -> Vec<BrowserInfo> {
        match &*self.browsers.borrow() {
            Some(b) => b.clone(),
            None => crate::browser::browsers_real(ctx),
        }
    }
    fn listener_in_unit(&self, ctx: &Ctx, port: u16, unit: &str) -> bool {
        if self.cgroup.is_some() {
            return crate::net::listener_in_unit(ctx, port, unit);
        }
        self.listener.as_ref().is_some_and(|f| f(port, unit))
    }
    fn listener_owner(&self, ctx: &Ctx, port: u16, unit: &str) -> Option<Owner> {
        *self.listener_scans.borrow_mut() += 1;
        if self.cgroup.is_some() {
            return crate::net::listener_owner(ctx, port, unit);
        }
        self.listener.as_ref().is_some_and(|f| f(port, unit)).then_some(Owner { port, pid: 4242, start_time: 1, inode: 7 })
    }
    fn owner_valid(&self, owner: &Owner, _unit: &str) -> bool {
        *self.owner_checks.borrow_mut() += 1;
        self.owner_ok.as_ref().is_some_and(|f| f(owner))
    }
    fn vnc_events(&self, _ctx: &Ctx, ctl: &Path) -> Result<Box<dyn VncEvents>> {
        self.vnc_opened.borrow_mut().push(ctl.to_path_buf());
        Ok(Box::new(FakeVnc(std::mem::take(&mut *self.vnc_script.borrow_mut()))))
    }
    fn unit_cgroup(&self, _ctx: &Ctx, _unit: &str) -> Option<String> {
        self.cgroup.clone().flatten()
    }
    fn pid_in_unit(&self, pid: u32, unit: &str) -> bool {
        self.pid_checks.borrow_mut().push((pid, unit.to_string()));
        self.pid_in_unit.unwrap_or(false)
    }
    fn port_free(&self, port: u16) -> bool {
        self.port_free.unwrap_or_else(|| crate::net::port_free(port))
    }
    fn port_owner(&self, _ctx: &Ctx, _port: u16) -> String {
        self.port_owner.clone().unwrap_or_else(|| "unknown process".into())
    }
    fn cdp_version(&self, port: u16, timeout: f64) -> Result<Value> {
        *self.version_calls.borrow_mut() += 1;
        self.version_timeouts.borrow_mut().push(timeout);
        if let Some(e) = self.version_fail.as_ref().and_then(|f| f(port)) {
            return Err(e);
        }
        Ok(json!({}))
    }
    fn cdp_targets(&self, _port: u16, timeout: f64) -> Result<Vec<Value>> {
        self.targets_timeouts.borrow_mut().push(timeout);
        match &self.browser {
            Some(b) => b.list(),
            None => Ok(vec![]),
        }
    }
    fn cdp_connect(&self, _ctx: &Ctx, port: u16, timeout: f64) -> Result<Box<dyn CdpConn>> {
        // Like Cdp::connect: /json/version first (where stalls show up).
        self.cdp_version(port, timeout)?;
        let b = self.browser.clone().ok_or_else(|| Error::os("[Errno 111] Connection refused"))?;
        if b.st().fail_connect {
            return Err(Error::os("[Errno 111] Connection refused"));
        }
        Ok(Box::new(FakeConn { b, port, default: timeout }))
    }
    fn display(&self, ctx: &Ctx) -> Result<Disp> {
        match &self.display {
            Some(r) => r.clone(),
            None => crate::display::display_real(ctx),
        }
    }
    fn sway_json(&self, _disp: &Disp, kind: &str) -> Result<Value> {
        self.sway.as_ref().expect("sway").json(kind)
    }
    fn sway_cmd(&self, _disp: &Disp, command: &str) -> Result<()> {
        self.sway.as_ref().expect("sway").cmd(command)
    }
    fn find_window(&self, ctx: &Ctx, disp: &Disp, title: &str, _timeout: f64) -> Option<Window> {
        if *self.hide_window.borrow() {
            return None;
        }
        crate::display::windows(ctx, disp).ok()?.into_iter().find(|w| w.title_str().starts_with(title))
    }
    fn sleep(&self, secs: f64) {
        self.sleeps.borrow_mut().push(secs);
    }
    fn mkdtemp(&self, prefix: &str, dir: &Path) -> Result<PathBuf> {
        let p = crate::net::mkdtemp(prefix, self.mkdtemp_in.as_deref().unwrap_or(dir))?;
        self.made.borrow_mut().push(p.clone());
        Ok(p)
    }
}

pub struct FakeVnc(pub VecDeque<VncEvent>);
impl VncEvents for FakeVnc {
    fn next(&mut self, _timeout: f64) -> VncEvent {
        self.0.pop_front().unwrap_or(VncEvent::Closed)
    }
}

// ------------------------------------------------------------ FakeBrowser

/// What a test hook does with a CDP call.
pub enum Hook {
    Pass,
    Return(Result<Value>),
    /// Run the default handler (e.g. Chromium creates the tab late), then fail.
    DefaultThen(Error),
}

pub type Call = (String, Value, Option<String>);

pub struct BrowserState {
    pub targets: Vec<(String, Value)>,
    pub calls: Vec<Call>,
    pub timeouts: Vec<(String, Option<f64>)>,
    /// (method, timeout it ran with: its own or the connection default).
    pub effective: Vec<(String, f64)>,
    pub opened: usize,
    pub eval_result: Value,
    pub hook: Option<HookFn>,
    pub sway: Option<FakeSway>,
    pub fail_connect: bool,
}

#[derive(Clone)]
pub struct FakeBrowser(pub Rc<RefCell<BrowserState>>);

impl FakeBrowser {
    pub fn new() -> Self {
        FakeBrowser(Rc::new(RefCell::new(BrowserState {
            targets: vec![
                ("OTHERTAB00000001".into(), json!({"type": "page", "url": "https://a/", "title": "theirs"})),
                ("FRAME00000000001".into(), json!({"type": "iframe", "url": "x", "title": ""})),
            ],
            calls: vec![],
            timeouts: vec![],
            effective: vec![],
            opened: 0,
            eval_result: json!({"result": {"type": "string", "value": "Example"}}),
            hook: None,
            sway: None,
            fail_connect: false,
        })))
    }

    pub fn st(&self) -> RefMut<'_, BrowserState> {
        self.0.borrow_mut()
    }

    pub fn methods(&self) -> Vec<String> {
        self.0.borrow().calls.iter().map(|c| c.0.clone()).collect()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.borrow().targets.iter().map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    pub fn has(&self, id: &str) -> bool {
        self.0.borrow().targets.iter().any(|(k, _)| k == id)
    }

    pub fn list(&self) -> Result<Vec<Value>> {
        Ok(self
            .0
            .borrow()
            .targets
            .iter()
            .map(|(id, v)| {
                let mut m = serde_json::Map::new();
                m.insert("id".into(), id.clone().into());
                m.extend(v.as_object().unwrap().clone());
                Value::Object(m)
            })
            .collect())
    }

    pub fn set_hook(&self, f: impl FnMut(&str, &Value) -> Hook + 'static) {
        self.st().hook = Some(Box::new(f));
    }

    fn handle(&self, method: &str, params: &Value, session: Option<&str>) -> Result<Value> {
        let mut st = self.st();
        let target_of = |s: Option<&str>| s.unwrap_or("").trim_start_matches("S-").to_string();
        match method {
            "Target.createTarget" => {
                st.opened += 1;
                let tab = format!("MINE{:012}", st.opened);
                st.targets.push((tab.clone(), json!({"type": "page", "url": params["url"], "title": ""})));
                let sway = st.sway.clone();
                drop(st);
                if let Some(s) = sway {
                    s.created(&tab, params);
                }
                Ok(json!({"targetId": tab}))
            }
            "Target.attachToTarget" => Ok(json!({"sessionId": format!("S-{}", params["targetId"].as_str().unwrap())})),
            "Target.closeTarget" => {
                let id = params["targetId"].as_str().unwrap().to_string();
                st.targets.retain(|(k, _)| *k != id);
                let sway = st.sway.clone();
                drop(st);
                if let Some(s) = sway {
                    s.closed(&id);
                }
                Ok(json!({"success": true}))
            }
            "Page.navigate" => {
                let id = target_of(session);
                if let Some((_, t)) = st.targets.iter_mut().find(|(k, _)| *k == id) {
                    t["url"] = params["url"].clone();
                }
                Ok(json!({"frameId": "F", "loaderId": "L"}))
            }
            "Runtime.evaluate" => {
                if params["expression"].as_str().unwrap().starts_with("({url") {
                    let id = target_of(session);
                    let t = st.targets.iter().find(|(k, _)| *k == id).map(|(_, v)| v.clone()).unwrap();
                    return Ok(json!({"result": {"type": "object", "value": {"url": t["url"], "title": t["title"]}}}));
                }
                Ok(st.eval_result.clone())
            }
            "Target.getTargetInfo" => {
                let id = params["targetId"].as_str().unwrap_or("");
                match st.targets.iter().find(|(k, _)| k == id) {
                    Some((k, v)) => Ok(json!({"targetInfo": {
                        "targetId": k, "type": v["type"], "title": v["title"], "url": v["url"], "attached": false}})),
                    None => Err(Error::new("Target.getTargetInfo: No target with given id found")),
                }
            }
            "Page.captureScreenshot" => Ok(json!({"data": b64encode(b"\x89PNG fake")})),
            "Page.getLayoutMetrics" => Ok(json!({
                "cssLayoutViewport": {"clientWidth": 800, "clientHeight": 600},
                "cssContentSize": {"height": 5000.5}
            })),
            _ => Ok(json!({})),
        }
    }

    fn dispatch(&self, method: &str, params: Value, session: Option<&str>, timeout: Option<f64>) -> Result<Value> {
        {
            let mut st = self.st();
            st.calls.push((method.to_string(), params.clone(), session.map(String::from)));
            st.timeouts.push((method.to_string(), timeout));
        }
        let hook = self.st().hook.take();
        let decision = match hook {
            Some(mut h) => {
                let d = h(method, &params);
                self.st().hook = Some(h);
                d
            }
            None => Hook::Pass,
        };
        match decision {
            Hook::Pass => self.handle(method, &params, session),
            Hook::Return(r) => r,
            Hook::DefaultThen(e) => {
                let _ = self.handle(method, &params, session);
                Err(e)
            }
        }
    }
}

pub struct FakeConn {
    b: FakeBrowser,
    port: u16,
    /// Default call timeout (as Cdp::timeout): recorded when a call has none.
    default: f64,
}

impl CdpConn for FakeConn {
    fn port(&self) -> u16 {
        self.port
    }
    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64> {
        self.b.st().calls.push((method.to_string(), params, session.map(String::from)));
        Ok(1)
    }
    fn call(&mut self, method: &str, params: Value, session: Option<&str>, timeout: Option<f64>) -> Result<Value> {
        self.b.st().effective.push((method.to_string(), timeout.unwrap_or(self.default)));
        self.b.dispatch(method, params, session, timeout)
    }
    fn set_default_timeout(&mut self, timeout: f64) {
        self.default = timeout;
    }
    fn wait_event(&mut self, method: &str, session: Option<&str>, _timeout: Option<f64>) -> Result<Value> {
        self.b.st().calls.push((format!("wait:{method}"), Value::Null, session.map(String::from)));
        Ok(json!({"method": method}))
    }
    fn close(&mut self) {}
}

// ------------------------------------------------------------ FakeSway

#[derive(Clone, Debug)]
pub struct Win {
    pub title: String,
    pub output: String,
    pub tab: Option<String>,
}

pub struct SwayState {
    pub outputs: Vec<String>,
    pub next_output: u32,
    pub wins: BTreeMap<i64, Win>,
    pub focus: Option<i64>,
    pub next_id: i64,
    pub commands: Vec<String>,
    pub fail_create: bool,
    pub focused_output: Option<String>,
    pub sizes: HashMap<String, (i64, i64)>,
    pub fail_mode: bool,
    pub hide_new: bool,
    pub fail_tree: bool,
}

#[derive(Clone)]
pub struct FakeSway(pub Rc<RefCell<SwayState>>);

impl FakeSway {
    pub fn new() -> Self {
        FakeSway(Rc::new(RefCell::new(SwayState {
            outputs: vec!["HEADLESS-1".into()],
            next_output: 2,
            wins: BTreeMap::from([(5, Win { title: "theirs".into(), output: "HEADLESS-1".into(), tab: None })]),
            focus: Some(5),
            next_id: 100,
            commands: vec![],
            fail_create: false,
            focused_output: None,
            sizes: HashMap::from([("HEADLESS-1".to_string(), (1280, 800))]),
            fail_mode: false,
            hide_new: false,
            fail_tree: false,
        })))
    }

    pub fn st(&self) -> RefMut<'_, SwayState> {
        self.0.borrow_mut()
    }

    fn tree(&self) -> Value {
        let st = self.0.borrow();
        let con = |i: i64, w: &Win| {
            json!({"type": "con", "id": i, "pid": 1, "name": w.title, "app_id": "chromium",
                   "focused": Some(i) == st.focus, "rect": {}, "nodes": [], "floating_nodes": []})
        };
        let mut nodes = vec![json!({"type": "output", "name": "__i3", "nodes": []})];
        for (n, o) in st.outputs.iter().enumerate() {
            let cons: Vec<Value> = st.wins.iter().filter(|(_, w)| w.output == *o).map(|(i, w)| con(*i, w)).collect();
            nodes.push(json!({"type": "output", "name": o, "nodes": [
                {"type": "workspace", "name": (n + 1).to_string(), "floating_nodes": [], "nodes": cons}]}));
        }
        json!({"nodes": nodes})
    }

    pub fn json(&self, kind: &str) -> Result<Value> {
        if kind == "get_tree" {
            if self.0.borrow().fail_tree {
                return Err(Error::new("swaymsg: broken pipe"));
            }
            return Ok(self.tree());
        }
        let st = self.0.borrow();
        Ok(Value::Array(
            st.outputs
                .iter()
                .map(|o| {
                    let (w, h) = st.sizes[o];
                    json!({"name": o, "rect": {"width": w, "height": h}, "current_mode": {"width": w, "height": h}})
                })
                .collect(),
        ))
    }

    pub fn cmd(&self, command: &str) -> Result<()> {
        let mut st = self.st();
        st.commands.push(command.to_string());
        let con = command.strip_prefix("[con_id=").and_then(|r| r.split_once("] ")).map(|(id, rest)| (id.parse::<i64>().unwrap(), rest.to_string()));
        if command == "create_output" {
            if st.fail_create {
                return Err(Error::new("sway rejected create_output"));
            }
            let name = format!("HEADLESS-{}", st.next_output);
            st.outputs.push(name.clone());
            st.sizes.insert(name, (1920, 1080)); // wlroots default
            st.next_output += 1;
        } else if let Some(rest) = command.strip_prefix("output ").filter(|r| r.contains(" mode --custom ")) {
            if st.fail_mode {
                return Err(Error::new("sway rejected mode"));
            }
            let (name, size) = rest.split_once(" mode --custom ").unwrap();
            let (w, h) = size.split_once('x').unwrap();
            st.sizes.insert(name.to_string(), (w.parse().unwrap(), h.parse().unwrap()));
        } else if let Some((id, rest)) = &con {
            if rest == "focus" {
                st.focus = Some(*id);
            } else if let Some(o) = rest.strip_prefix("move container to output ") {
                st.wins.get_mut(id).unwrap().output = o.to_string();
            } else {
                return Err(Error::new(format!("unexpected sway command {command:?}")));
            }
        } else if let Some(o) = command.strip_prefix("focus output ") {
            st.focused_output = Some(o.to_string());
        } else if let Some(name) = command.strip_prefix("output ").and_then(|r| r.strip_suffix(" unplug")) {
            let name = name.to_string();
            for w in st.wins.values_mut() {
                if w.output == name {
                    w.output = "HEADLESS-1".into();
                }
            }
            st.outputs.retain(|o| *o != name);
        } else {
            return Err(Error::new(format!("unexpected sway command {command:?}")));
        }
        Ok(())
    }

    pub fn created(&self, tab: &str, params: &Value) {
        let mut st = self.st();
        if st.hide_new {
            return;
        }
        let url = params["url"].as_str().unwrap();
        let title = url.split("<title>").nth(1).unwrap().split("</title>").next().unwrap();
        st.next_id += 1;
        let id = st.next_id;
        st.wins.insert(id, Win { title: format!("{title} - Chromium"), output: "HEADLESS-1".into(), tab: Some(tab.into()) });
        st.focus = Some(id); // sway focuses new windows
    }

    pub fn closed(&self, tab: &str) {
        let mut st = self.st();
        let ids: Vec<i64> = st.wins.iter().filter(|(_, w)| w.tab.as_deref() == Some(tab)).map(|(i, _)| *i).collect();
        for i in ids {
            st.wins.remove(&i);
            if st.focus == Some(i) {
                st.focus = Some(5);
            }
        }
    }

    pub fn window_of(&self, tab: &str) -> Win {
        self.0.borrow().wins.values().find(|w| w.tab.as_deref() == Some(tab)).unwrap().clone()
    }
}
