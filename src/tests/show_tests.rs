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

/// The running viewer of HEADLESS-2 is owned by another waiting `show`.
fn owned_by_another_waiter(f: &Fx) {
    let p = f.t.rt.join("agent-display/vnc/HEADLESS-2.json");
    let mut st: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    st["waiter"] = json!(true);
    std::fs::write(&p, st.to_string()).unwrap();
}

/// wayvnc exits by itself shortly after its last client leaves
/// (--exit-on-disconnect): its socket goes at the view end's settle pause.
fn viewer_exits_on_settle(f: &Fx) {
    let sock = f.t.rt.join("agent-display/vnc/HEADLESS-2.sock");
    *f.sys.sleep_hook.borrow_mut() = Some(Box::new(move |secs| {
        if secs == 0.1 {
            let _ = std::fs::remove_file(&sock);
        }
    }));
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
    owned_by_another_waiter(&f);
    // A second caller waits for the human, who leaves; the owner's viewer
    // exits by itself (--exit-on-disconnect), not by this show.
    viewer_exits_on_settle(&f);
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0)]);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", &tab]);
    assert_eq!(code, 0);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "disconnected"}));
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
    // Again, with the owner's viewer up: this caller's own --timeout ends
    // its wait (3), leaves the viewer up and says so.
    *f.sys.sleep_hook.borrow_mut() = None;
    assert_eq!(f.cli(&["unshow", &tab]).0, 0);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    owned_by_another_waiter(&f);
    f.sys.stopped.borrow_mut().clear();
    let (code, out, _) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3);
    assert_eq!(lines(&out).last().unwrap(), &json!({"done": tab, "reason": "timeout", "still_shown": true}));
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
}

#[test]
fn viewer_killed_by_its_own_timer_at_the_deadline_is_a_timeout() {
    // The review's case: wayvnc's timer fires, the event stream closes and
    // the unit is already inactive. At the deadline that is exit 3, not a
    // normal stop.
    let mut f = fx();
    let (tab, _) = opened(&f);
    // Inactive before the start (never started) and after it is "killed";
    // the start itself is seen through its socket.
    f.sys.active_fn = Some(Box::new(|u| !u.starts_with("agent-display-vnc-")));
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
    // The new viewer is up and not ours: said so, not "unshown".
    assert_eq!(lines(&out).last().unwrap(), &json!({"done": tab, "reason": "timeout", "still_shown": true}));
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
fn reshow_never_stops_a_viewer_that_is_already_running() {
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
    assert!(err.contains("may already be running (it is active); try again"), "{err}");
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
fn wait_viewer_on_another_waiters_viewer_ends_at_its_expiry_and_leaves_it() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    owned_by_another_waiter(&f);
    set_expiry(&f, 0.05);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", "--timeout", "60", &tab]);
    assert_eq!(code, 3);
    assert_eq!(lines(&out).last().unwrap(), &json!({"done": tab, "reason": "timeout", "still_shown": true}));
    assert!(stopped(&f).is_empty(), "not ours to stop");
}

#[test]
fn viewer_start_is_locked_and_its_state_written_once_it_runs() {
    // Concurrent shows of one tab are serialised by vnc/HEADLESS-N.lock, so
    // the options can be written after wayvnc starts (when a previous
    // viewer's ExecStopPost can no longer remove them): no other `show` can
    // look in between.
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    // At systemd-run: lock held, state not written yet (old files cleared).
    assert_eq!(*f.sys.viewer_start_seen.borrow(), vec![(false, true)]);
    let st: Value =
        serde_json::from_str(&std::fs::read_to_string(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).unwrap()).unwrap();
    assert_eq!(st["token"].as_str().unwrap().len(), 16);
    // Released afterwards: later shows aren't blocked.
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

// ------------------------------------------------------------ round 4

/// vnc units: inactive for the first `n` state checks (the old viewer is
/// stopping), active after (ours runs).
fn vnc_active_after(f: &mut Fx, n: usize) {
    let checks = std::rc::Rc::new(std::cell::Cell::new(0));
    f.sys.active_fn = Some(Box::new(move |u| {
        if !u.starts_with("agent-display-vnc-") {
            return true;
        }
        checks.set(checks.get() + 1);
        checks.get() > n
    }));
    f.sys.active = None;
}

#[test]
fn reshow_during_the_old_units_stop_keeps_the_new_state_and_token() {
    // The old viewer's ExecStopPost removes the state the new show wrote
    // before its retry: the state (and token) must still be there after.
    let mut f = fx();
    let (tab, _) = opened(&f);
    *f.sys.run_still_loaded.borrow_mut() = 1;
    vnc_active_after(&mut f, 2); // show's check, start_wayvnc's check after the refusal
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out)[0]["already_shown"], false);
    let st: Value =
        serde_json::from_str(&std::fs::read_to_string(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).unwrap()).unwrap();
    assert_eq!(st["view_only"], false);
    assert_eq!(st["token"].as_str().unwrap().len(), 16);
    // Another show finds its options (no "unknown options" refusal).
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out)[0]["already_shown"], true);
}

