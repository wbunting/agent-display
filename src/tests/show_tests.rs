//! `tab show` / `tab unshow` against the fake sway, browser and systemd.
//! wayvnc never runs: the fake systemd_run binds its Unix socket, and the
//! viewer events come from a script.

use serde_json::{Value, json};

use super::fakes::*;
use super::tab_tests::Fx;
use crate::show::VncEvent;
use crate::util::Error;

fn fx() -> Fx {
    let f = Fx::new(true);
    for t in ["wayvnc", "wayvncctl", "timeout"] {
        f.t.fake_tool(t);
    }
    f
}

fn opened(f: &Fx) -> (String, String) {
    let o = f.open(&[]);
    (o["tab"].as_str().unwrap().to_string(), o["output"].as_str().unwrap().to_string())
}

fn lines(out: &str) -> Vec<Value> {
    out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn viewer_runs(f: &Fx) -> Vec<Vec<String>> {
    f.sys.systemd_runs.borrow().iter().filter(|r| r[0].starts_with("agent-display-vnc-")).cloned().collect()
}

fn ev(method: &str, clients: i64) -> VncEvent {
    VncEvent::Event(json!({"method": method, "params": {"id": "1", "connection_count": clients}}))
}

fn stopped(f: &Fx) -> Vec<String> {
    f.sys.stopped.borrow().iter().flatten().cloned().collect()
}

#[test]
fn show_serves_only_the_tabs_output_on_a_unix_socket() {
    let f = fx();
    let (tab, output) = opened(&f);
    assert_eq!(output, "HEADLESS-2");
    let sway_before = f.sw().st().commands.len();
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 0, "{err}");
    let sock = f.t.rt.join("agent-display/vnc/HEADLESS-2.sock");
    let ctl = f.t.rt.join("agent-display/vnc/HEADLESS-2.ctl");
    let l = lines(&out);
    assert_eq!(l.len(), 1, "returns at once without --wait-viewer/--until-*");
    assert_eq!(
        out.trim_end(),
        format!(
            r#"{{"shown": "{tab}", "output": "HEADLESS-2", "socket": "{s}", "viewer_command": "vncviewer {s}", "unit": "agent-display-vnc-headless-2", "view_only": false, "timeout": 900.0, "already_shown": false}}"#,
            s = sock.display()
        )
    );
    let runs = viewer_runs(&f);
    assert_eq!(runs.len(), 1);
    let argv = &runs[0];
    assert_eq!(argv[0], "agent-display-vnc-headless-2");
    let tail: Vec<&str> = argv.iter().skip_while(|a| !a.ends_with("/timeout")).map(String::as_str).collect();
    let bin = f.t.root.join("bin");
    let want = vec![
        bin.join("timeout").to_string_lossy().into_owned(),
        "--kill-after=5".into(),
        "902".into(), // wayvnc's own timer: 2 s after the view's deadline
        bin.join("wayvnc").to_string_lossy().into_owned(),
        "--exit-on-disconnect".into(),
        format!("--config={}", f.t.rt.join("agent-display/wayvnc.config").display()),
        format!("--socket={}", ctl.display()),
        "--output=HEADLESS-2".into(),
        "--disable-resizing".into(),
        "--render-cursor".into(),
        "--name=agent-display HEADLESS-2".into(),
        "--unix-socket".into(),
        sock.to_string_lossy().into_owned(),
    ];
    assert_eq!(tail, want.iter().map(String::as_str).collect::<Vec<_>>());
    // Only that output; no TCP address, no desktop-wide capture.
    assert!(!argv.iter().any(|a| a == "127.0.0.1" || a == "--desktop" || a.starts_with("--output=HEADLESS-1")));
    assert!(argv.iter().any(|a| a == "/usr/bin/env") && argv.contains(&"-i".to_string()));
    assert!(f.sys.vnc_opened.borrow().is_empty());
    // Seat focus and other agents' windows are left alone: no sway commands.
    assert_eq!(f.sw().st().focus, Some(5));
    assert_eq!(f.sw().st().commands.len(), sway_before, "{:?}", f.sw().st().commands);
}

