//! Agent Chromium units: profiles, ports, readiness, clean shutdown.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::cdp::cdp_send;
use crate::ctx::*;
use crate::display::display;
use crate::net::Owner;
use crate::systemd::{agent_units, journal_tail, unit_name};
use crate::util::{Result, resolve, unlink_missing_ok, write_private};
use crate::{bail, json, tab};

/// `/json/version` timeout of the ownership check used by `tab` commands.
/// The shared browser can stall for seconds under host load; 5 s (the
/// Python's) failed every tab command during such stalls.
pub const OWNERSHIP_TIMEOUT: f64 = 15.0;

/// Browser profiles that belong to the user or other tools.
pub fn forbidden_profile_roots(ctx: &Ctx) -> Vec<PathBuf> {
    let home = ctx.home();
    let cfg = match ctx.env.get_nonempty("XDG_CONFIG_HOME") {
        Some(c) => PathBuf::from(c),
        None => home.join(".config"),
    };
    let names = [
        "chromium", "google-chrome", "google-chrome-beta", "google-chrome-unstable", "BraveSoftware",
        "microsoft-edge", "vivaldi", "helium", "net.imput.helium", "opera", "thorium",
    ];
    let mut out: Vec<PathBuf> = names.iter().map(|n| cfg.join(n)).collect();
    out.extend([
        home.join(".local/state/vuon-outbound"),
        home.join(".hermes"),
        home.join(".mozilla"),
        home.join(".var/app"),
    ]);
    out
}

fn under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

pub fn check_profile_allowed(ctx: &Ctx, path: &Path) -> Result<PathBuf> {
    let path = resolve(path);
    if path == resolve(&ctx.home()) {
        bail!("refusing to use the home directory as a profile");
    }
    for root in forbidden_profile_roots(ctx) {
        let root = resolve(&root);
        if under(&path, &root) {
            bail!("refusing profile {}: it belongs to {}", path.display(), root.display());
        }
    }
    let allowed = [resolve(&ctx.state_dir()), resolve(Path::new("/var/tmp"))];
    if !allowed.iter().any(|a| under(&path, a)) {
        bail!("refusing profile {}: agent profiles live under {} or /var/tmp", path.display(), allowed[0].display());
    }
    Ok(path)
}

pub fn check_port(ctx: &Ctx, port: i64, reserved: &[(u16, &str)]) -> Result<()> {
    if let Some((_, why)) = reserved.iter().find(|(p, _)| *p as i64 == port) {
        bail!("port {port} is reserved ({why}); pick another");
    }
    if !(1024..=65535).contains(&port) {
        bail!("port {port} out of range");
    }
    if !ctx.sys.port_free(port as u16) {
        bail!(
            "port {port} is already in use by: {}. Refusing to start; never reuse another browser's port.",
            ctx.sys.port_owner(ctx, port as u16)
        );
    }
    Ok(())
}

/// PID holding the Chromium profile lock, if alive.
pub fn singleton_owner(ctx: &Ctx, profile: &Path) -> Option<u32> {
    let target = std::fs::read_link(profile.join("SingletonLock")).ok()?;
    let target = target.to_string_lossy();
    let (host, pid) = target.rsplit_once('-')?;
    if host.is_empty() || pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) || host != ctx.hostname {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    Path::new(&format!("/proc/{pid}")).exists().then_some(pid)
}