#[test]
fn starter_stops_its_viewer_after_a_reshow_during_the_old_units_stop() {
    let mut f = fx();
    let (tab, _) = opened(&f);
    *f.sys.run_still_loaded.borrow_mut() = 1;
    vnc_active_after(&mut f, 2);
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    // One stop to wait out the old unit, one by end() for our own viewer.
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2", "agent-display-vnc-headless-2"]);
    assert!(!f.t.rt.join("agent-display/vnc/HEADLESS-2.json").exists());
}

// ------------------------------------------------------------ round 5
// An unknown unit state (systemctl failed/timed out) is not "stopped":
// stop the unit anyway and keep its files unless that worked.

/// [socket, state] of HEADLESS-2's viewer exist (the fake wayvnc makes no
/// control socket).
fn vnc_files(f: &Fx) -> Vec<bool> {
    ["sock", "json"].iter().map(|e| f.t.rt.join(format!("agent-display/vnc/HEADLESS-2.{e}")).exists()).collect()
}

#[test]
fn unshow_stops_a_viewer_whose_state_is_unknown() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    let (code, out, err) = f.cli(&["unshow", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["was_shown"], true);
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
    assert_eq!(vnc_files(&f), vec![false; 2]);
}

#[test]
fn failed_stop_keeps_the_files_and_unshow_can_retry() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    *f.sys.stop_fail.borrow_mut() = true;
    let (code, out, err) = f.cli(&["unshow", &tab]);
    assert_eq!((code, out.as_str()), (75, ""), "{err}");
    assert!(err.contains("could not stop the viewer agent-display-vnc-headless-2"), "{err}");
    assert_eq!(vnc_files(&f), vec![true; 2], "kept for a retry");
    *f.sys.stop_fail.borrow_mut() = false;
    let (code, out, _) = f.cli(&["unshow", &tab]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["was_shown"], true);
    assert_eq!(vnc_files(&f), vec![false; 2]);
}

#[test]
fn foreground_show_stops_its_viewer_when_the_state_is_unknown() {
    // systemctl can't answer for the viewer unit. The start still happens
    // (unknown isn't "running"); the view's end must still stop it.
    let f = fx();
    let (tab, _) = opened(&f);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1)]);
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
    assert_eq!(vnc_files(&f), vec![false; 2]);
}

#[test]
fn foreground_show_reports_a_failed_stop_and_keeps_the_files() {
    let f = fx();
    let (tab, _) = opened(&f);
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1)]);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    *f.sys.stop_fail.borrow_mut() = true;
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    // The view's outcome is still its outcome; the failed stop is on stderr
    // and the last line says the viewer is still up.
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"done": tab, "reason": "timeout", "still_shown": true}));
    assert!(err.contains("could not stop the viewer agent-display-vnc-headless-2"), "{err}");
    assert!(err.contains(&format!("agent-display tab unshow {tab}")), "{err}");
    assert_eq!(vnc_files(&f), vec![true; 2]);
}