#[test]
fn view_only_disables_input_and_timeout_bounds_the_view() {
    let f = fx();
    let (tab, _) = opened(&f);
    let (code, out, _) = f.cli(&["show", "--view-only", "--timeout", "120", &tab]);
    assert_eq!(code, 0);
    assert_eq!(lines(&out)[0]["view_only"], true);
    let argv = &viewer_runs(&f)[0];
    let i = argv.iter().position(|a| a == "--kill-after=5").unwrap();
    assert_eq!(argv[i + 1], "122");
    assert!(argv.contains(&"--disable-input".to_string()));
}

#[test]
fn show_again_while_shown_reuses_the_viewer_and_reports_its_options() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", "--timeout", "600", &tab]).0, 0);
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).unwrap()).unwrap();
    assert_eq!(state["view_only"], false);
    // Asked with another timeout: the running viewer's remaining time is
    // what counts and what is printed.
    let (code, out, _) = f.cli(&["show", "--timeout", "30", &tab]);
    assert_eq!(code, 0);
    let l = &lines(&out)[0];
    assert_eq!(l["already_shown"], true);
    assert_eq!(l["view_only"], false);
    let left = l["timeout"].as_f64().unwrap();
    assert!((590.0..=600.0).contains(&left), "{left}");
    assert_eq!(viewer_runs(&f).len(), 1);
}

#[test]
fn show_refuses_a_different_mode_while_shown() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    // An interactive viewer is running: --view-only can't be promised.
    let (code, out, err) = f.cli(&["show", "--view-only", &tab]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("already shown interactive; `agent-display tab unshow"), "{err}");
    assert!(stopped(&f).is_empty(), "the running viewer is left alone");
    assert_eq!(f.cli(&["unshow", &tab]).0, 0);
    assert_eq!(f.cli(&["show", "--view-only", &tab]).0, 0);
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 1);
    assert!(err.contains("already shown view-only"), "{err}");
    // Unknown options (state file lost): refuse rather than guess.
    std::fs::remove_file(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).unwrap();
    let (code, _, err) = f.cli(&["show", "--view-only", &tab]);
    assert_eq!(code, 1);
    assert!(err.contains("unknown options"), "{err}");
}

#[test]
fn waiting_on_someone_elses_viewer_never_stops_it() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    // A second caller waits for the human, who leaves; the first caller's
    // viewer exits by itself (--exit-on-disconnect), not by this show.
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0)]);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", &tab]);
    assert_eq!(code, 0);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "disconnected"}));
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
    // Its own --timeout ends its wait (3) and still leaves the viewer up.
    let (code, out, _) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
}

#[test]
fn viewer_killed_by_its_own_timer_at_the_deadline_is_a_timeout() {
    // The review's case: wayvnc's timer fires, the event stream closes and
    // the unit is already inactive. At the deadline that is exit 3, not a
    // normal stop.
    let mut f = fx();
    let (tab, _) = opened(&f);
    let starts = std::rc::Rc::new(std::cell::Cell::new(0));
    let s2 = starts.clone();
    // Active while show starts it (socket check), inactive once it is "killed".
    f.sys.active_fn = Some(Box::new(move |u| {
        if !u.starts_with("agent-display-vnc-") {
            return true;
        }
        s2.set(s2.get() + 1);
        s2.get() <= 1
    }));
    f.sys.active = None;
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), VncEvent::Closed]);
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "0.5", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
}

#[test]
fn foreground_show_prints_viewer_events_and_ends_when_the_viewer_leaves() {
    let f = fx();
    let (tab, _) = opened(&f);
    f.sys.vnc_script.borrow_mut().extend([VncEvent::Idle, ev("client-connected", 1), VncEvent::Idle, ev("client-disconnected", 0)]);
    let (code, out, err) = f.cli(&["show", "--wait-viewer", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert_eq!(l[0]["shown"], tab.as_str());
    assert_eq!(&l[1..], &[
        json!({"viewer": "connected", "clients": 1}),
        json!({"viewer": "disconnected", "clients": 0}),
        json!({"unshown": tab, "reason": "disconnected"}),
    ]);
    assert_eq!(*f.sys.vnc_opened.borrow(), vec![f.t.rt.join("agent-display/vnc/HEADLESS-2.ctl")]);
    assert!(stopped(&f).contains(&"agent-display-vnc-headless-2".to_string()));
    assert!(!f.t.rt.join("agent-display/vnc/HEADLESS-2.sock").exists());
}

#[test]
fn until_url_ends_the_view_without_printing_the_url() {
    let f = fx();
    let (tab, _) = opened(&f);
    {
        let mut st = f.b.st();
        let t = st.targets.iter_mut().find(|(k, _)| *k == tab).unwrap();
        t.1["url"] = json!("https://bank.example/home?code=SECRET123");
    }
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), VncEvent::Idle]);
    let (code, out, err) = f.cli(&["show", "--until-url", r"^https://bank\.example/home", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert_eq!(l.last().unwrap(), &json!({"unshown": tab, "reason": "url-matched"}));
    assert!(!out.contains("SECRET123") && !err.contains("SECRET123"));
    assert!(stopped(&f).contains(&"agent-display-vnc-headless-2".to_string()));
}

fn vanish_after_lookup(f: &Fx) {
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n > 1 {
                return Hook::Return(Err(Error::new("Target.getTargetInfo: No target with given id found")));
            }
        }
        Hook::Pass
    });
}

