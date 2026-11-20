//! Port of the Python `test_agent_display.py`. Stubs systemd, sockets, CDP
//! and sway through `Sys`; starts no compositor or browser. Live checks are
//! in README.md (Verification) and isolation_check.mjs.

mod fakes;
mod tab_tests;

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use fakes::*;
use serde_json::json;

use crate::browser::{self, check_port, check_profile_allowed, chromium_argv, close_browser, singleton_owner};
use crate::ctx::*;
use crate::display::{self, dbus_config, ensure_runtime_dir, env_exports, sway_config};
use crate::net::port_free_with;
use crate::systemd;
use crate::util::Environ;

// ------------------------------------------------------------ profiles

#[test]
fn refuses_user_and_other_tool_profiles() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    for rel in [
        ".config/chromium", ".config/chromium/Default", ".config/google-chrome", ".config/BraveSoftware/x",
        ".config/helium", ".local/state/vuon-outbound/browser", ".hermes/tools/x", "",
    ] {
        assert!(check_profile_allowed(&ctx, &t.home.join(rel)).is_err(), "{rel}");
    }
}

#[test]
fn refuses_paths_outside_agent_roots() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    assert!(check_profile_allowed(&ctx, &t.home.join("Downloads/profile")).is_err());
}

#[test]
fn allows_agent_state_and_var_tmp() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    let p = ctx.profile_dir();
    assert_eq!(check_profile_allowed(&ctx, &p).unwrap(), crate::util::resolve(&p));
    check_profile_allowed(&ctx, Path::new("/var/tmp/agent-display-browser.abc")).unwrap();
}

#[test]
fn default_profile_is_agent_owned() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    assert_eq!(ctx.profile_dir(), t.home.join(".local/state/agent-display/browser-profile"));
}

// ------------------------------------------------------------ ports

#[test]
fn reserved_ports_refused_even_if_free() {
    let t = TempHome::new();
    let sys = FakeSys { port_free: Some(true), ..Default::default() };
    let ctx = t.ctx(&sys);
    for port in [9222, 9333] {
        let e = check_port(&ctx, port, RESERVED_PORTS).unwrap_err();
        assert!(e.msg.contains("reserved"), "{e}");
    }
}

#[test]
fn default_port_is_not_reserved() {
    assert!(reserved_reason(DEFAULT_CDP_PORT as i64).is_none());
    assert!(!RESERVED_VNC_PORTS.iter().any(|(p, _)| *p == DEFAULT_VNC_PORT));
}

#[test]
fn time_wait_is_not_busy() {
    // Chromium's CDP listener sets SO_REUSEADDR; on Linux a TIME_WAIT socket
    // only yields to a new SO_REUSEADDR bind if it had it too (std sets it).
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let cli = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (conn, _) = srv.accept().unwrap();
    drop(conn); // server side closes first: TIME_WAIT on `port`
    drop(cli);
    drop(srv);
    assert!(crate::net::port_free(port));
}

#[test]
fn busy_port_refused_with_owner() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    assert!(!crate::net::port_free(port));
    let t = TempHome::new();
    let sys = FakeSys { port_owner: Some("chromium pid=1".into()), ..Default::default() };
    let ctx = t.ctx(&sys);
    let e = check_port(&ctx, port as i64, RESERVED_PORTS).unwrap_err();
    assert!(e.msg.contains("chromium pid=1"), "{e}");
}

#[test]
fn no_ipv6_is_not_busy() {
    for code in [libc::EAFNOSUPPORT, libc::EADDRNOTAVAIL] {
        assert!(port_free_with(0, |_, v6| if v6 { Err(code) } else { Ok(()) }));
    }
}

#[test]
fn ipv6_in_use_is_busy() {
    assert!(!port_free_with(0, |_, v6| if v6 { Err(libc::EADDRINUSE) } else { Ok(()) }));
}

// ------------------------------------------------------------ environment

fn disp() -> Disp {
    Disp {
        runtime: "/run/user/1/agent-display".into(),
        wayland: "wayland-1".into(),
        swaysock: "s".into(),
        bus: "unix:path=/run/user/1/agent-display/d".into(),
    }
}