#[test]
fn unshow_finds_a_running_viewer_whose_files_are_gone() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    for e in ["sock", "ctl", "json"] {
        let _ = std::fs::remove_file(f.t.rt.join(format!("agent-display/vnc/HEADLESS-2.{e}")));
    }
    let (code, out, _) = f.cli(&["unshow", &tab]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["was_shown"], true);
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
    // Nothing running any more: a second unshow is a no-op.
    let (_, out, _) = f.cli(&["unshow", &tab]);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["was_shown"], false);
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
}

#[test]
fn viewer_stops_happen_under_the_viewer_lock() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(f.cli(&["unshow", &tab]).0, 0);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(f.cli(&["close", &tab]).0, 0); // gc stops it
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1)]);
    let (tab2, _) = {
        let o = f.open(&[]);
        (o["tab"].as_str().unwrap().to_string(), o["output"].as_str().unwrap().to_string())
    };
    let _ = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab2]); // end()
    let seen = f.sys.stop_lock_seen.borrow().clone();
    assert!(seen.len() >= 2 && seen.iter().all(|h| *h), "{seen:?}");
}

// ------------------------------------------------------------ browser gone mid-view
// The poll connection breaks when the browser exits: the view ends at once
// (not at --timeout), as browser-down (5), or closed (0) for --until-closed.

/// After the lookup's getTargetInfo, the browser exits: later polls on the
/// old connection fail like a closed WebSocket.
fn browser_exits_after_lookup(f: &Fx) {
    let b = f.b.clone();
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n > 1 {
                b.st().down = true;
                return Hook::Return(Err(Error::new("CDP closed the connection")));
            }
        }
        Hook::Pass
    });
}

#[test]
fn until_url_ends_with_browser_down_when_the_browser_exits() {
    let f = fx();
    let (tab, _) = opened(&f);
    browser_exits_after_lookup(&f);
    // `browser stop`'s gc stopped the viewer too.
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let t0 = std::time::Instant::now();
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", "--timeout", "30", &tab]);
    assert_eq!(code, 5, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "browser-down"}));
    assert!(t0.elapsed().as_secs_f64() < 5.0, "not at --timeout: {:?}", t0.elapsed());
}

#[test]
fn until_closed_ends_closed_when_the_browser_exits() {
    let f = fx();
    let (tab, _) = opened(&f);
    browser_exits_after_lookup(&f);
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let t0 = std::time::Instant::now();
    let (code, out, err) = f.cli(&["show", "--until-closed", "--timeout", "30", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "closed"}));
    assert!(t0.elapsed().as_secs_f64() < 5.0, "{:?}", t0.elapsed());
}

#[test]
fn wait_viewer_ends_and_stops_its_viewer_when_the_browser_exits() {
    // A crashed browser leaves the viewer up on an empty output: the view
    // must end and stop it, not wait for the human or the timeout.
    let f = fx();
    let (tab, _) = opened(&f);
    browser_exits_after_lookup(&f);
    f.sys.vnc_script.borrow_mut().extend(std::iter::repeat_n(VncEvent::Idle, 50));
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "30", &tab]);
    assert_eq!(code, 5, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "browser-down"}));
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
}

#[test]
fn broken_connection_to_a_live_tab_reconnects_and_keeps_watching() {
    // The poll connection drops but the browser and tab are fine: look the
    // tab up again on a new connection; the sign-in then lands (0).
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    let t2 = tab.clone();
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n == 2 {
                return Hook::Return(Err(Error::os("[Errno 32] Broken pipe")));
            }
            if n >= 8 {
                return Hook::Return(Ok(json!({"targetInfo": {"targetId": t2, "type": "page",
                    "url": "https://bank.example/home", "title": ""}})));
            }
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let before = *f.sys.version_calls.borrow();
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", "--timeout", "30", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "url-matched"}));
    // lookup + tab_alive's probe + the reconnect's lookup: 3 connections.
    assert!(*f.sys.version_calls.borrow() - before >= 3, "reconnected");
}

