//! systemd user units: state, cgroup, transient units, journal.

use crate::ctx::{Ctx, DISPLAY_UNIT};
use crate::util::{Environ, Error, Result, shjoin, spawn_capture};

const USER_MANAGER_TOOLS: &[&str] = &["systemctl", "journalctl", "systemd-run"];

/// The Python's `run()`: `env` None means the caller's environment, except
/// systemctl/journalctl/systemd-run, which get `manager_env()` so they reach
/// the user's systemd even from a shell that ran `eval "$(agent-display env)"`.
pub fn run(ctx: &Ctx, argv: &[&str], env: Option<&Environ>, timeout: Option<f64>, check: bool) -> Result<String> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    ctx.sys.run(ctx, &argv, env, timeout, check)
}

/// The environment `run()` gives argv (None: inherit the caller's).
pub fn run_env(ctx: &Ctx, argv: &[String], env: Option<&Environ>) -> Option<Environ> {
    match env {
        None if argv.first().is_some_and(|a| USER_MANAGER_TOOLS.contains(&a.as_str())) => Some(ctx.manager_env()),
        other => other.cloned(),
    }
}

pub fn run_real(ctx: &Ctx, argv: &[String], env: Option<&Environ>, timeout: Option<f64>, check: bool) -> Result<String> {
    let env = run_env(ctx, argv, env);
    let out = spawn_capture(argv, env.as_ref(), timeout)?;
    if check && out.code != 0 {
        let msg = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
        return Err(Error::new(format!("{}: {}", shjoin(argv), msg)));
    }
    Ok(out.stdout.trim().to_string())
}

pub fn unit_name(name: &str) -> String {
    if name.ends_with(".service") { name.to_string() } else { format!("{name}.service") }
}

/// What `systemctl show` told this process about a unit: its cgroup path
/// (fixed by the unit name) and main pid (only a scan-order hint).
#[derive(Debug, Clone, PartialEq)]
pub struct UnitInfo {
    pub cgroup: String,
    pub main_pid: u32,
}