pub fn browsers_real(ctx: &Ctx) -> Vec<BrowserInfo> {
    let dir = ctx.browsers_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut files: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    let mut found = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let Ok(Value::Object(info)) = serde_json::from_str::<Value>(&text) else { continue };
        let info = BrowserInfo(info);
        if !info.unit().is_empty() {
            found.push((f, info));
        }
    }
    // A browser whose remembered CDP owner still checks out in /proc is
    // running (a live process sits in its cgroup): no systemctl needed.
    let (known, ask): (Vec<_>, Vec<_>) = found.into_iter().partition(|(_, i)| {
        i.port().and_then(|p| u16::try_from(p).ok()).is_some_and(|p| owner_confirmed(ctx, p, i.unit()))
    });
    let units: Vec<String> = ask.iter().map(|(_, i)| i.unit().to_string()).collect();
    let states = ctx.sys.units_active(ctx, &units);
    let mut keep: Vec<(PathBuf, BrowserInfo)> = known;
    for ((f, info), state) in ask.into_iter().zip(states) {
        match state {
            // Confirmed stopped: forget it.
            Some(false) => {
                unlink_missing_ok(&f);
                unlink_missing_ok(&owner_path(ctx, info.unit()));
            }
            // Running, or unknown (systemctl failed or timed out, or the unit
            // is starting/stopping): keep the registration. The ownership
            // check decides whether it can be used now.
            Some(true) | None => keep.push((f, info)),
        }
    }
    keep.sort_by(|a, b| a.0.cmp(&b.0));
    keep.into_iter().map(|(_, i)| i).collect()
}

/// `browsers/<unit>.owner`: who served the unit's CDP port when last
/// checked (see `net::Owner`). Only a hint: it is re-proven from /proc
/// before every use, and replaced by a full check when that fails.
pub fn owner_path(ctx: &Ctx, unit: &str) -> PathBuf {
    ctx.browsers_dir().join(format!("{unit}.owner"))
}

fn read_owner(ctx: &Ctx, unit: &str) -> Option<Owner> {
    let text = std::fs::read_to_string(owner_path(ctx, unit)).ok()?;
    Owner::from_json(&serde_json::from_str(&text).ok()?)
}

/// Best effort: the cache only saves work, so failures are ignored.
fn write_owner(ctx: &Ctx, unit: &str, owner: &Owner) {
    let dir = ctx.browsers_dir();
    if !dir.is_dir() {
        return;
    }
    let tmp = dir.join(format!(".{unit}.owner.{}", std::process::id()));
    if write_private(&tmp, &json::dumps(&owner.to_json(), true)).is_ok() && std::fs::rename(&tmp, owner_path(ctx, unit)).is_err() {
        unlink_missing_ok(&tmp);
    }
}

/// The remembered owner of `unit`'s CDP on `port` still holds, per /proc.
pub fn owner_confirmed(ctx: &Ctx, port: u16, unit: &str) -> bool {
    read_owner(ctx, unit).is_some_and(|o| o.port == port && ctx.sys.owner_valid(&o, &unit_name(unit)))
}

/// Why the LISTEN socket on `port` isn't `unit`'s, or None if it is. The
/// remembered owner is checked from /proc first; only when that fails does
/// it ask systemd for the cgroup and scan it (and remember the result).
pub fn listener_problem(ctx: &Ctx, port: u16, unit: &str) -> Option<String> {
    if owner_confirmed(ctx, port, unit) {
        return None;
    }
    match ctx.sys.listener_owner(ctx, port, &unit_name(unit)) {
        Some(owner) => {
            write_owner(ctx, unit, &owner);
            None
        }
        None => {
            unlink_missing_ok(&owner_path(ctx, unit));
            Some(format!("CDP on port {port} is not served from {unit}"))
        }
    }
}

pub fn devtools_port(profile: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let first = text.split_whitespace().next()?;
    first.chars().all(|c| c.is_ascii_digit()).then(|| first.parse().ok()).flatten()
}

/// Why `port` isn't our browser's CDP, or None if it is. The listener must
/// be inside the browser unit AND answer CDP. The one ownership check used
/// by readiness, `browser stop` and `tab`. Not running means no listener in
/// the unit (no cgroup), so that fails at once without waiting on HTTP.
/// See `listener_problem` for the /proc-first listener check.
pub fn cdp_problem(ctx: &Ctx, port: u16, unit: &str, timeout: f64) -> Option<String> {
    if let Some(problem) = listener_problem(ctx, port, unit) {
        return Some(problem);
    }
    if let Err(e) = ctx.sys.cdp_version(port, timeout) {
        return Some(format!("{unit} is not answering CDP on {port}: {e}"));
    }
    None
}