#[test]
fn stalled_polls_are_busy_not_gone() {
    // Polls that time out on a browser that is still registered but not
    // answering: the view keeps going (until --timeout here), never "gone".
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n > 1 {
                return Hook::Return(Err(Error::new("timed out waiting for Target.getTargetInfo")));
            }
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let before = *f.sys.version_calls.borrow();
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", "--timeout", "1.5", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    // Within STALL_RECHECK a timeout is just busy: no fresh lookups, only
    // the one connection the show's own lookup made.
    assert_eq!(*f.sys.version_calls.borrow() - before, 1);
}

// ------------------------------------------------------------ round 6
// Starting a viewer never treats an unknown state as "no viewer".

fn token_of(f: &Fx) -> Option<String> {
    let st: Value = serde_json::from_str(&std::fs::read_to_string(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).ok()?).ok()?;
    st["token"].as_str().map(String::from)
}

#[test]
fn show_never_starts_over_a_viewer_whose_state_is_unknown() {
    // The reviewer's case: someone's viewer runs, systemctl can't answer;
    // a second show must not unlink its files or stop it: 75, untouched.
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    let token = token_of(&f).unwrap();
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!((code, out.as_str()), (75, ""), "{err}");
    assert!(err.contains("can't tell whether the viewer of HEADLESS-2 is running (systemctl failed); try again"), "{err}");
    assert_eq!(token_of(&f).as_deref(), Some(token.as_str()), "its state and token stay");
    assert!(f.t.rt.join("agent-display/vnc/HEADLESS-2.sock").exists());
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
    assert_eq!(viewer_runs(&f).len(), 1, "no second start");
    // systemctl answers again: the viewer is reused.
    *f.sys.unknown_prefix.borrow_mut() = None;
    let (code, out, _) = f.cli(&["show", &tab]);
    assert_eq!(code, 0);
    assert_eq!(lines(&out)[0]["already_shown"], true);
}

#[test]
fn already_loaded_with_an_unknown_state_is_75_not_a_stop() {
    // No files of ours, state unknown, yet systemd-run says the unit is
    // loaded: something runs there. Don't stop it.
    let f = fx();
    let (tab, _) = opened(&f);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    *f.sys.run_still_loaded.borrow_mut() = 1;
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 75, "{err}");
    assert!(err.contains("may already be running (its state can't be read (systemctl failed))"), "{err}");
    assert!(stopped(&f).is_empty(), "{:?}", stopped(&f));
}

#[test]
fn reshow_while_the_old_viewer_is_deactivating_waits_it_out() {
    // A stopping unit (not unknown) is still "start": wait it out, retry.
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    f.sys.raw_state.borrow_mut().insert("agent-display-vnc-headless-2".into(), "deactivating".into());
    *f.sys.run_still_loaded.borrow_mut() = 1;
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out)[0]["already_shown"], false);
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"], "waited out the old unit");
    assert_eq!(token_of(&f).map(|t| t.len()), Some(16));
}

#[test]
fn active_viewer_without_its_socket_is_left_alone() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    let token = token_of(&f).unwrap();
    std::fs::remove_file(f.t.rt.join("agent-display/vnc/HEADLESS-2.sock")).unwrap();
    let (code, _, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 75, "{err}");
    assert!(err.contains("running without its socket; `agent-display tab unshow"), "{err}");
    assert_eq!(token_of(&f).as_deref(), Some(token.as_str()));
    assert!(stopped(&f).is_empty());
    // unshow then stops it, and a show starts afresh.
    assert_eq!(f.cli(&["unshow", &tab]).0, 0);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
}

// ------------------------------------------------------------ round 7

fn move_window(f: &Fx, tab: &str, to: &str) {
    let mut st = f.sw().st();
    let id = *st.wins.iter().find(|(_, w)| w.tab.as_deref() == Some(tab)).unwrap().0;
    st.wins.get_mut(&id).unwrap().output = to.into();
}

