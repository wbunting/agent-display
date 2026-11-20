//! Process context (environment, output streams) and `Sys`, the seam for
//! everything the Python unit tests stub: systemd, sockets, CDP and sway.
//! `RealSys` uses the default methods; tests override what they need.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cdp::{self, CdpConn};
use crate::util::{Environ, Result};
use crate::{browser, display, net, sway, systemd};

pub const DISPLAY_UNIT: &str = "agent-display.service";
pub const BROWSER_UNIT: &str = "agent-display-browser";
pub const VNC_UNIT: &str = "agent-display-vnc";
pub const APP_PREFIX: &str = "agent-display-app-";
pub const OUTPUT: &str = "HEADLESS-1";
pub const DEFAULT_SIZE: &str = "1920x1080";
/// Persistent agent profile's CDP port. Never 9222 (Chromium's usual
/// default, which other tools assume) or 9333 (the user's vuon-outbound).
pub const DEFAULT_CDP_PORT: u16 = 9425;
pub const RESERVED_PORTS: &[(u16, &str)] = &[
    (9222, "conventional default CDP port, used by other tools"),
    (9333, "the user's vuon-outbound browser"),
];
pub const DEFAULT_VNC_PORT: u16 = 5910;
/// The user's own wayvnc for Hyprland listens on 5900.
pub const RESERVED_VNC_PORTS: &[(u16, &str)] = &[(5900, "the user's own wayvnc")];
pub const CHROMIUM: &str = "/usr/lib/chromium/chromium"; // not /usr/bin/chromium: see SKILL.md
/// Variables that point a process at the user's desktop session.
pub const BLOCKED_ENV: &[&str] = &[
    "DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE", "HYPRLAND_CMD",
    "SWAYSOCK", "I3SOCK", "DBUS_SESSION_BUS_ADDRESS", "XAUTHORITY", "DESKTOP_STARTUP_ID",
    "XDG_ACTIVATION_TOKEN",
];
pub const PASS_ENV: &[&str] =
    &["HOME", "USER", "LOGNAME", "PATH", "LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "TZ", "SHELL"];

pub fn reserved_reason(port: i64) -> Option<&'static str> {
    RESERVED_PORTS.iter().find(|(p, _)| *p as i64 == port).map(|(_, why)| *why)
}

pub fn install_hint(tool: &str) -> &'static str {
    match tool {
        "sway" => "install with: sudo pacman -S --needed sway",
        "swaymsg" => "installed with sway",
        "dbus-run-session" => "from the dbus package",
        "systemd-run" => "from systemd",
        "grim" => "needed by screenshot",
        "wtype" => "needed by type/key",
        "wayvnc" => "needed by view",
        _ => "not installed",
    }
}

/// The validated agent display (see `display::display`).
#[derive(Debug, Clone, PartialEq)]
pub struct Disp {
    pub runtime: String,
    pub wayland: String,
    pub swaysock: String,
    pub bus: String,
}

impl Disp {
    /// The variables clients get, in the Python's dict order.
    pub fn env(&self) -> Environ {
        Environ(vec![
            ("XDG_RUNTIME_DIR".into(), self.runtime.clone()),
            ("WAYLAND_DISPLAY".into(), self.wayland.clone()),
            ("SWAYSOCK".into(), self.swaysock.clone()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), self.bus.clone()),
            ("XDG_SESSION_TYPE".into(), "wayland".into()),
            ("XDG_CURRENT_DESKTOP".into(), "sway".into()),
        ])
    }
}

/// A registered agent browser (`browsers/<unit>.json`), kept as the JSON
/// object so other keys and their order survive like in the Python.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserInfo(pub serde_json::Map<String, Value>);