#[test]
fn clean_env_drops_desktop_pointers() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let mut ctx = t.ctx(&sys);
    ctx.env = Environ(
        [("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-1"), ("HYPRLAND_INSTANCE_SIGNATURE", "x"), ("XAUTHORITY", "a"),
         ("DBUS_SESSION_BUS_ADDRESS", "b"), ("KEEP", "1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    assert_eq!(ctx.clean_env(), Environ(vec![("KEEP".into(), "1".into())]));
}

#[test]
fn minimal_env_has_only_pass_list_and_agent_vars() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let mut ctx = t.ctx(&sys);
    for (k, v) in [("DISPLAY", ":0"), ("SECRET_TOKEN", "x"), ("HYPRLAND_INSTANCE_SIGNATURE", "h"), ("LANG", "C.UTF-8")] {
        ctx.env.set(k, v);
    }
    let env = ctx.minimal_env(&disp());
    for k in ["DISPLAY", "SECRET_TOKEN", "HYPRLAND_INSTANCE_SIGNATURE"] {
        assert!(env.get(k).is_none(), "{k}");
    }
    assert_eq!(env.get("WAYLAND_DISPLAY"), Some("wayland-1"));
    assert_eq!(env.get("LANG"), Some("C.UTF-8"));
}

#[test]
fn env_exports_scrub_and_switch_bus() {
    let out = env_exports(&disp());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0].split_whitespace().skip(1).collect::<Vec<_>>(), BLOCKED_ENV.to_vec());
    assert!(lines.contains(&"export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1/agent-display/d"));
    assert!(lines.contains(&"export WAYLAND_DISPLAY=/run/user/1/agent-display/wayland-1"));
    assert!(lines.contains(&"export SWAYSOCK=s"));
    assert!(!lines.iter().any(|l| l.contains("XDG_RUNTIME_DIR")));
}

#[test]
fn tool_still_works_in_shell_after_env() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let mut ctx = t.ctx(&sys);
    let before = (ctx.runtime_dir(), ctx.runtime_root());
    ctx.env.set("XDG_RUNTIME_DIR", t.rt.join("agent-display").to_string_lossy());
    ctx.env.set("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1/agent-display/d");
    ctx.env.set("WAYLAND_DISPLAY", "wayland-1");
    assert_eq!((ctx.runtime_dir(), ctx.runtime_root()), before);
    let env = ctx.manager_env();
    assert_eq!(env.get("XDG_RUNTIME_DIR"), Some(t.rt.to_str().unwrap()));
    assert!(env.get("DBUS_SESSION_BUS_ADDRESS").is_none());
    assert!(env.get("WAYLAND_DISPLAY").is_none());
}

#[test]
fn systemctl_runs_with_manager_env() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let mut ctx = t.ctx(&sys);
    ctx.env.set("XDG_RUNTIME_DIR", t.rt.join("agent-display").to_string_lossy());
    ctx.env.set("DBUS_SESSION_BUS_ADDRESS", "unix:path=/x");
    let argv: Vec<String> = ["systemctl", "--user", "is-active", "agent-display.service"].map(String::from).to_vec();
    let env = systemd::run_env(&ctx, &argv, None).unwrap();
    assert_eq!(env.get("XDG_RUNTIME_DIR"), Some(t.rt.to_str().unwrap()));
    assert!(env.get("DBUS_SESSION_BUS_ADDRESS").is_none());
    // and unit_active goes through run() with no explicit env
    systemd::units_active(&ctx, &["agent-display".to_string()]);
    let runs = sys.runs.borrow();
    assert_eq!(runs[0].0[0], "systemctl");
    assert!(runs[0].1.is_none());
}

#[test]
fn env_argv_clears_inherited_environment() {
    let argv = env_argv(&Environ(vec![("A".into(), "1".into())]));
    assert_eq!(&argv[..2], &["/usr/bin/env", "-i"]);
    assert_eq!(argv[2], "A=1");
}

// ------------------------------------------------------------ configs

#[test]
fn chromium_flags() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    let argv = chromium_argv(&ctx, Path::new("/p"), 9425, &[], &[]);
    // Not the /usr/bin wrapper: it appends ~/.config/chromium-flags.conf.
    assert_eq!(argv[0], "/usr/lib/chromium/chromium");
    for flag in ["--ozone-platform=wayland", "--user-data-dir=/p", "--remote-debugging-port=9425", "--no-first-run", "--password-store=basic"] {
        assert!(argv.iter().any(|a| a == flag), "{flag}");
    }
    assert_eq!(argv.last().unwrap(), "about:blank");
}

#[test]
fn sway_config_is_self_contained() {
    let text = sway_config("HEADLESS-1", "1920x1080", Path::new("/opt/agent display/agent-display"));
    let directives: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    for word in ["include", "import-environment", "dbus-update-activation-environment"] {
        assert!(!directives.iter().any(|l| l.contains(word)), "{word}");
    }
    assert!(text.contains("xwayland disable"));
    assert!(text.contains("mode --custom 1920x1080"));
    // hold-input runs this binary, shell-quoted.
    assert!(text.contains("\nexec '/opt/agent display/agent-display' hold-input\n"));
}

