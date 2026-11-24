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

// ------------------------------------------------------------ --until-*
// Exit 0 only when the condition is met; a viewer leaving first doesn't end
// the wait (the sign-in can finish on the human's phone).

fn set_url(f: &Fx, tab: &str, url: &str) {
    let mut st = f.b.st();
    st.targets.iter_mut().find(|(k, _)| k == tab).unwrap().1["url"] = json!(url);
}

#[test]
fn until_url_not_met_after_the_viewer_leaves_is_a_timeout_not_success() {
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    set_url(&f, &tab, "https://bank.example/login");
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    // Above the 1 s grace: the viewer leaving now is not the timeout.
    let (code, out, err) = f.cli(&["show", "--until-url", r"/home", "--timeout", "1.5", &tab]);
    assert_eq!(code, 3, "{err}");
    let l = lines(&out);
    assert_eq!(&l[1..], &[
        json!({"viewer": "connected", "clients": 1}),
        json!({"viewer": "disconnected", "clients": 0}),
        json!({"viewer": "stopped"}),
        json!({"unshown": tab, "reason": "timeout"}),
    ]);
}

#[test]
fn until_url_met_after_the_viewer_left_is_success() {
    // The human typed the SMS code and closed the viewer; the bank redirects
    // once the phone approval lands.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    set_url(&f, &tab, "https://bank.example/login");
    let polls = std::rc::Rc::new(std::cell::Cell::new(0));
    let p2 = polls.clone();
    let t2 = tab.clone();
    // The 5th getTargetInfo (well after the viewer left) sees the redirect.
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            p2.set(p2.get() + 1);
            if p2.get() >= 5 {
                return Hook::Return(Ok(json!({"targetInfo": {"targetId": t2, "type": "page",
                    "url": "https://bank.example/home?code=SECRET", "title": ""}})));
            }
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let (code, out, err) = f.cli(&["show", "--until-url", r"^https://bank\.example/home", "--timeout", "10", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert_eq!(l.last().unwrap(), &json!({"unshown": tab, "reason": "url-matched"}));
    assert!(l.contains(&json!({"viewer": "stopped"})));
    assert!(!out.contains("SECRET"));
    assert!(polls.get() >= 5);
}

#[test]
fn until_closed_waits_past_the_viewer_and_exits_0_when_the_tab_closes() {
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    // Found by the lookup and the first polls, then closed (every later
    // getTargetInfo, including the double check, says no such target).
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n > 4 {
                return Hook::Return(Err(Error::new("Target.getTargetInfo: No target with given id found")));
            }
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let (code, out, err) = f.cli(&["show", "--until-closed", "--timeout", "10", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert!(l.contains(&json!({"viewer": "stopped"})));
    assert_eq!(l.last().unwrap(), &json!({"unshown": tab, "reason": "closed"}));
}

#[test]
fn wait_viewer_alone_still_ends_on_disconnect() {
    let f = fx();
    let (tab, _) = opened(&f);
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0)]);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", &tab]);
    assert_eq!(code, 0);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "disconnected"}));
}

// ------------------------------------------------------------ ownership
// A `show` stops only the viewer it started, and only while it still runs:
// never a later viewer on the same output.

/// From the Nth getTargetInfo on: "our viewer exited (its ExecStopPost
/// cleared its files) and another `show` started a new one" on HEADLESS-2.
fn replace_viewer_after(f: &Fx, n: usize) -> std::path::PathBuf {
    let state = f.t.rt.join("agent-display/vnc/HEADLESS-2.json");
    let st = state.clone();
    let mut calls = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            calls += 1;
            if calls == n {
                std::fs::write(&st, r#"{"view_only": false, "expires": 9999999999.0, "token": "othershow0000000"}"#).unwrap();
            }
        }
        Hook::Pass
    });
    state
}

#[test]
fn until_waiter_never_stops_a_later_viewer_on_the_same_output() {
    // The reviewer's case: --until-url, the human leaves (our viewer
    // exits), the agent re-shows the tab (new viewer), then the first
    // waiter times out. The new viewer must survive.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    let state = replace_viewer_after(&f, 4);
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", "--timeout", "1.5", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    assert!(stopped(&f).is_empty(), "stopped someone else's viewer: {:?}", stopped(&f));
    assert!(std::fs::read_to_string(&state).unwrap().contains("othershow0000000"));
}

#[test]
fn waiter_never_stops_a_viewer_that_replaced_its_own() {
    // Same output, ours replaced while the waiter still runs (another
    // agent's `unshow` + `show`): the token in the state file is no longer
    // ours, so the waiter's end leaves the new viewer alone.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    replace_viewer_after(&f, 2);
    f.sys.vnc_script.borrow_mut().extend([VncEvent::Idle, VncEvent::Closed]);
    let (code, _, err) = f.cli(&["show", "--wait-viewer", "--timeout", "0.3", &tab]);
    assert_eq!(code, 3, "{err}");
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
}