impl BrowserInfo {
    pub fn unit(&self) -> &str {
        self.0.get("unit").and_then(Value::as_str).unwrap_or("")
    }
    /// The port if it is an integer (Python: `isinstance(port, int)`).
    pub fn port(&self) -> Option<i64> {
        self.0.get("port").and_then(Value::as_i64)
    }
    pub fn profile(&self) -> String {
        match self.0.get("profile") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }
    pub fn temp(&self) -> bool {
        self.0.get("temp").map(truthy).unwrap_or(false)
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub id: i64,
    pub app_id: Value,
    pub title: Value,
    pub pid: Value,
    pub focused: bool,
    pub rect: Value,
}

impl Window {
    pub fn title_str(&self) -> &str {
        self.title.as_str().unwrap_or("")
    }
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), self.id.into());
        m.insert("app_id".into(), self.app_id.clone());
        m.insert("title".into(), self.title.clone());
        m.insert("pid".into(), self.pid.clone());
        m.insert("focused".into(), self.focused.into());
        m.insert("rect".into(), self.rect.clone());
        Value::Object(m)
    }
}

/// Everything with side effects outside this process. Defaults are real.
pub trait Sys {
    /// `run()`: a captured subprocess (`timeout` None waits forever).
    fn run(&self, ctx: &Ctx, argv: &[String], env: Option<&Environ>, timeout: Option<f64>, check: bool) -> Result<String> {
        systemd::run_real(ctx, argv, env, timeout, check)
    }
    fn unit_active(&self, ctx: &Ctx, unit: &str) -> bool {
        systemd::unit_active(ctx, unit)
    }
    /// Several units in one systemctl call.
    fn units_active(&self, ctx: &Ctx, units: &[String]) -> Vec<bool> {
        systemd::units_active(ctx, units)
    }
    /// The unit's cgroup path (`ControlGroup`), if it has one.
    fn unit_cgroup(&self, ctx: &Ctx, unit: &str) -> Option<String> {
        systemd::unit_cgroup(ctx, unit)
    }
    fn stop_units(&self, ctx: &Ctx, units: &[String]) -> Result<()> {
        systemd::stop_units(ctx, units)
    }
    fn systemd_run(&self, ctx: &Ctx, unit: &str, argv: &[String], bind: bool, stop_post: Option<&[String]>, description: Option<&str>) -> Result<()> {
        systemd::systemd_run(ctx, unit, argv, bind, stop_post, description)
    }
    fn browsers(&self, ctx: &Ctx) -> Vec<BrowserInfo> {
        browser::browsers_real(ctx)
    }
    /// The LISTEN socket on `port` is held by a process inside `unit`.
    fn listener_in_unit(&self, ctx: &Ctx, port: u16, unit: &str) -> bool {
        net::listener_in_unit(ctx, port, unit)
    }
    fn pid_in_unit(&self, pid: u32, unit: &str) -> bool {
        net::pid_in_unit(pid, unit)
    }
    fn port_free(&self, port: u16) -> bool {
        net::port_free(port)
    }
    fn port_owner(&self, ctx: &Ctx, port: u16) -> String {
        net::port_owner(ctx, port)
    }
    fn cdp_version(&self, port: u16, timeout: f64) -> Result<Value> {
        cdp::cdp_version(port, timeout)
    }
    fn cdp_targets(&self, port: u16, timeout: f64) -> Result<Vec<Value>> {
        cdp::cdp_targets(port, timeout)
    }
    fn cdp_connect(&self, ctx: &Ctx, port: u16, timeout: f64) -> Result<Box<dyn CdpConn>> {
        cdp::Cdp::connect(ctx, port, timeout).map(|c| Box::new(c) as Box<dyn CdpConn>)
    }
    fn display(&self, ctx: &Ctx) -> Result<Disp> {
        display::display_real(ctx)
    }
    fn sway_json(&self, disp: &Disp, kind: &str) -> Result<Value> {
        sway::sway_json(disp, kind)
    }
    fn sway_cmd(&self, disp: &Disp, command: &str) -> Result<()> {
        sway::sway_cmd(disp, command)
    }
    fn find_window(&self, ctx: &Ctx, disp: &Disp, title: &str, timeout: f64) -> Option<Window> {
        crate::tab::find_window_real(ctx, disp, title, timeout)
    }
    fn sleep(&self, secs: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(secs.max(0.0)));
    }
    fn mkdtemp(&self, prefix: &str, dir: &Path) -> Result<PathBuf> {
        net::mkdtemp(prefix, dir)
    }
}

pub struct RealSys;
impl Sys for RealSys {}