#[test]
fn show_when_the_window_stays_off_its_output_is_75_and_starts_nothing() {
    let f = fx();
    let (tab, output) = opened(&f);
    move_window(&f, &tab, "HEADLESS-1");
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!((code, out.as_str()), (75, ""), "{err}");
    assert!(err.contains(&format!("tab {tab}'s window is not on {output} right now; try again")), "{err}");
    assert!(viewer_runs(&f).is_empty());
    // It waited ~2 s for the window first.
    let waited: f64 = f.sys.sleeps.borrow().iter().filter(|s| **s == 0.05).sum();
    assert!((1.9..=2.1).contains(&waited), "{waited}");
}

#[test]
fn show_waits_for_a_window_that_is_briefly_off_its_output() {
    // The window comes back after a few polls (a dialog closed): show goes on.
    let f = fx();
    let (tab, output) = opened(&f);
    move_window(&f, &tab, "HEADLESS-1");
    let sw = f.sw().clone();
    let t2 = tab.clone();
    let polls = std::rc::Rc::new(std::cell::Cell::new(0));
    let p2 = polls.clone();
    // Each sleep is one poll; put the window back on the 5th.
    *f.sys.sleep_hook.borrow_mut() = Some(Box::new(move |secs| {
        if secs == 0.05 {
            p2.set(p2.get() + 1);
            if p2.get() == 5 {
                let mut st = sw.st();
                let id = *st.wins.iter().find(|(_, w)| w.tab.as_deref() == Some(t2.as_str())).unwrap().0;
                st.wins.get_mut(&id).unwrap().output = output.clone();
            }
        }
    }));
    let (code, out, err) = f.cli(&["show", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out)[0]["shown"], tab.as_str());
    assert_eq!(polls.get(), 5);
    assert_eq!(viewer_runs(&f).len(), 1);
}

// ------------------------------------------------------------ round 8

#[test]
fn closed_stream_with_the_viewer_state_unknown_is_not_stopped() {
    // The event stream ends but systemctl can't read the viewer unit: not
    // "stopped" (0). Keep looking until the deadline: 3.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-vnc-".into());
    f.sys.vnc_script.borrow_mut().extend([VncEvent::Closed]);
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "1.2", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    // Ours, so the end still stops it (unknown is stopped, not skipped).
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
}

// ------------------------------------------------------------ round 9
// show-then-wait: a waiting `show` takes over a viewer that a `show`
// returning at once started (nobody else would end it), and ends it.

fn state(f: &Fx) -> Value {
    serde_json::from_str(&std::fs::read_to_string(f.t.rt.join("agent-display/vnc/HEADLESS-2.json")).unwrap()).unwrap()
}

#[test]
fn state_records_whether_a_waiter_owns_the_viewer() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(state(&f)["waiter"], false, "returned at once: no waiter");
    assert_eq!(f.cli(&["unshow", &tab]).0, 0);
    // A waiting show records itself as the owner while it runs; check from
    // inside the view (the first getTargetInfo poll).
    let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
    let s2 = seen.clone();
    let p = f.t.rt.join("agent-display/vnc/HEADLESS-2.json");
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n == 2 {
                *s2.borrow_mut() = std::fs::read_to_string(&p).ok();
            }
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1)]);
    assert_eq!(f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]).0, 3);
    let st: Value = serde_json::from_str(seen.borrow().as_deref().unwrap()).unwrap();
    assert_eq!(st["waiter"], true);
}

