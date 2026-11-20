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

/// One `systemctl show` for the state, and the cgroup remembered for this
/// process (a unit's cgroup path is fixed by its name).
pub fn unit_active(ctx: &Ctx, name: &str) -> bool {
    let unit = unit_name(name);
    let out = run(ctx, &["systemctl", "--user", "show", "-p", "ActiveState", "-p", "ControlGroup", &unit], None, Some(15.0), false)
        .unwrap_or_default();
    let mut active = false;
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            active = v == "active";
        } else if let Some(v) = line.strip_prefix("ControlGroup=")
            && !v.is_empty()
        {
            ctx.cgroups.borrow_mut().insert(unit.clone(), v.to_string());
        }
    }
    active
}

pub fn unit_cgroup(ctx: &Ctx, name: &str) -> Option<String> {
    let unit = unit_name(name);
    if let Some(cg) = ctx.cgroups.borrow().get(&unit) {
        return Some(cg.clone());
    }
    let out = run(ctx, &["systemctl", "--user", "show", "-P", "ControlGroup", &unit], None, Some(15.0), false).ok()?;
    let cg = out.trim().to_string();
    if cg.is_empty() {
        return None;
    }
    ctx.cgroups.borrow_mut().insert(unit, cg.clone());
    Some(cg)
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