#[test]
fn dbus_config_has_no_service_activation() {
    let text = dbus_config(Path::new("/run/user/1/agent-display"));
    assert!(!text.contains("servicedir"));
    assert!(text.contains("<listen>unix:dir=/run/user/1/agent-display</listen>"));
}

// ------------------------------------------------------------ display

struct DisplayFixture {
    t: TempHome,
    dir: PathBuf,
    _socks: Vec<std::os::unix::net::UnixListener>,
}

impl DisplayFixture {
    fn new() -> Self {
        let t = TempHome::new();
        let sys = FakeSys::default();
        let dir = ensure_runtime_dir(&t.ctx(&sys)).unwrap();
        let socks = ["wayland-1", "sway-ipc.1000.4242.sock"]
            .iter()
            .map(|n| std::os::unix::net::UnixListener::bind(dir.join(n)).unwrap())
            .collect();
        let f = DisplayFixture { t, dir, _socks: socks };
        f.write_env(&[]);
        f
    }

    fn write_env(&self, over: &[(&str, String)]) {
        let mut env = [
            ("WAYLAND_DISPLAY", "wayland-1".to_string()),
            ("SWAYSOCK", self.dir.join("sway-ipc.1000.4242.sock").to_string_lossy().into_owned()),
            ("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}/dbus-x,guid=1", self.dir.display())),
        ];
        for (k, v) in over {
            env.iter_mut().find(|(ek, _)| ek == k).unwrap().1 = v.clone();
        }
        let text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        std::fs::write(self.dir.join("env"), text).unwrap();
    }
}

#[test]
fn valid_display() {
    let f = DisplayFixture::new();
    let sys = FakeSys { active: Some(true), pid_in_unit: Some(true), ..Default::default() };
    let ctx = f.t.ctx(&sys);
    let d = display::display_real(&ctx).unwrap();
    assert_eq!(sys.pid_checks.borrow().last().unwrap(), &(4242, "agent-display.service".to_string()));
    assert_eq!(d.runtime, f.dir.to_string_lossy());
    assert_eq!(d.wayland, "wayland-1");
}

#[test]
fn display_not_running() {
    let f = DisplayFixture::new();
    let sys = FakeSys { active: Some(false), pid_in_unit: Some(true), ..Default::default() };
    assert!(display::display_real(&f.t.ctx(&sys)).is_err());
}

#[test]
fn stale_sway_owner() {
    let f = DisplayFixture::new();
    let sys = FakeSys { active: Some(true), pid_in_unit: Some(false), ..Default::default() };
    assert!(display::display_real(&f.t.ctx(&sys)).is_err());
}

#[test]
fn never_accepts_socket_outside_private_dir() {
    let f = DisplayFixture::new();
    let sys = FakeSys { active: Some(true), pid_in_unit: Some(true), ..Default::default() };
    for over in [
        ("WAYLAND_DISPLAY", "/run/user/1000/wayland-1".to_string()),
        ("WAYLAND_DISPLAY", "wayland-9".to_string()),
        ("SWAYSOCK", "/run/user/1000/sway-ipc.1000.4242.sock".to_string()),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus".to_string()),
    ] {
        f.write_env(std::slice::from_ref(&over));
        assert!(display::display_real(&f.t.ctx(&sys)).is_err(), "{over:?}");
    }
}

#[test]
fn runtime_dir_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let f = DisplayFixture::new();
    std::fs::set_permissions(&f.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sys = FakeSys::default();
    assert!(ensure_runtime_dir(&f.t.ctx(&sys)).is_err());
}

// ------------------------------------------------------------ profile lock

#[test]
fn live_lock_reported() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    let prof = t.home.join("p");
    std::fs::create_dir(&prof).unwrap();
    std::os::unix::fs::symlink(format!("{}-{}", ctx.hostname, std::process::id()), prof.join("SingletonLock")).unwrap();
    assert_eq!(singleton_owner(&ctx, &prof), Some(std::process::id()));
}

#[test]
fn stale_or_foreign_lock_ignored() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    let prof = t.home.join("p");
    std::fs::create_dir(&prof).unwrap();
    std::os::unix::fs::symlink("otherhost-1", prof.join("SingletonLock")).unwrap();
    assert_eq!(singleton_owner(&ctx, &prof), None);
}

// ------------------------------------------------------------ systemd