#[test]
fn show_then_until_url_takes_the_viewer_over_and_ends_the_view() {
    // The two-step handoff: `show` (returns at once, the agent hands the
    // viewer command to the human), then `show --until-url` waits for the
    // sign-in. When it lands, the view really ends.
    let f = fx();
    let (tab, _) = opened(&f);
    let first = lines(&f.cli(&["show", &tab]).1)[0].clone();
    f.b.st().targets.iter_mut().find(|(k, _)| *k == tab).unwrap().1["url"] = json!("https://bank.example/home");
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1)]);
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert_eq!(l[0]["already_shown"], true);
    assert_eq!(l[0]["socket"], first["socket"], "the same viewer");
    assert_eq!(l.last().unwrap(), &json!({"unshown": tab, "reason": "url-matched"}));
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
    assert_eq!(vnc_files(&f), vec![false; 2]);
    assert_eq!(viewer_runs(&f).len(), 1, "taken over, not restarted");
}

#[test]
fn show_then_wait_viewer_takes_over_and_ends_at_its_timeout() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    let (code, out, _) = f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]);
    assert_eq!(code, 3);
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    assert_eq!(stopped(&f), vec!["agent-display-vnc-headless-2"]);
}

#[test]
fn a_viewer_another_waiter_owns_is_not_taken_over() {
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    owned_by_another_waiter(&f);
    let token = state(&f)["token"].clone();
    f.b.st().targets.iter_mut().find(|(k, _)| *k == tab).unwrap().1["url"] = json!("https://bank.example/home");
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"done": tab, "reason": "url-matched", "still_shown": true}));
    assert!(stopped(&f).is_empty());
    assert_eq!(state(&f)["token"], token);
}

#[test]
fn a_takeover_is_recorded_so_no_second_waiter_takes_the_viewer() {
    // While the waiter that took the viewer over runs, the state says a
    // waiter owns it (a concurrent waiting show then leaves it alone).
    let f = fx();
    let (tab, _) = opened(&f);
    assert_eq!(f.cli(&["show", &tab]).0, 0);
    assert_eq!(state(&f)["waiter"], false);
    let token = state(&f)["token"].clone();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
    let s2 = seen.clone();
    let p = f.t.rt.join("agent-display/vnc/HEADLESS-2.json");
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n == 2 {
                *s2.borrow_mut() = std::fs::read_to_string(&p).ok();
            }
        }
        Hook::Pass
    });
    assert_eq!(f.cli(&["show", "--wait-viewer", "--timeout", "0.05", &tab]).0, 3);
    let st: Value = serde_json::from_str(seen.borrow().as_deref().unwrap()).unwrap();
    assert_eq!(st["waiter"], true);
    assert_eq!(st["token"], token, "same viewer, same token");
}

#[test]
fn polls_timing_out_past_stall_recheck_look_the_tab_up_again() {
    // The browser stalls (polls time out, never a broken connection), then
    // dies. Only the STALL_RECHECK (5 s) recheck can notice: the view ends
    // as browser-down soon after 5 s, not at its 30 s --timeout.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    let b = f.b.clone();
    let mut n = 0;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" {
            n += 1;
            if n > 1 {
                b.st().down = true; // gone, but our old connection only times out
                return Hook::Return(Err(Error::new("timed out waiting for Target.getTargetInfo")));
            }
        }
        Hook::Pass
    });
    f.sys.vnc_script.borrow_mut().extend([ev("client-connected", 1), ev("client-disconnected", 0), VncEvent::Closed]);
    let t0 = std::time::Instant::now();
    let (code, out, err) = f.cli(&["show", "--until-url", "/home", "--timeout", "30", &tab]);
    let took = t0.elapsed().as_secs_f64();
    assert_eq!(code, 5, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "browser-down"}));
    assert!((5.0..10.0).contains(&took), "after the 5 s recheck, not before and not at --timeout: {took}");
}

// ------------------------------------------------------------ broken event stream
// wayvncctl ends while wayvnc keeps running: reopen the stream (without
// -w), so later viewer events are seen; back off the systemctl checks.