#[test]
fn starter_still_stops_its_own_viewer_and_records_a_token() {
    let f = fx();
    let (tab, _) = opened(&f);
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1)]);
    let (code, _, _) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3);
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
    // A fresh start records a new token.
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    let st: Value =
        serde_json::from_str(&std::fs::read_to_string(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).unwrap()).unwrap();
    assert_eq!(st["token"].as_str().unwrap().len(), 16);
}

#[test]
fn reshow_waits_out_a_viewer_that_is_still_stopping() {
    // Found live: right after the last client leaves, the old transient
    // unit is still loaded (ExecStopPost); systemd-run refuses the name.
    let mut f = fx();
    let (tab, _) = opened(&f);
    *f.sys.run_still_loaded.borrow_mut() = 2;
    // Not active (stopping): wait it out, then start.
    f.sys.active_fn = Some(Box::new(|u| !u.starts_with("agent-display-vnc-")));
    f.sys.active = None;
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out)[0]["already_shown"], false);
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2", "agent-display-vnc-headless-2"]);
    assert_eq!(f.sys.sleeps.borrow().iter().filter(|s| **s == 0.2).count(), 2);
}

#[test]
fn reshow_never_stops_a_viewer_another_call_just_started() {
    let mut f = fx();
    let (tab, _) = opened(&f);
    *f.sys.run_still_loaded.borrow_mut() = 1;
    // Inactive when we checked, active by the time systemd-run refused:
    // another `show` won the race. Leave its viewer alone; 75.
    let checks = std::rc::Rc::new(std::cell::Cell::new(0));
    let c2 = checks.clone();
    f.sys.active_fn = Some(Box::new(move |u| {
        if !u.starts_with("agent-display-vnc-") {
            return true;
        }
        c2.set(c2.get() + 1);
        c2.get() > 1
    }));
    f.sys.active = None;
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 75, "{err}");
    assert!(err.contains("was just started by another call; try again"), "{err}");
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
}

// ------------------------------------------------------------ round 3

fn set_expiry(f: &Fx, secs_from_now: f64) {
    let p = f.t.rt.join("agent-display/vnc/HEADLESS-2.json");
    let mut st: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    st["expires"] = json!(now + secs_from_now);
    std::fs::write(&p, st.to_string()).unwrap();
}

#[test]
fn until_url_on_a_reused_viewer_waits_its_own_timeout_not_the_viewers_expiry() {
    // Another caller's viewer has 0.3 s left; this call asks --until-url
    // with --timeout 5. The viewer expires, the sign-in lands at ~0.6 s:
    // exit 0, not a timeout at the other viewer's expiry.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    set_expiry(&f, 0.3);
    let start = std::time::Instant::now();
    let t2 = tab.clone();
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" && start.elapsed().as_secs_f64() > 0.6 {
            return Hook::Return(Ok(json!({"targetInfo": {"targetId": t2, "type": "page",
                "url": "https://bank.example/home", "title": ""}})));
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", "--timeout", "5", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert_eq!(l[0]["already_shown"], true);
    assert_eq!(l.last().unwrap(), &json!({"unshown": tab, "reason": "url-matched"}));
    assert!(start.elapsed().as_secs_f64() > 0.6);
}

#[test]
fn wait_viewer_on_a_reused_viewer_still_ends_at_its_expiry() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    set_expiry(&f, 0.05);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", "--timeout", "60", &tab]);
    assert_eq!(code, 3);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    assert!(stopped(&f).is_empty(), "not ours to stop");
}

#[test]
fn viewer_state_exists_before_wayvnc_and_the_start_is_locked() {
    // A concurrent show that finds the socket must also find the options,
    // and two shows of one tab can't both start a viewer.
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(*f.sys.viewer_start_seen.borrow(), vec![(true, true)]);
    // Released afterwards: a later show isn't blocked.
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(f.cli(&["unshow", &tab]).0, 0);
    assert_eq!(f.cli(&["show", "--view-only", &tab]).0, 0);
    assert_eq!(f.sys.viewer_start_seen.borrow().len(), 2);
}

#[test]
fn failed_start_leaves_no_state_behind() {
    let f = fx();
    let (tab, _) = opened(&f);
    // systemd-run keeps refusing (still loaded) and the unit never becomes
    // active: the start fails and its early state file is removed.
    *f.sys.run_still_loaded.borrow_mut() = 100;
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_ne!(code, 0, "{err}");
    assert!(!f.t.rt.join("agent-display/vnc/HEADLESS-2.json").exists());
    assert!(!f.t.rt.join("agent-display/vnc/HEADLESS-2.sock").exists());
}