#[test]
fn dependents_bind_to_display() {
    let t = TempHome::new();
    let sys = FakeSys::default();
    let ctx = t.ctx(&sys);
    let post: Vec<String> = ["/usr/bin/rm", "-rf", "--", "/var/tmp/x"].map(String::from).to_vec();
    systemd::systemd_run(&ctx, "agent-display-browser", &["/bin/true".into()], true, Some(&post), None).unwrap();
    let argv = sys.runs.borrow()[0].0.clone();
    assert!(argv.contains(&"BindsTo=agent-display.service".to_string()));
    assert!(argv.contains(&"ExecStopPost=/usr/bin/rm -rf -- /var/tmp/x".to_string()));
    assert_eq!(&argv[argv.len() - 2..], &["--", "/bin/true"]);
}

// ------------------------------------------------------------ browser start / stop

#[test]
fn temp_profile_removed_when_refused() {
    let t = TempHome::new();
    let sys = FakeSys { mkdtemp_in: Some(t.root.clone()), display: Some(Ok(disp())), ..Default::default() };
    let ctx = t.ctx(&sys);
    // The temp dir is outside the allowed roots here, so the profile check
    // refuses (the Python stubs that check out and refuses port 9333).
    let e = browser::cmd_browser_start(&ctx, true, Some(9333), false, &[]).unwrap_err();
    assert!(e.msg.contains("refusing"), "{e}");
    assert!(sys.systemd_runs.borrow().is_empty());
    let made = sys.made.borrow();
    assert!(!made[0].exists());
}

#[test]
fn temp_profile_removed_when_port_reserved() {
    let t = TempHome::new();
    let sys = FakeSys { display: Some(Ok(disp())), ..Default::default() };
    let ctx = t.ctx(&sys);
    let e = browser::cmd_browser_start(&ctx, true, Some(9333), false, &[]).unwrap_err();
    assert!(e.msg.contains("reserved"), "{e}");
    assert!(sys.systemd_runs.borrow().is_empty());
    let made = sys.made.borrow();
    assert!(made[0].starts_with("/var/tmp") && !made[0].exists());
}

#[test]
fn cli_reports_errors_without_traceback() {
    let t = TempHome::new();
    let sys = FakeSys { active: Some(false), ..Default::default() };
    let (code, _, err) = t.cli(&sys, &["browser", "start"]);
    assert_eq!(code, 1);
    assert!(err.contains("not running"), "{err}");
    assert!(err.starts_with("agent-display: "));
}

#[test]
fn clean_quit_before_sigterm() {
    let t = TempHome::new();
    let b = FakeBrowser::new();
    let sys = FakeSys { listener: Some(Box::new(|_, _| true)), browser: Some(b.clone()), ..Default::default() };
    sys.active_seq.borrow_mut().extend([true, false, false]);
    let ctx = t.ctx(&sys);
    close_browser(&ctx, "agent-display-browser", Some(9425), 10.0).unwrap();
    assert_eq!(b.methods(), vec!["Browser.close"]);
    assert!(sys.stopped.borrow().is_empty());
}

#[test]
fn falls_back_to_systemctl_stop() {
    let t = TempHome::new();
    let b = FakeBrowser::new();
    b.st().fail_connect = true;
    let sys = FakeSys { listener: Some(Box::new(|_, _| true)), active: Some(true), browser: Some(b), ..Default::default() };
    let ctx = t.ctx(&sys);
    close_browser(&ctx, "agent-display-browser", Some(9425), 0.0).unwrap();
    assert_eq!(*sys.stopped.borrow(), vec![vec!["agent-display-browser".to_string()]]);
}

#[test]
fn never_closes_a_port_owned_by_someone_else() {
    let t = TempHome::new();
    let b = FakeBrowser::new();
    let sys = FakeSys { listener: Some(Box::new(|_, _| false)), active: Some(true), browser: Some(b.clone()), ..Default::default() };
    let ctx = t.ctx(&sys);
    close_browser(&ctx, "agent-display-browser", Some(9333), 0.0).unwrap();
    assert!(b.methods().is_empty());
}

// ------------------------------------------------------------ ownership

#[test]
fn tab_and_readiness_share_one_check() {
    let t = TempHome::new();
    let sys = FakeSys { listener: Some(Box::new(|_, _| false)), ..Default::default() };
    let ctx = t.ctx(&sys);
    assert!(!browser::cdp_ready(&ctx, 9425, "agent-display-browser"));
    let e = crate::tab::check_owned(&ctx, 9425, "agent-display-browser").unwrap_err();
    assert!(e.msg.contains("not served from agent-display-browser"), "{e}");
    assert_eq!(*sys.version_calls.borrow(), 0);
}