#[test]
fn until_closed_ends_the_view_when_the_tab_closes() {
    let f = fx();
    let (tab, _) = opened(&f);
    vanish_after_lookup(&f);
    f.sys.vnc_script.borrow_mut().extend(std::iter::repeat_n(VncEvent::Idle, 3));
    let (code, out, err) = f.cli(&["show", "--until-closed", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "closed"}));
}

#[test]
fn tab_gone_while_shown_exits_4_without_until_closed() {
    let f = fx();
    let (tab, _) = opened(&f);
    vanish_after_lookup(&f);
    f.sys.vnc_script.borrow_mut().extend(std::iter::repeat_n(VncEvent::Idle, 3));
    let (code, out, _) = f.cli(&["show", "--wait-viewer", &tab]);
    assert_eq!(code, 4);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "tab-gone"}));
}

#[test]
fn show_timeout_exits_3_and_stops_the_viewer() {
    let f = fx();
    let (tab, _) = opened(&f);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    assert!(stopped(&f).contains(&"agent-display-vnc-headless-2".to_string()));
}

#[test]
fn viewer_that_exits_without_a_client_ends_the_wait() {
    let mut f = fx();
    let (tab, _) = opened(&f);
    // `tab unshow` (or the viewer's own timeout) stopped it: unit inactive.
    f.sys.active_fn = Some(Box::new(|u| !u.starts_with("agent-display-vnc-")));
    f.sys.active = None;
    f.sys.vnc_script.borrow_mut().extend([VncEvent::Idle, VncEvent::Closed]);
    // show itself sees the unit inactive after start only via the socket.
    let (code, out, err) = f.cli(&["show", "--wait-viewer", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "stopped"}));
}

#[test]
fn unshow_stops_the_viewer_and_is_idempotent() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    let (code, out, _) = f.cli(&["unshow", &tab]);
    assert_eq!(code, 0);
    assert_eq!(out.trim_end(), format!(r#"{{"unshown": "{tab}", "output": "HEADLESS-2", "was_shown": true}}"#));
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
    assert!(!f.t.rt.join("agent-display/vnc/HEADLESS-2.sock").exists());
    let (code, out, _) = f.cli(&["unshow", &tab]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["was_shown"], false);
    assert_eq!(f.cli(&["unshow", "../etc"]).0, 1);
}

#[test]
fn closing_the_tab_stops_its_viewer() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(f.cli(&["close", &tab]).0, 0);
    assert!(stopped(&f).contains(&"agent-display-vnc-headless-2".to_string()));
    assert!(!f.t.rt.join("agent-display/vnc/HEADLESS-2.sock").exists());
}

#[test]
fn show_refuses_a_tab_without_its_own_output() {
    let f = fx();
    f.sw().st().fail_create = true;
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 1);
    assert!(err.contains("has no output of its own"), "{err}");
    assert!(viewer_runs(&f).is_empty());
}

#[test]
fn show_errors_use_the_shared_exit_codes() {
    let f = fx();
    let (tab, _) = opened(&f);
    let (code, _, err) = f.cli(&["show", "--until-url", "(", &tab]);
    assert_eq!(code, 2, "bad regex is a usage error: {err}");
    let (code, _, _) = f.cli(&["show", "--timeout", "0", &tab]);
    assert_eq!(code, 2);
    f.b.st().targets.retain(|(k, _)| *k != tab);
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 4, "{err}");
    assert!(viewer_runs(&f).is_empty());
}