pub fn cdp_ready(ctx: &Ctx, port: u16, unit: &str) -> bool {
    cdp_problem(ctx, port, unit, 2.0).is_none()
}

pub fn chromium_argv(ctx: &Ctx, profile: &Path, port: u16, urls: &[String], extra: &[String]) -> Vec<String> {
    let mut v = vec![
        ctx.env.get("AGENT_DISPLAY_CHROMIUM").unwrap_or(CHROMIUM).to_string(),
        "--ozone-platform=wayland".into(),
        format!("--user-data-dir={}", profile.display()),
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--password-store=basic".into(),
        "--disable-session-crashed-bubble".into(),
    ];
    v.extend(extra.iter().cloned());
    if urls.is_empty() {
        v.push("about:blank".into());
    } else {
        v.extend(urls.iter().cloned());
    }
    v
}

fn info_json(unit: &str, port: Value, profile: &Path, temp: bool) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("unit".into(), unit.into());
    m.insert("port".into(), port);
    m.insert("profile".into(), profile.to_string_lossy().into_owned().into());
    m.insert("temp".into(), temp.into());
    m
}

pub fn cmd_browser_start(ctx: &Ctx, temp: bool, port_arg: Option<i64>, audio: bool, urls: &[String]) -> Result<i32> {
    let disp = display(ctx)?;
    let (port, mut profile, unit);
    if temp {
        port = port_arg.unwrap_or(0);
        profile = ctx.sys.mkdtemp("agent-display-browser.", Path::new("/var/tmp"))?;
        let name = profile.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let suffix: String = name
            .split_once('.')
            .map(|(_, s)| s)
            .unwrap_or("")
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .take(12)
            .collect();
        unit = format!("{BROWSER_UNIT}-tmp-{suffix}");
    } else {
        port = port_arg.unwrap_or(DEFAULT_CDP_PORT as i64);
        profile = ctx.profile_dir();
        unit = BROWSER_UNIT.to_string();
        if ctx.sys.unit_active(ctx, &unit) {
            let info = ctx.sys.browsers(ctx).into_iter().find(|b| b.unit() == unit);
            let p = info.and_then(|b| b.0.get("port").cloned()).unwrap_or(Value::Null);
            let mut m = info_json(&unit, p, &profile, false);
            m.insert("already_running".into(), true.into());
            ctx.println(&json::dumps(&Value::Object(m), true));
            return Ok(0);
        }
    }
    let setup = (|| -> Result<PathBuf> {
        let profile = check_profile_allowed(ctx, &profile)?;
        if port != 0 {
            check_port(ctx, port, RESERVED_PORTS)?;
        }
        if !temp {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&profile)?;
            if let Some(owner) = singleton_owner(ctx, &profile) {
                bail!("profile {} is locked by PID {owner} outside agent-display; refusing to start", profile.display());
            }
        }
        unlink_missing_ok(&profile.join("DevToolsActivePort"));
        let extra: Vec<String> = if audio { vec![] } else { vec!["--mute-audio".into()] };
        let argv = chromium_argv(ctx, &profile, port as u16, urls, &extra);
        if !Path::new(&argv[0]).exists() {
            bail!("{} not found (set AGENT_DISPLAY_CHROMIUM)", argv[0]);
        }
        let stop_post: Option<Vec<String>> = temp.then(|| {
            vec!["/usr/bin/rm".into(), "-rf".into(), "--one-file-system".into(), "--".into(), profile.to_string_lossy().into()]
        });
        let mut full = env_argv(&ctx.minimal_env(&disp));
        full.extend(argv);
        ctx.sys.systemd_run(ctx, &unit, &full, true, stop_post.as_deref(), Some(&format!("Agent Chromium ({})", profile.display())))?;
        Ok(profile)
    })();
    match setup {
        Ok(p) => profile = p,
        Err(e) => {
            if temp {
                let _ = std::fs::remove_dir_all(&profile);
            }
            return Err(e);
        }
    }
    // Chromium writes DevToolsActivePort only for port 0; poll the port.
    let deadline = Instant::now() + Duration::from_secs(30);
    let actual = loop {
        if !ctx.sys.unit_active(ctx, &unit) {
            bail!("{unit} exited:\n{}", journal_tail(ctx, &unit, 12));
        }
        let actual = if port != 0 { Some(port as u16) } else { devtools_port(&profile) };
        if let Some(a) = actual
            && cdp_ready(ctx, a, &unit)
        {
            break a;
        }
        if Instant::now() > deadline {
            let log = journal_tail(ctx, &unit, 12);
            ctx.sys.stop_units(ctx, std::slice::from_ref(&unit))?;
            bail!("CDP did not come up on {unit}:\n{log}");
        }
        ctx.sys.sleep(0.25);
    };
    let info = Value::Object(info_json(&unit, actual.into(), &profile, temp));
    let dir = ctx.browsers_dir();
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    write_private(&dir.join(format!("{unit}.json")), &json::dumps(&info, true))?;
    ctx.println(&json::dumps(&info, true));
    Ok(0)
}