#[test]
fn listener_ownership_from_proc_and_unit_cgroup() {
    // Fix 2: /proc/net/tcp{,6} LISTEN inode + an fd for it in the unit's
    // cgroup pids, instead of `ss -Hltnp`.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let own = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap().to_string();
    let t = TempHome::new();
    let check = |cg: Option<&str>, port: u16| {
        let sys = FakeSys { cgroup: Some(cg.map(String::from)), ..Default::default() };
        let ctx = t.ctx(&sys);
        sys.listener_in_unit(&ctx, port, "agent-display-browser")
    };
    assert!(check(Some(&own), port), "our listener, our cgroup");
    assert!(!check(Some("/init.scope"), port), "listener outside the unit");
    assert!(!check(None, port), "unit not running: no cgroup");
    drop(l);
    assert!(!check(Some(&own), port), "nothing listening");
}

#[test]
fn unit_state_and_cgroup_come_from_one_systemctl_call() {
    struct Show(RefCell<Vec<Vec<String>>>);
    impl Sys for Show {
        fn run(&self, _: &Ctx, argv: &[String], _: Option<&Environ>, _: Option<f64>, _: bool) -> crate::util::Result<String> {
            self.0.borrow_mut().push(argv.to_vec());
            Ok("Id=agent-display-browser.service\nActiveState=active\nControlGroup=/a/agent-display-browser.service\nMainPID=42\n\n\
                Id=agent-display-browser-tmp-x.service\nActiveState=inactive\nControlGroup=\nMainPID=0".into())
        }
    }
    let t = TempHome::new();
    let sys = Show(RefCell::new(vec![]));
    let ctx = Ctx::new(t.env(), &sys, Box::new(std::io::sink()), Box::new(std::io::sink()));
    let units = vec!["agent-display-browser".to_string(), "agent-display-browser-tmp-x".to_string()];
    assert_eq!(systemd::units_active(&ctx, &units), vec![true, false]);
    let calls = sys.0.borrow().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(&calls[0][..3], &["systemctl", "--user", "show"]);
    assert_eq!(&calls[0][calls[0].len() - 2..], &["agent-display-browser.service", "agent-display-browser-tmp-x.service"]);
    assert_eq!(
        ctx.units.borrow().get("agent-display-browser.service"),
        Some(&systemd::UnitInfo { cgroup: "/a/agent-display-browser.service".into(), main_pid: 42 })
    );
    // Cached: the ownership check needs no second systemctl call.
    assert_eq!(systemd::unit_cgroup(&ctx, "agent-display-browser").as_deref(), Some("/a/agent-display-browser.service"));
    assert_eq!(sys.0.borrow().len(), 1);
    // A unit that isn't running has no cgroup.
    assert_eq!(ctx.units.borrow().get("agent-display-browser-tmp-x.service"), None);
}

#[test]
fn browsers_registry_drops_inactive_units_in_one_query() {
    let t = TempHome::new();
    let sys = FakeSys { active_fn: Some(Box::new(|u| !u.contains("tmp"))), ..Default::default() };
    let ctx = t.ctx(&sys);
    let dir = ctx.browsers_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("agent-display-browser.json"), r#"{"unit": "agent-display-browser", "port": 9425, "profile": "/p", "temp": false}"#).unwrap();
    std::fs::write(dir.join("agent-display-browser-tmp-x.json"), r#"{"unit": "agent-display-browser-tmp-x", "port": 1, "profile": "/q", "temp": true}"#).unwrap();
    std::fs::write(dir.join("junk.json"), "not json").unwrap();
    let got = crate::browser::browsers_real(&ctx);
    assert_eq!(got.iter().map(|b| b.unit()).collect::<Vec<_>>(), vec!["agent-display-browser"]);
    assert!(!dir.join("agent-display-browser-tmp-x.json").exists());
    assert!(dir.join("junk.json").exists());
}

// ------------------------------------------------------------ type

#[test]
fn type_uses_host_gui_splitting() {
    let argvs = crate::typing::type_args("x@y 日本語");
    assert!(argvs.iter().all(|a| a[..3] == ["wtype", "-s", "400"]));
}

#[test]
fn json_float_and_key_order_survive_round_trip() {
    let v: serde_json::Value = serde_json::from_str(r#"{"z": 1, "a": [0.5, 2], "m": {"y": null}}"#).unwrap();
    assert_eq!(crate::json::dumps(&v, true), r#"{"z": 1, "a": [0.5, 2], "m": {"y": null}}"#);
    assert_eq!(crate::json::dumps(&json!(1e21), true), "1e+21");
}