/// `systemctl show` blocks (id, ActiveState, cgroup/pid), or None if
/// systemctl itself failed or timed out: then nothing is known about any of
/// the units, which is not the same as "inactive".
fn show(ctx: &Ctx, units: &[String]) -> Option<Vec<(String, String, Option<UnitInfo>)>> {
    let mut argv: Vec<String> = ["systemctl", "--user", "show", "-p", "Id", "-p", "ActiveState", "-p", "ControlGroup", "-p", "MainPID"]
        .map(String::from)
        .to_vec();
    argv.extend(units.iter().cloned());
    let out = ctx.sys.run(ctx, &argv, None, Some(15.0), true).ok()?;
    let mut blocks = Vec::new();
    for block in out.split("\n\n") {
        let (mut id, mut state, mut cg, mut pid) = (String::new(), String::new(), String::new(), 0u32);
        for line in block.lines() {
            match line.split_once('=') {
                Some(("Id", v)) => id = v.to_string(),
                Some(("ActiveState", v)) => state = v.to_string(),
                Some(("ControlGroup", v)) => cg = v.to_string(),
                Some(("MainPID", v)) => pid = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        if !id.is_empty() {
            let info = (!cg.is_empty()).then_some(UnitInfo { cgroup: cg, main_pid: pid });
            blocks.push((id, state, info));
        }
    }
    Some(blocks)
}

/// What an ActiveState says about whether the unit runs: `Some(true)`
/// active, `Some(false)` confirmed stopped (`inactive`, `failed`; a unit
/// systemd doesn't know is `inactive` too), `None` in between
/// (`activating`, `deactivating`, ...).
pub fn state_active(state: &str) -> Option<bool> {
    match state {
        "active" | "reloading" | "refreshing" => Some(true),
        "inactive" | "failed" => Some(false),
        _ => None,
    }
}

/// ActiveState says the unit runs (`active`, `reloading`, `refreshing`).
pub fn is_running(state: &str) -> bool {
    state_active(state) == Some(true)
}

/// ActiveState says the unit is stopped (`inactive`, `failed`) or on its way
/// there (`deactivating`). Anything else, including an unknown state, may
/// still run.
pub fn stopping_or_stopped(state: &str) -> bool {
    state == "deactivating" || state_active(state) == Some(false)
}

/// ActiveState of several units in one `systemctl show` (the Python runs
/// `systemctl is-active` per unit). Cgroup and main pid are remembered for
/// the ownership check later in this process. `None` for a unit means
/// unknown: systemctl failed or timed out, or the unit is in transition.
/// Callers must not treat unknown as stopped (e.g. delete its state).
pub fn units_active(ctx: &Ctx, names: &[String]) -> Vec<Option<bool>> {
    if names.is_empty() {
        return vec![];
    }
    let units: Vec<String> = names.iter().map(|n| unit_name(n)).collect();
    let Some(blocks) = show(ctx, &units) else {
        return vec![None; units.len()];
    };
    for (id, _, info) in &blocks {
        if let Some(info) = info {
            ctx.units.borrow_mut().insert(id.clone(), info.clone());
        }
    }
    units
        .iter()
        .map(|u| blocks.iter().find(|(id, _, _)| id == u).and_then(|(_, state, _)| state_active(state)))
        .collect()
}

/// Whether the unit is active; unknown counts as not active.
pub fn unit_active(ctx: &Ctx, name: &str) -> bool {
    unit_state(ctx, name) == Some(true)
}

/// The unit's raw ActiveState (`active`, `deactivating`, ...), or None if
/// systemctl failed or timed out (unknown). For decisions that must tell a
/// stopping unit from one whose state can't be read.
pub fn unit_active_state(ctx: &Ctx, name: &str) -> Option<String> {
    let unit = unit_name(name);
    let blocks = show(ctx, std::slice::from_ref(&unit))?;
    let (id, state, info) = blocks.into_iter().find(|(id, _, _)| *id == unit)?;
    if let Some(info) = info {
        ctx.units.borrow_mut().insert(id, info);
    }
    (!state.is_empty()).then_some(state)
}

/// `units_active` for one unit.
pub fn unit_state(ctx: &Ctx, name: &str) -> Option<bool> {
    ctx.sys.units_active(ctx, &[name.to_string()]).first().copied().flatten()
}

pub fn unit_cgroup(ctx: &Ctx, name: &str) -> Option<String> {
    let unit = unit_name(name);
    if let Some(info) = ctx.units.borrow().get(&unit) {
        return Some(info.cgroup.clone());
    }
    let (_, _, info) = show(ctx, std::slice::from_ref(&unit))?.into_iter().find(|(id, _, _)| *id == unit)?;
    let info = info?;
    ctx.units.borrow_mut().insert(unit, info.clone());
    Some(info.cgroup)
}

pub fn agent_units(ctx: &Ctx, prefix: &str) -> Vec<String> {
    let pattern = format!("{prefix}*.service");
    let out = run(
        ctx,
        &["systemctl", "--user", "list-units", "--plain", "--no-legend", "--state=active,activating,deactivating", &pattern],
        None,
        Some(15.0),
        false,
    )
    .unwrap_or_default();
    out.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| l.split_whitespace().next().map(String::from)).collect()
}

pub fn stop_units(ctx: &Ctx, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut argv = vec!["systemctl".to_string(), "--user".into(), "stop".into()];
    argv.extend(names.iter().map(|n| unit_name(n)));
    ctx.sys.run(ctx, &argv, None, Some(60.0), true).map(|_| ())
}

/// Recent log of the unit's latest run, without core-dump backtraces.
pub fn journal_tail(ctx: &Ctx, name: &str, lines: usize) -> String {
    let unit = unit_name(name);
    let inv = run(ctx, &["systemctl", "--user", "show", "-P", "InvocationID", &unit], None, Some(15.0), false)
        .unwrap_or_default();
    let matcher = if inv.is_empty() { vec!["-u".to_string(), unit.clone()] } else { vec![format!("_SYSTEMD_INVOCATION_ID={inv}")] };
    let mut argv: Vec<String> = vec!["journalctl".into(), "--user".into()];
    argv.extend(matcher);
    argv.extend(["-n", "200", "--no-pager", "-o", "cat"].map(String::from));
    let out = ctx.sys.run(ctx, &argv, None, Some(15.0), false).unwrap_or_default();
    let keep: Vec<&str> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            let t = l.trim_start();
            let backtrace_frame = t.starts_with('#')
                && t[1..].find(' ').is_some_and(|i| i > 0 && t[1..1 + i].chars().all(|c| c.is_ascii_digit()));
            !(backtrace_frame || t.starts_with("Stack trace of") || t.starts_with("ELF object"))
        })
        .collect();
    keep[keep.len().saturating_sub(lines)..].join("\n")
}

pub fn systemd_run(ctx: &Ctx, unit: &str, argv: &[String], bind: bool, stop_post: Option<&[String]>, description: Option<&str>) -> Result<()> {
    let mut cmd: Vec<String> = vec![
        "systemd-run".into(),
        "--user".into(),
        "--quiet".into(),
        "--collect".into(),
        format!("--unit={unit}"),
        "-p".into(),
        format!("Description={}", description.unwrap_or(unit)),
    ];
    if bind {
        cmd.extend(["-p".into(), format!("BindsTo={DISPLAY_UNIT}"), "-p".into(), format!("After={DISPLAY_UNIT}")]);
    }
    if let Some(post) = stop_post {
        cmd.extend(["-p".into(), format!("ExecStopPost={}", shjoin(post))]);
    }
    cmd.push("--".into());
    cmd.extend(argv.iter().cloned());
    ctx.sys.run(ctx, &cmd, None, Some(30.0), true).map(|_| ())
}