/// Quit Chromium cleanly so it flushes cookies (it doesn't on SIGTERM).
/// Chromium writes cookies on a 30 s timer; SIGTERM, which `systemctl stop`
/// sends to the whole cgroup, exits without a final flush. CDP
/// Browser.close is a normal quit. Falls back to `systemctl stop`.
pub fn close_browser(ctx: &Ctx, unit: &str, port: Option<i64>, timeout: f64) -> Result<()> {
    if let Some(port) = port.and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0)
        && cdp_ready(ctx, port, unit)
    {
        let _ = cdp_send(ctx, port, "Browser.close", 5.0);
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        while ctx.sys.unit_active(ctx, unit) && Instant::now() < deadline {
            ctx.sys.sleep(0.2);
        }
    }
    if ctx.sys.unit_active(ctx, unit) {
        ctx.sys.stop_units(ctx, &[unit.to_string()])?;
    }
    Ok(())
}

pub fn close_browsers(ctx: &Ctx, units: &[String]) -> Result<()> {
    let known = ctx.sys.browsers(ctx);
    for unit in units {
        if ctx.sys.unit_active(ctx, unit) {
            let port = known.iter().find(|b| b.unit() == unit).and_then(|b| b.port());
            close_browser(ctx, unit, port, 10.0)?;
        }
        unlink_missing_ok(&ctx.browsers_dir().join(format!("{unit}.json")));
        unlink_missing_ok(&owner_path(ctx, unit));
    }
    Ok(())
}

pub fn cmd_browser_stop(ctx: &Ctx, unit: Option<&str>, all: bool) -> Result<i32> {
    let mut units: Vec<String>;
    if all {
        let mut set: std::collections::BTreeSet<String> = ctx.sys.browsers(ctx).iter().map(|b| b.unit().to_string()).collect();
        set.extend(agent_units(ctx, BROWSER_UNIT).iter().map(|u| u.trim_end_matches(".service").to_string()));
        units = set.into_iter().collect();
    } else {
        let u = unit.unwrap_or(BROWSER_UNIT);
        let u = u.strip_suffix(".service").unwrap_or(u).to_string();
        if !u.starts_with(BROWSER_UNIT) {
            bail!("{u} is not an agent-display browser unit");
        }
        units = vec![u];
    }
    units.dedup();
    close_browsers(ctx, &units)?;
    if let Ok(disp) = display(ctx)
        && let Ok(_lock) = tab::OutputsLock::acquire(ctx)
    {
        let _ = tab::gc_tab_outputs(ctx, &disp);
    }
    ctx.println(&format!("stopped: {}", if units.is_empty() { "none".into() } else { units.join(", ") }));
    Ok(0)
}

pub fn cmd_browser_list(ctx: &Ctx) -> Result<i32> {
    let list = Value::Array(ctx.sys.browsers(ctx).into_iter().map(|b| Value::Object(b.0)).collect());
    ctx.println(&json::dumps_indent(&list, 2, true));
    Ok(0)
}