#[test]
fn broken_event_stream_is_reopened_and_later_viewer_events_are_seen() {
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    // The first stream breaks at once (wayvncctl died); the viewer runs on.
    f.sys.vnc_script.borrow_mut().extend([VncEvent::Closed]);
    f.sys.vnc_more_scripts.borrow_mut().push_back(vec![ev("client-connected", 1), ev("client-disconnected", 0)]);
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "3", &tab]);
    assert_eq!(code, 0, "{err}");
    let l = lines(&out);
    assert_eq!(&l[1..], &[
        json!({"viewer": "connected", "clients": 1}),
        json!({"viewer": "disconnected", "clients": 0}),
        json!({"unshown": tab, "reason": "disconnected"}),
    ]);
    // Reopened without -w (fails at once if wayvnc is gone).
    assert_eq!(*f.sys.vnc_waits.borrow(), vec![true, false]);
}

#[test]
fn broken_event_stream_backs_off_its_systemctl_checks() {
    // wayvncctl keeps failing while the unit stays active, until the view's
    // --timeout: a handful of state checks (0.5 s, 1 s, 2 s ... apart),
    // not one per 0.5 s poll for the whole view.
    let mut f = fx();
    f.sys.real_sleep_cap = Some(0.01);
    let (tab, _) = opened(&f);
    f.sys.vnc_script.borrow_mut().extend([VncEvent::Closed]);
    *f.sys.vnc_state_checks.borrow_mut() = 0;
    let (code, out, err) = f.cli(&["show", "--wait-viewer", "--timeout", "4", &tab]);
    assert_eq!(code, 3, "{err}");
    assert_eq!(lines(&out).last().unwrap(), &json!({"unshown": tab, "reason": "timeout"}));
    let checks = *f.sys.vnc_state_checks.borrow();
    // show's start decision, then 0 s, 0.5 s, 1.5 s, 3.5 s at most, end().
    assert!(checks <= 8, "{checks} systemctl calls");
    let reopens = f.sys.vnc_waits.borrow().iter().filter(|w| !**w).count();
    assert!((1..=2).contains(&reopens), "{reopens} reopens");
}

// ------------------------------------------------------------ view --exit-on-disconnect

fn view(f: &Fx, args: &[&str]) -> (i32, String, String) {
    let mut a = vec!["view", "--port", "5999"];
    a.extend_from_slice(args);
    f.t.cli(&f.sys, &a)
}

#[test]
fn view_says_what_the_running_viewer_actually_does() {
    let mut f = fx();
    let _ = opened(&f); // a running display has its runtime dir
    f.sys.port_free = Some(true);
    // Not running yet: this call starts it, with the flag.
    f.sys.active_fn = Some(Box::new(|u| u.trim_end_matches(".service") != "agent-display-vnc"));
    f.sys.active = None;
    let (code, out, err) = view(&f, &["--exit-on-disconnect"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("It stops by itself when the last viewer disconnects.\n"), "{out}");
    assert!(!out.contains("if it was started"), "no hedge: {out}");
    let run = f.sys.systemd_runs.borrow().last().unwrap().clone();
    assert!(run.contains(&"--exit-on-disconnect".to_string()));
    // Reused: it was started with the flag, and says so even if not asked.
    f.sys.active_fn = None;
    f.sys.active = Some(true);
    let (_, out, _) = view(&f, &[]);
    assert!(out.contains("It stops by itself when the last viewer disconnects."), "{out}");
    // unview forgets it; started again without the flag, then reused with
    // it: the flag can't apply, and that is said plainly.
    assert_eq!(f.t.cli(&f.sys, &["unview"]).0, 0);
    f.sys.active_fn = Some(Box::new(|u| u.trim_end_matches(".service") != "agent-display-vnc"));
    f.sys.active = None;
    let (_, out, _) = view(&f, &[]);
    assert!(!out.contains("stops by itself"), "{out}");
    f.sys.active_fn = None;
    f.sys.active = Some(true);
    let (code, out, _) = view(&f, &["--exit-on-disconnect"]);
    assert_eq!(code, 0);
    assert!(out.contains("was started without --exit-on-disconnect and keeps its options"), "{out}");
    assert!(out.contains("`agent-display unview` first"), "{out}");
}