/// One invocation: environment snapshot, the `Sys` seam and output.
pub struct Ctx<'a> {
    pub env: Environ,
    pub sys: &'a dyn Sys,
    pub out: RefCell<Box<dyn Write + 'a>>,
    pub err: RefCell<Box<dyn Write + 'a>>,
    pub uid: u32,
    pub hostname: String,
    /// This executable (the sway config's `exec ... hold-input`).
    pub exe: PathBuf,
    /// Per unit, learned from systemctl in this process: cgroup path (fixed
    /// by the unit name; its pids are re-read on every check) and main pid.
    pub units: RefCell<std::collections::HashMap<String, systemd::UnitInfo>>,
}

impl<'a> Ctx<'a> {
    pub fn new(env: Environ, sys: &'a dyn Sys, out: Box<dyn Write + 'a>, err: Box<dyn Write + 'a>) -> Self {
        Ctx {
            env,
            sys,
            out: RefCell::new(out),
            err: RefCell::new(err),
            uid: crate::util::getuid(),
            hostname: crate::util::hostname(),
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agent-display")),
            units: RefCell::new(Default::default()),
        }
    }

    pub fn println(&self, s: &str) {
        let mut o = self.out.borrow_mut();
        let _ = o.write_all(s.as_bytes());
        let _ = o.write_all(b"\n");
        let _ = o.flush();
    }

    pub fn eprintln(&self, s: &str) {
        let mut e = self.err.borrow_mut();
        let _ = e.write_all(s.as_bytes());
        let _ = e.write_all(b"\n");
        let _ = e.flush();
    }

    pub fn home(&self) -> PathBuf {
        PathBuf::from(self.env.get("HOME").unwrap_or("/"))
    }

    /// The user's runtime dir, even if XDG_RUNTIME_DIR is the agent one.
    pub fn runtime_root(&self) -> PathBuf {
        let path = match self.env.get_nonempty("XDG_RUNTIME_DIR") {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(format!("/run/user/{}", self.uid)),
        };
        if path.file_name().is_some_and(|n| n == "agent-display") {
            path.parent().map(Path::to_path_buf).unwrap_or(path)
        } else {
            path
        }
    }

    /// Private runtime dir: holds the Wayland, sway IPC and D-Bus sockets.
    pub fn runtime_dir(&self) -> PathBuf {
        self.runtime_root().join("agent-display")
    }

    pub fn state_dir(&self) -> PathBuf {
        let base = match self.env.get_nonempty("XDG_STATE_HOME") {
            Some(b) => PathBuf::from(b),
            None => self.home().join(".local/state"),
        };
        base.join("agent-display")
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.state_dir().join("browser-profile")
    }

    pub fn browsers_dir(&self) -> PathBuf {
        self.runtime_dir().join("browsers")
    }

    pub fn tab_outputs_dir(&self) -> PathBuf {
        self.runtime_dir().join("tab-outputs")
    }

    /// Caller environment minus anything that points at the user's desktop.
    pub fn clean_env(&self) -> Environ {
        let mut env = self.env.clone();
        for k in BLOCKED_ENV {
            env.remove(k);
        }
        env
    }

    /// Caller env for systemctl/journalctl: user runtime dir, no agent bus.
    pub fn manager_env(&self) -> Environ {
        let mut env = self.clean_env();
        env.set("XDG_RUNTIME_DIR", self.runtime_root().to_string_lossy());
        env
    }

    pub fn minimal_env(&self, disp: &Disp) -> Environ {
        let mut env = Environ::default();
        for k in PASS_ENV {
            if let Some(v) = self.env.get(k) {
                env.set(k, v);
            }
        }
        if env.get("PATH").is_none() {
            env.set("PATH", "/usr/local/bin:/usr/bin");
        }
        env.extend(&disp.env());
        env
    }
}

/// `env -i K=V ... ` prefix for systemd units (no inherited desktop).
pub fn env_argv(env: &Environ) -> Vec<String> {
    let mut v = vec!["/usr/bin/env".to_string(), "-i".to_string()];
    v.extend(env.0.iter().map(|(k, val)| format!("{k}={val}")));
    v
}
