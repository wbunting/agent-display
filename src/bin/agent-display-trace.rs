//! Benchmark build: `agent-display` with every `Sys` operation and CDP call
//! timed. Prints `TRACE {json}` to stderr on exit (phase -> [ms...], plus
//! the in-process total). Built only with `--features bench-trace`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Instant;

use agent_display::cdp::{self, CdpConn};
use agent_display::ctx::{BrowserInfo, Ctx, Disp, Sys, Window};
use agent_display::util::{Environ, Result};
use agent_display::{browser, display, net, sway, systemd, tab};
use serde_json::Value;

#[derive(Default)]
struct Trace(RefCell<Vec<(String, f64)>>);

impl Trace {
    fn time<T>(&self, label: impl Into<String>, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let r = f();
        self.0.borrow_mut().push((label.into(), t.elapsed().as_secs_f64() * 1000.0));
        r
    }
}

struct TracingSys(Trace);

impl Sys for TracingSys {
    fn run(&self, ctx: &Ctx, argv: &[String], env: Option<&Environ>, timeout: Option<f64>, check: bool) -> Result<String> {
        let label = format!("exec {} {}", argv[0], argv.get(2).map(String::as_str).unwrap_or(""));
        self.0.time(label, || systemd::run_real(ctx, argv, env, timeout, check))
    }
    fn unit_active(&self, ctx: &Ctx, unit: &str) -> bool {
        systemd::unit_active(ctx, unit) // its systemctl is timed via run()
    }
    fn units_active(&self, ctx: &Ctx, units: &[String]) -> Vec<Option<bool>> {
        systemd::units_active(ctx, units)
    }
    fn unit_cgroup(&self, ctx: &Ctx, unit: &str) -> Option<String> {
        systemd::unit_cgroup(ctx, unit)
    }
    fn browsers(&self, ctx: &Ctx) -> Vec<BrowserInfo> {
        browser::browsers_real(ctx)
    }
    fn listener_in_unit(&self, ctx: &Ctx, port: u16, unit: &str) -> bool {
        // Make sure a cgroup lookup (cached from unit_active in normal runs)
        // is not counted as the /proc scan.
        let _ = ctx.sys.unit_cgroup(ctx, unit);
        self.0.time("listener check (/proc)", || net::listener_in_unit(ctx, port, unit))
    }
    fn listener_owner(&self, ctx: &Ctx, port: u16, unit: &str) -> Option<net::Owner> {
        let _ = ctx.sys.unit_cgroup(ctx, unit);
        self.0.time("listener scan (cgroup + /proc)", || net::listener_owner(ctx, port, unit))
    }
    fn owner_valid(&self, owner: &net::Owner, unit: &str) -> bool {
        self.0.time("owner re-check (/proc)", || net::owner_valid(owner, unit))
    }
    fn cdp_version(&self, port: u16, timeout: f64) -> Result<Value> {
        self.0.time("HTTP /json/version", || cdp::cdp_version(port, timeout))
    }
    fn cdp_targets(&self, port: u16, timeout: f64) -> Result<Vec<Value>> {
        self.0.time("HTTP /json/list", || cdp::cdp_targets(port, timeout))
    }
    fn cdp_connect(&self, ctx: &Ctx, port: u16, timeout: f64) -> Result<Box<dyn CdpConn>> {
        // Connect asks /json/version first (timed on its own); record the
        // TCP connect + WebSocket handshake as the remainder.
        let before = self.0.0.borrow().len();
        let t = Instant::now();
        let c = cdp::Cdp::connect(ctx, port, timeout);
        let total = t.elapsed().as_secs_f64() * 1000.0;
        let nested: f64 = self.0.0.borrow()[before..].iter().map(|(_, d)| d).sum();
        self.0.0.borrow_mut().push(("WS connect + handshake".into(), total - nested));
        Ok(Box::new(Timed { inner: Box::new(c?), trace: &self.0 as *const Trace }))
    }
    fn display(&self, ctx: &Ctx) -> Result<Disp> {
        self.0.time("display() validation", || display::display_real(ctx))
    }
    fn sway_json(&self, disp: &Disp, kind: &str) -> Result<Value> {
        self.0.time(format!("sway IPC {kind}"), || sway::sway_json(disp, kind))
    }
    fn sway_cmd(&self, disp: &Disp, command: &str) -> Result<()> {
        let word = command.split_whitespace().next().unwrap_or("").to_string();
        self.0.time(format!("sway IPC command ({word})"), || sway::sway_cmd(disp, command))
    }
    fn find_window(&self, ctx: &Ctx, disp: &Disp, title: &str, timeout: f64) -> Option<Window> {
        self.0.time("find_window (poll)", || tab::find_window_real(ctx, disp, title, timeout))
    }
    fn sleep(&self, secs: f64) {
        self.0.time("sleep", || std::thread::sleep(std::time::Duration::from_secs_f64(secs.max(0.0))))
    }
    fn mkdtemp(&self, prefix: &str, dir: &Path) -> Result<PathBuf> {
        net::mkdtemp(prefix, dir)
    }
}

struct Timed {
    inner: Box<dyn CdpConn>,
    trace: *const Trace,
}

impl Timed {
    fn t(&self) -> &Trace {
        // SAFETY: the Trace lives in the TracingSys for the whole process.
        unsafe { &*self.trace }
    }
}

impl CdpConn for Timed {
    fn port(&self) -> u16 {
        self.inner.port()
    }
    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64> {
        self.inner.send(method, params, session)
    }
    fn call(&mut self, method: &str, params: Value, session: Option<&str>, timeout: Option<f64>) -> Result<Value> {
        let t = Instant::now();
        let r = self.inner.call(method, params, session, timeout);
        self.t().0.borrow_mut().push((format!("CDP {method}"), t.elapsed().as_secs_f64() * 1000.0));
        r
    }
    fn wait_event(&mut self, method: &str, session: Option<&str>, timeout: Option<f64>) -> Result<Value> {
        let t = Instant::now();
        let r = self.inner.wait_event(method, session, timeout);
        self.t().0.borrow_mut().push((format!("wait {method}"), t.elapsed().as_secs_f64() * 1000.0));
        r
    }
    fn close(&mut self) {
        self.inner.close()
    }
}

fn main() {
    let t0 = Instant::now();
    let argv: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let sys = TracingSys(Trace::default());
    let ctx = Ctx::new(Environ::from_process(), &sys, Box::new(std::io::stdout()), Box::new(std::io::stderr()));
    let code = agent_display::main_with(&ctx, &argv);
    drop(ctx);
    let total = t0.elapsed().as_secs_f64() * 1000.0;
    let mut phases = serde_json::Map::new();
    for (k, v) in sys.0.0.borrow().iter() {
        phases.entry(k.clone()).or_insert_with(|| Value::Array(vec![])).as_array_mut().unwrap().push((*v).into());
    }
    eprintln!("TRACE {}", serde_json::json!({"total_ms": total, "phases": phases}));
    std::process::exit(code & 0xff);
}
