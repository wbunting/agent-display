//! `tab wait` against the fake CDP (see fakes.rs) with fake time: the page
//! side (`Runtime.callFunctionOn` of CHECK_FN) is scripted per test, so
//! these cover the Rust side: condition plumbing, --any, --stable-ms,
//! --follow, stalls, reconnects, closed/crashed/browser-down evidence,
//! dialogs, timeouts and errors. CHECK_FN itself is tested in node below.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use serde_json::{Value, json};

use super::fakes::*;
use super::tab_tests::Fx;
use crate::cdp::{DETACHED, Dialog};
use crate::util::Error;

const TAB: &str = "OTHERTAB00000001";
const SESSION: &str = "S-OTHERTAB00000001";

/// What the page answers to one check.
enum Step {
    /// A page state: (url, title, matched).
    State(&'static str, &'static str, Value),
    /// Same state as before: the page waits out the tick.
    Same,
    Err(Error),
    Exception(&'static str),
    /// Run something (with the browser), then answer the next step.
    Do(Box<dyn FnMut(&FakeBrowser)>),
}

fn state(url: &'static str, title: &'static str, matched: Value) -> Step {
    Step::State(url, title, matched)
}

fn reply(url: &str, title: &str, matched: &Value) -> Value {
    let st = json!({"url": url, "title": title, "matched": matched});
    json!({"result": {"type": "object", "value": {"key": serde_json::to_string(&st).unwrap(), "state": st}}})
}

/// Script the page. After the script ends the page keeps its last state
/// and every check waits out its tick (so timeouts end the test).
fn script(f: &Fx, steps: Vec<Step>) {
    let steps = Rc::new(RefCell::new(VecDeque::from(steps)));
    let last: Rc<RefCell<(String, String, Value)>> = Rc::new(RefCell::new(("https://a/".into(), "theirs".into(), json!({}))));
    let clock = f.sys.clock.clone();
    let b = f.b.clone();
    f.b.set_hook(move |m, p| {
        if m != "Runtime.callFunctionOn" {
            return Hook::Pass;
        }
        let tick = p["arguments"][2]["value"].as_f64().unwrap() / 1000.0;
        loop {
            let step = steps.borrow_mut().pop_front();
            match step {
                Some(Step::Do(mut f)) => {
                    f(&b);
                    continue;
                }
                Some(Step::State(u, t, m)) => {
                    *last.borrow_mut() = (u.into(), t.into(), m);
                    let l = last.borrow();
                    // The page answers at once only if this differs from
                    // what the caller saw last; else after the tick.
                    let r = reply(&l.0, &l.1, &l.2);
                    if r["result"]["value"]["key"] == p["arguments"][1]["value"] {
                        clock.set(clock.get() + tick);
                    }
                    return Hook::Return(Ok(r));
                }
                Some(Step::Err(e)) => return Hook::Return(Err(e)),
                Some(Step::Exception(text)) => {
                    return Hook::Return(Ok(json!({"result": {"type": "object"},
                        "exceptionDetails": {"text": "Uncaught", "exception": {"description": text}}})));
                }
                Some(Step::Same) | None => {
                    clock.set(clock.get() + tick);
                    let l = last.borrow();
                    return Hook::Return(Ok(reply(&l.0, &l.1, &l.2)));
                }
            }
        }
    });
}

/// (exit code, JSON lines, stderr).
fn wait(f: &Fx, args: &[&str]) -> (i32, Vec<Value>, String) {
    let mut a = vec!["wait", TAB];
    a.extend_from_slice(args);
    let (code, out, err) = f.cli(&a);
    let lines = out.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l}: {e}"))).collect();
    (code, lines, err)
}

fn events(lines: &[Value]) -> Vec<String> {
    lines.iter().filter_map(|l| l["event"].as_str().map(String::from)).collect()
}

fn checks(f: &Fx) -> Vec<Value> {
    f.calls().into_iter().filter(|c| c.0 == "Runtime.callFunctionOn").map(|c| c.1).collect()
}

fn fx() -> Fx {
    Fx::new(false)
}

/// A browser that has gone away: `/json/version` fails, its unit stops.
fn browser_gone(f: &mut Fx) {
    let down = Rc::new(Cell::new(false));
    let d = down.clone();
    f.sys_version_fail(move |_| d.get().then(|| Error::os("<urlopen error [Errno 111] Connection refused>")));
    let d = down.clone();
    f.sys.active = None;
    f.sys.active_fn = Some(Box::new(move |_| !d.get()));
    script(
        f,
        vec![
            state("https://a/login", "Log in", json!({"text": false})),
            Step::Do(Box::new(move |br: &FakeBrowser| {
                br.st().targets.clear();
                down.set(true);
            })),
            Step::Err(Error::new(format!("Runtime.callFunctionOn: {DETACHED}"))),
        ],
    );
}

// ------------------------------------------------------------ matching

#[test]
fn immediate_match_prints_one_line_and_stays_on_its_own_target() {
    let f = fx();
    script(&f, vec![state("https://a/home", "Home", json!({"url": true}))]);
    let (code, lines, err) = wait(&f, &["--url", "/home$"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        lines,
        vec![json!({"tab": TAB, "result": "matched", "url": "https://a/home", "title": "Home",
                    "matched": {"url": true}, "elapsed": 0.0})]
    );
    let m = f.methods();
    for never in ["Runtime.enable", "Target.activateTarget", "Page.bringToFront", "Target.setAutoAttach", "Target.closeTarget"] {
        assert!(!m.contains(&never.to_string()), "{never} in {m:?}");
    }
    // Every page command went to our tab's session only.
    for c in f.calls() {
        if let Some(s) = &c.2 {
            assert_eq!(s, SESSION, "{c:?}");
        }
    }
    let world = f.calls().into_iter().find(|c| c.0 == "Page.createIsolatedWorld").unwrap();
    assert_eq!(world.1["worldName"], "agent-display-wait");
    assert_eq!(world.1["frameId"], format!("FRAME-{TAB}"));
    let check = &checks(&f)[0];
    assert_eq!(check["executionContextId"], 7);
    assert_eq!(check["awaitPromise"], true);
    let t = f.b.st().timeouts.iter().find(|(m, _)| m == "Runtime.callFunctionOn").unwrap().1;
    assert_eq!(t, Some(1.0 + crate::wait::CHECK_SLACK), "tick + slack");
    assert!(check["functionDeclaration"].as_str().unwrap().contains("const userJs = null;"));
    // Answers dialogs while attached (dismiss by default); the barrier and
    // detach end the session.
    assert_eq!(f.b.st().dialog_policy, Some(false));
    let calls = f.calls();
    let n = calls.len();
    assert_eq!(calls[n - 1].0, "Target.detachFromTarget");
    assert_eq!((calls[n - 2].0.as_str(), &calls[n - 2].1["expression"]), ("Runtime.evaluate", &json!("0")));
}

#[test]
fn every_condition_reaches_the_page_and_js_is_compiled_in() {
    let f = fx();
    let all = json!({"url": true, "url_not": true, "title": true, "text": true, "text_not": true,
                     "selector": true, "selector_not": true, "js": true});
    script(&f, vec![state("https://a/", "t", all.clone())]);
    let (code, lines, _) = wait(
        &f,
        &["--url", "A", "--url-not", "B", "--title", "C", "--text", "/D/i", "--text-not", "E", "--selector", "#f",
          "--selector-not", ".g", "--js", "window.ok === 1"],
    );
    assert_eq!(code, 0);
    assert_eq!(lines[0]["matched"], all);
    let check = &checks(&f)[0];
    assert_eq!(
        check["arguments"][0]["value"],
        json!({"url": "A", "url_not": "B", "title": "C", "text": "/D/i", "text_not": "E", "selector": "#f", "selector_not": ".g"})
    );
    let func = check["functionDeclaration"].as_str().unwrap();
    assert!(func.contains("const userJs = (() => (\nwindow.ok === 1\n));"), "{func}");
    assert_eq!(check["arguments"][1]["value"], "", "first check: nothing seen yet");
}

#[test]
fn all_conditions_must_hold_unless_any() {
    let f = fx();
    script(&f, vec![state("https://a/", "t", json!({"url": false, "title": true}))]);
    let (code, lines, _) = wait(&f, &["--url", "x", "--title", "t", "--timeout", "3"]);
    assert_eq!((code, lines.last().unwrap()["result"].as_str()), (3, Some("timeout")));

    let f = fx();
    script(&f, vec![state("https://a/", "t", json!({"url": false, "title": true}))]);
    let (code, lines, _) = wait(&f, &["--url", "x", "--title", "t", "--any"]);
    assert_eq!((code, &lines[0]["result"]), (0, &json!("matched")));
}

#[test]
fn state_changes_are_followed_and_the_last_key_is_passed_back() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"text_not": false})),
            Step::Same,
            Step::Same,
            state("https://a/login", "Log in", json!({"text_not": true})),
        ],
    );
    let (code, lines, _) = wait(&f, &["--text-not", "/waiting for your response/i", "--follow"]);
    assert_eq!(code, 0);
    assert_eq!(events(&lines), vec!["state", "state"]);
    assert_eq!(lines[0], json!({"event": "state", "tab": TAB, "t": 0.0, "url": "https://a/login", "title": "Log in",
                                "matched": {"text_not": false}}));
    assert_eq!(lines[1]["matched"], json!({"text_not": true}));
    assert_eq!(lines[1]["t"], 2.0, "two 1 s ticks without a change");
    assert_eq!(lines[2]["result"], "matched");
    // Each check passes the key of the state seen before it.
    let keys: Vec<Value> = checks(&f).iter().map(|c| c["arguments"][1]["value"].clone()).collect();
    assert_eq!(keys[0], "");
    assert_eq!(keys[1], reply("https://a/login", "Log in", &json!({"text_not": false}))["result"]["value"]["key"]);
    assert_eq!(keys[1], keys[3]);
    // Only the final line without --follow.
    let f = fx();
    script(&f, vec![state("https://a/", "t", json!({"text": false})), state("https://a/", "t", json!({"text": true}))]);
    let (_, lines, _) = wait(&f, &["--text", "x"]);
    assert_eq!(lines.len(), 1);
}

#[test]
fn stable_ms_ignores_a_redirect_bounce() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/sso", "SSO", json!({"url_not": true})),
            state("https://a/login", "Log in", json!({"url_not": false})), // bounced back
            state("https://a/home", "Home", json!({"url_not": true})),
        ],
    );
    let (code, lines, _) = wait(&f, &["--url-not", "/login", "--stable-ms", "500", "--follow"]);
    assert_eq!(code, 0);
    let fin = lines.last().unwrap();
    assert_eq!((fin["url"].as_str(), fin["elapsed"].as_f64()), (Some("https://a/home"), Some(0.5)));
    // While a match must hold, the tick shrinks to what is left of it.
    let ticks: Vec<i64> = checks(&f).iter().map(|c| c["arguments"][2]["value"].as_i64().unwrap()).collect();
    assert_eq!(ticks, vec![1000, 500, 1000, 500]);
}

#[test]
fn tick_never_runs_past_the_deadline() {
    let f = fx();
    script(&f, vec![state("https://a/", "t", json!({"text": false}))]);
    let (code, lines, _) = wait(&f, &["--text", "x", "--timeout", "2.5"]);
    assert_eq!(code, 3);
    assert_eq!(
        lines,
        vec![json!({"tab": TAB, "result": "timeout", "url": "https://a/", "title": "t", "matched": {"text": false},
                    "elapsed": 2.5, "stalled": null, "timeout": 2.5})]
    );
    let ticks: Vec<i64> = checks(&f).iter().map(|c| c["arguments"][2]["value"].as_i64().unwrap()).collect();
    assert_eq!(ticks, vec![1000, 1000, 1000, 500]);
}

#[test]
fn timeout_zero_waits_until_matched() {
    let f = fx();
    let mut steps: Vec<Step> = vec![state("https://a/", "t", json!({"text": false}))];
    steps.extend((0..100).map(|_| Step::Same));
    steps.push(state("https://a/", "t", json!({"text": true})));
    script(&f, steps);
    let (code, lines, _) = wait(&f, &["--text", "x", "--timeout", "0"]);
    assert_eq!((code, lines[0]["elapsed"].as_f64()), (0, Some(100.0)));
}

// ------------------------------------------------------------ navigation

#[test]
fn a_destroyed_world_is_recreated_without_failing() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"url": false})),
            Step::Exception("Error: Execution context was destroyed."),
            Step::Err(Error::new("Runtime.callFunctionOn: Cannot find context with specified id")),
            state("https://a/home", "Home", json!({"url": true})),
        ],
    );
    let (code, lines, _) = wait(&f, &["--url", "home", "--follow"]);
    assert_eq!(code, 0);
    assert_eq!(events(&lines), vec!["state", "state"]);
    let worlds = f.methods().iter().filter(|m| *m == "Page.createIsolatedWorld").count();
    assert_eq!(worlds, 3);
    assert_eq!(f.b.st().next_conn, 1, "a navigation is not a reconnect");
}

// ------------------------------------------------------------ errors

#[test]
fn invalid_regex_or_throwing_condition_is_an_error() {
    let f = fx();
    script(&f, vec![Step::Exception("SyntaxError: Invalid regular expression: /(/: Unterminated group\n    at new RegExp")]);
    let (code, lines, err) = wait(&f, &["--text", "("]);
    assert_eq!((code, lines.len()), (1, 0));
    assert_eq!(err, "agent-display: tab wait: the condition threw: SyntaxError: Invalid regular expression: /(/: Unterminated group\n");

    let f = fx();
    script(&f, vec![Step::Exception("TypeError: Cannot read properties of null (reading 'textContent')")]);
    let (code, _, err) = wait(&f, &["--js", "document.querySelector('#x').textContent"]);
    assert_eq!(code, 1);
    assert!(err.contains("the condition threw: TypeError"), "{err}");
}

#[test]
fn usage_errors_exit_2_with_the_usage_line() {
    let f = fx();
    let cases: &[(&[&str], &str)] = &[
        (&[], "error: give a condition: --url, --url-not,"),
        (&["--gone", "--url", "x"], "error: argument --gone: with page conditions, also pass --any"),
        (&["--url", "x", "--stable-ms", "-1"], "error: argument --stable-ms: must be >= 0\n"),
        (&["--url", "x", "--tick-ms", "0"], "error: argument --tick-ms: must be > 0\n"),
        (&["--url", "x", "--timeout=-1"], "error: argument --timeout: must be >= 0 (0: no limit)\n"),
        (&["--url", "x", "--timeout", "nan"], "error: argument --timeout: must be >= 0"),
    ];
    for (args, want) in cases {
        let (code, lines, err) = wait(&f, args);
        assert_eq!((code, lines.len()), (2, 0), "{args:?}");
        assert!(err.starts_with("usage: agent-display tab wait [-h]"), "{args:?}: {err}");
        assert!(err.contains(&format!("agent-display tab wait: {want}")), "{args:?}: {err}");
    }
    // --gone with page conditions is fine with --any.
    script(&f, vec![state("https://a/", "t", json!({"url": true}))]);
    assert_eq!(wait(&f, &["--gone", "--url", "x", "--any"]).0, 0);
    f.b.st().calls.clear();
    // A malformed tab id exits 1, like every other tab command.
    let (code, _, err) = f.cli(&["wait", "bad id!", "--url", "x"]);
    assert_eq!((code, err.as_str()), (1, "agent-display: invalid tab id 'bad id!'\n"));
    let (code, _, err) = f.cli(&["wait", TAB, "--url", "x", "--dialog", "maybe"]);
    assert_eq!(code, 2);
    assert!(err.contains("argument --dialog: invalid choice: 'maybe' (choose from 'accept', 'dismiss')"), "{err}");
    assert!(f.calls().is_empty(), "no CDP for usage errors");
}

// ------------------------------------------------------------ stalls

#[test]
fn a_slow_check_is_a_stall_on_the_same_session_not_a_wake() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"url": false})),
            Step::Err(Error::new("timed out waiting for Runtime.callFunctionOn")),
            Step::Err(Error::new("timed out waiting for Runtime.callFunctionOn")),
            Step::Same,
            state("https://a/home", "Home", json!({"url": true})),
        ],
    );
    let (code, lines, _) = wait(&f, &["--url", "home", "--follow"]);
    assert_eq!(code, 0);
    assert_eq!(events(&lines), vec!["state", "stall", "resumed", "state"]);
    assert_eq!(lines[1]["error"], "timed out waiting for Runtime.callFunctionOn");
    assert_eq!(f.b.st().next_conn, 1);
}

#[test]
fn a_stalled_browser_reconnects_with_backoff_and_resumes() {
    let mut f = fx();
    // After the first round: the connection drops and /json/version stops
    // answering for three lookups (browser frozen), then recovers. The
    // unit stays active throughout.
    let frozen = Rc::new(Cell::new(0u32));
    let fz = frozen.clone();
    f.sys_version_fail(move |_| {
        if fz.get() > 0 {
            fz.set(fz.get() - 1);
            return Some(Error::new("timed out"));
        }
        None
    });
    let fz = frozen.clone();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"url": false})),
            Step::Do(Box::new(move |_| fz.set(3))),
            Step::Err(Error::new("CDP closed the connection")),
            state("https://a/home", "Home", json!({"url": true})),
        ],
    );
    f.sys.sleeps.borrow_mut().clear();
    let (code, lines, _) = wait(&f, &["--url", "home", "--follow", "--timeout", "120"]);
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(events(&lines), vec!["state", "stall", "resumed", "state"]);
    assert_eq!(lines[1]["error"], "CDP closed the connection");
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.5, 1.0, 2.0, 4.0]);
    assert_eq!(f.b.st().next_conn, 2);
    assert_eq!(lines.last().unwrap()["result"], "matched");
}

#[test]
fn a_stall_that_outlasts_the_deadline_is_75_and_says_why() {
    let mut f = fx();
    f.sys_version_fail(|_| Some(Error::new("timed out")));
    let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", "10"]);
    assert_eq!(code, 75);
    let fin = &lines[0];
    assert_eq!((fin["result"].as_str(), fin["elapsed"].as_f64()), (Some("stalled"), Some(10.0)));
    let why = fin["stalled"].as_str().unwrap();
    assert!(why.starts_with("no definitive answer: 9425 (") && why.contains("not answering CDP"), "{fin}");
}

#[test]
fn a_timeout_with_the_page_answering_is_3() {
    // The browser stalled once but answered again before the deadline.
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/", "t", json!({"url": false})),
            Step::Err(Error::new("timed out waiting for Runtime.callFunctionOn")),
            state("https://a/", "t", json!({"url": false})),
        ],
    );
    let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", "8", "--follow"]);
    assert_eq!(code, 3);
    assert_eq!(events(&lines), vec!["state", "stall", "resumed"]);
    let fin = lines.last().unwrap();
    assert_eq!((&fin["result"], &fin["stalled"]), (&json!("timeout"), &Value::Null));
}

#[test]
fn a_detached_session_whose_tab_still_exists_reattaches() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"url": false})),
            Step::Err(Error::new(format!("Runtime.callFunctionOn: {DETACHED}"))),
            state("https://a/home", "Home", json!({"url": true})),
        ],
    );
    let (code, lines, _) = wait(&f, &["--url", "home"]);
    assert_eq!((code, &lines[0]["result"]), (0, &json!("matched")));
    assert_eq!(f.b.st().next_conn, 2, "looked up again and reattached");
    assert_eq!(f.methods().iter().filter(|m| *m == "Target.getTargetInfo").count(), 2, "one lookup by id per round");
}

// ------------------------------------------------------------ gone

fn close_mid_wait(f: &Fx) {
    script(
        f,
        vec![
            state("https://a/login", "Log in", json!({"text": false})),
            Step::Do(Box::new(|b: &FakeBrowser| b.st().targets.retain(|(k, _)| k != TAB))),
            Step::Err(Error::new(format!("Runtime.callFunctionOn: {DETACHED}"))),
        ],
    );
}

#[test]
fn tab_close_is_exit_4() {
    let f = fx();
    close_mid_wait(&f);
    let (code, lines, _) = wait(&f, &["--text", "Welcome"]);
    assert_eq!(code, 4);
    assert_eq!(
        lines,
        vec![json!({"tab": TAB, "result": "gone", "url": "https://a/login", "title": "Log in",
                    "matched": {"text": false}, "elapsed": 0.8, "reason": "closed"})]
    );
}

#[test]
fn gone_flag_matches_a_closed_tab() {
    let f = fx();
    close_mid_wait(&f);
    let (code, lines, _) = wait(&f, &["--gone"]);
    assert_eq!((code, &lines[0]["result"], &lines[0]["reason"]), (0, &json!("matched"), &json!("closed")));
    // No page conditions: nothing to match in the page.
    assert_eq!(checks(&f)[0]["arguments"][0]["value"], json!({"url": null, "url_not": null, "title": null, "text": null,
        "text_not": null, "selector": null, "selector_not": null}));
}

#[test]
fn gone_with_any_matches_either() {
    let f = fx();
    script(&f, vec![state("https://a/home", "Home", json!({"url": true}))]);
    let (code, _, _) = wait(&f, &["--gone", "--url", "home", "--any"]);
    assert_eq!(code, 0);
}

#[test]
fn a_crashed_tab_is_exit_4() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"text": false})),
            Step::Do(Box::new(|b: &FakeBrowser| {
                b.st().events.push(json!({"method": "Inspector.targetCrashed", "params": {}, "sessionId": SESSION}));
            })),
            Step::Same,
        ],
    );
    let (code, lines, _) = wait(&f, &["--text", "Welcome"]);
    assert_eq!((code, &lines[0]["result"], &lines[0]["reason"]), (4, &json!("gone"), &json!("crashed")));
}

#[test]
fn a_tab_missing_from_one_lookup_pass_is_not_gone() {
    // Pass 1 of the lookup misses the tab (a browser briefly not reporting
    // it); pass 2, 250 ms later, finds it: no stall, no "gone".
    let mut f = fx();
    f.b.st().targets.retain(|(k, _)| k != TAB);
    let b = f.b.clone();
    let lookups = Rc::new(Cell::new(0));
    let l = lookups.clone();
    f.sys_version_fail(move |_| {
        l.set(l.get() + 1);
        if l.get() == 2 {
            b.st().targets.push((TAB.into(), json!({"type": "page", "url": "https://a/", "title": "theirs"})));
        }
        None
    });
    script(&f, vec![state("https://a/home", "Home", json!({"url": true}))]);
    let (code, lines, _) = wait(&f, &["--url", "home", "--follow"]);
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(events(&lines), vec!["state"]);
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.25]);
}

#[test]
fn a_tab_missing_from_two_lookups_250_ms_apart_is_gone() {
    // The shared lookup's rule (tab::locate), as for every tab command.
    let f = fx();
    f.b.st().targets.retain(|(k, _)| k != TAB);
    f.sys.sleeps.borrow_mut().clear();
    let (code, lines, _) = wait(&f, &["--url", "x"]);
    assert_eq!(code, 4);
    assert_eq!(
        lines,
        vec![json!({"tab": TAB, "result": "gone", "url": null, "title": null, "matched": null,
                    "elapsed": 0.3, "reason": "closed"})]
    );
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.25]);
    // Each pass asks by id, creates DevTools hosts, asks again.
    assert_eq!(f.methods().iter().filter(|m| *m == "Target.getTargetInfo").count(), 4);
}

#[test]
fn browser_stop_is_exit_5() {
    let mut f = fx();
    browser_gone(&mut f);
    let (code, lines, _) = wait(&f, &["--text", "Welcome"]);
    assert_eq!(code, 5);
    assert_eq!(lines[0]["result"], "browser-down");
    assert_eq!(lines[0]["reason"], "agent-display-browser is not running");
}

#[test]
fn gone_flag_matches_when_the_browser_stops() {
    let mut f = fx();
    browser_gone(&mut f);
    let (code, lines, _) = wait(&f, &["--gone"]);
    assert_eq!(code, 0);
    assert_eq!((&lines[0]["result"], &lines[0]["reason"]), (&json!("matched"), &json!("browser-down")));
}

#[test]
fn a_browser_down_before_the_wait_is_exit_5() {
    let mut f = fx();
    f.sys_version_fail(|_| Some(Error::os("<urlopen error [Errno 111] Connection refused>")));
    f.sys.active = Some(false);
    let (code, lines, _) = wait(&f, &["--url", "x"]);
    assert_eq!((code, &lines[0]["result"]), (5, &json!("browser-down")));
    assert_eq!(f.b.st().next_conn, 0);
}

// ------------------------------------------------------------ dialogs

fn dialog_during_wait(f: &Fx, kind: &'static str) {
    let b = f.b.clone();
    script(
        f,
        vec![
            state("https://a/", "t", json!({"text": false})),
            Step::Do(Box::new(move |_| {
                // The connection answered it with the policy we set.
                let accepted = crate::cdp::answer_for(kind, b.st().dialog_policy.unwrap());
                b.st().dialogs.push(Dialog { kind: kind.into(), accepted });
            })),
            state("https://a/", "t", json!({"text": true})),
        ],
    );
}

#[test]
fn dialogs_are_dismissed_and_reported_without_their_text() {
    let f = fx();
    dialog_during_wait(&f, "confirm");
    let (code, lines, _) = wait(&f, &["--text", "Welcome", "--follow"]);
    assert_eq!(code, 0);
    assert_eq!(f.b.st().dialog_policy, Some(false));
    assert_eq!(events(&lines), vec!["state", "dialog", "dialog-closed", "state"]);
    assert_eq!(lines[1], json!({"event": "dialog", "tab": TAB, "t": 0.0, "type": "confirm"}));
    assert_eq!(lines[2], json!({"event": "dialog-closed", "tab": TAB, "t": 0.0, "type": "confirm", "handled": "dismissed"}));
}

#[test]
fn dialog_accept() {
    let f = fx();
    dialog_during_wait(&f, "prompt");
    let (code, lines, _) = wait(&f, &["--text", "Welcome", "--follow", "--dialog", "accept"]);
    assert_eq!(code, 0);
    assert_eq!(f.b.st().dialog_policy, Some(true));
    assert_eq!(lines[2]["handled"], "accepted");
}

// ------------------------------------------------------------ hygiene

#[test]
fn lines_carry_only_known_fields() {
    let mut f = fx();
    f.sys_version_fail({
        let n = Rc::new(Cell::new(0));
        move |_| {
            n.set(n.get() + 1);
            (n.get() == 2).then(|| Error::new("timed out"))
        }
    });
    let b = f.b.clone();
    script(
        &f,
        vec![
            state("https://a/", "t", json!({"text": false})),
            Step::Err(Error::new("CDP closed the connection")),
            Step::Do(Box::new(move |_| b.st().dialogs.push(Dialog { kind: "alert".into(), accepted: false }))),
            state("https://a/", "t", json!({"text": true})),
        ],
    );
    let (_, lines, _) = wait(&f, &["--text", "x", "--follow"]);
    let allowed = ["event", "tab", "t", "url", "title", "matched", "error", "type", "handled", "result", "elapsed", "reason",
                   "stalled", "timeout", "detail"];
    for l in &lines {
        for k in l.as_object().unwrap().keys() {
            assert!(allowed.contains(&k.as_str()), "{k} in {l}");
        }
    }
    assert_eq!(events(&lines), vec!["state", "stall", "resumed", "dialog", "dialog-closed", "state"]);
}

// ------------------------------------------------------------ CHECK_FN

/// CHECK_FN in node against a minimal DOM: regex syntax, every condition,
/// change detection, throttled mutation wakeups, the tick, and errors.
/// Skipped (with a note) when node isn't installed.
#[test]
fn check_fn_semantics_in_node() {
    let Some(node) = which("node") else {
        eprintln!("check_fn_semantics_in_node: node not found; skipped");
        return;
    };
    let plain = serde_json::to_string(&crate::wait::check_fn(None)).unwrap();
    let with_js = serde_json::to_string(&crate::wait::check_fn(Some("document.title.startsWith('Ho')"))).unwrap();
    let broken_js = serde_json::to_string(&crate::wait::check_fn(Some("document.nope.x"))).unwrap();
    let harness = format!(
        r#"
const assert = require('node:assert/strict');
// Minimal DOM: location, document (title, body text, elements), events,
// MutationObserver.
let observers = [];
const listeners = {{}};
globalThis.location = {{ href: 'https://bank.example/login?next=%2Fhome' }};
const els = new Set(['#status']);
globalThis.document = {{
  title: 'Log in', readyState: 'complete',
  body: {{ innerText: 'Waiting for your response' }},
  documentElement: {{ textContent: '' }},
  querySelector: (s) => (els.has(s) ? {{}} : null),
  addEventListener() {{}}, removeEventListener() {{}},
}};
globalThis.addEventListener = (t, f) => {{ (listeners[t] ||= []).push(f); }};
globalThis.removeEventListener = (t, f) => {{ listeners[t] = (listeners[t] || []).filter((g) => g !== f); }};
globalThis.MutationObserver = class {{
  constructor(cb) {{ this.cb = cb; }}
  observe() {{ observers.push(this); }}
  disconnect() {{ observers = observers.filter((o) => o !== this); }}
}};
const mutate = () => observers.forEach((o) => o.cb([]));
const fn = (src) => (0, eval)('(' + src + ')');
const CHECK = fn({plain}), WITH_JS = fn({with_js}), BROKEN_JS = fn({broken_js});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {{
  // Every condition, plain and /re/flags regexes.
  const S = {{ url: '/LOGIN/i', url_not: 'dashboard', title: '^Log', text: 'waiting', text_not: '/approved/i',
              selector: '#status', selector_not: '#otp', js: null }};
  let r = await CHECK(S, '', 1000);
  assert.deepEqual(r.state.matched, {{ url: true, url_not: true, title: true, text: false, text_not: true,
                                       selector: true, selector_not: true }});
  assert.equal(r.state.url, location.href);
  assert.equal(r.key, JSON.stringify(r.state));
  assert.equal(Object.keys(r.state).join(), 'url,title,matched', 'no page text in the state');
  // A g/y flag must not make .test() stateful across checks.
  const G = {{ title: '/Log/g' }};
  assert.equal((await CHECK(G, '', 1000)).state.matched.title, true);
  assert.equal((await CHECK(G, '', 1000)).state.matched.title, true);

  // Same state as last time: waits for a change (throttled mutation).
  const T = {{ text_not: '/waiting for your response/i' }};
  const first = await CHECK(T, '', 1000);
  assert.equal(first.state.matched.text_not, false);
  let t0 = Date.now();
  const p = CHECK(T, first.key, 5000);
  await sleep(20);
  mutate(); mutate(); mutate();  // a burst: one check
  document.body.innerText = 'You are signed in';
  r = await p;
  assert.equal(r.state.matched.text_not, true);
  const dt = Date.now() - t0;
  assert.ok(dt >= 100 && dt < 1000, 'woken by the mutation after the 100 ms throttle, took ' + dt);
  assert.equal(observers.length, 0, 'observer disconnected');

  // A mutation that changes nothing does not wake it; the tick does.
  t0 = Date.now();
  const same = await CHECK(T, r.key, 300);
  assert.equal(same.key, r.key);
  assert.ok(Date.now() - t0 >= 290, 'returned on the tick');

  // hashchange/popstate wake it too (SPA navigations).
  const u = await CHECK({{ url: '#done' }}, '', 1000);
  const p2 = CHECK({{ url: '#done' }}, u.key, 5000);
  location.href += '#done';
  listeners.popstate.forEach((f) => f());
  assert.equal((await p2).state.matched.url, true);
  assert.deepEqual(listeners.popstate, []);

  // --js is compiled in (no eval in the page).
  document.title = 'Home';
  assert.equal((await WITH_JS({{}}, '', 1000)).state.matched.js, true);

  // Invalid regex and throwing conditions reject (never hang).
  await assert.rejects(CHECK({{ text: '(' }}, '', 1000), /Invalid regular expression/);
  await assert.rejects(BROKEN_JS({{}}, '', 1000), TypeError);
  // ... also when the throw happens on a later check.
  globalThis.thrower = false;
  const odd = fn({with_js}.replace("document.title.startsWith('Ho')", "(() => {{ if (globalThis.thrower) throw new Error('late'); return false; }})()"));
  const k = (await odd({{}}, '', 1000)).key;
  const p3 = odd({{}}, k, 5000);
  globalThis.thrower = true; mutate();
  await assert.rejects(p3, /late/);
  console.log('ok');
}})().catch((e) => {{ console.error(e); process.exit(1); }});
"#
    );
    let out = std::process::Command::new(node).arg("-e").arg(&harness).output().unwrap();
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "ok",
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

// ------------------------------------------------------------ deadline

/// A browser that is frozen for CDP calls: every call (and connect) takes
/// its whole timeout, then times out. The fake clock moves by that much.
fn freeze_cdp(f: &Fx) {
    let clock = f.sys.clock.clone();
    let b = f.b.clone();
    f.b.set_hook(move |m, _| {
        let t = b.st().timeouts.last().and_then(|(_, t)| *t).unwrap_or(30.0);
        clock.set(clock.get() + t);
        Hook::Return(Err(Error::new(format!("timed out waiting for {m}"))))
    });
}

#[test]
fn unscripted_checks_fail_loudly_and_end_on_the_deadline() {
    let f = fx();
    let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", "5"]);
    assert_eq!(code, 75);
    assert_eq!(lines[0]["stalled"], "fake: unscripted Runtime.callFunctionOn");
}

#[test]
fn a_frozen_browser_cannot_push_the_wait_past_its_timeout() {
    for timeout in ["0.3", "5", "20"] {
        let f = fx();
        freeze_cdp(&f);
        let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", timeout, "--follow"]);
        let t: f64 = timeout.parse().unwrap();
        // Stalled at the deadline: no definitive answer (75), not "timeout".
        assert_eq!(code, 75, "{timeout}: {lines:?}");
        let fin = lines.last().unwrap();
        assert_eq!(fin["result"], "stalled", "{fin}");
        let elapsed = fin["elapsed"].as_f64().unwrap();
        assert!(elapsed >= t && elapsed <= t + crate::wait::CLEANUP_GRACE, "--timeout {timeout}: ended after {elapsed} s");
        assert!(fin["stalled"].as_str().unwrap().contains("timed out waiting for"), "{fin}");
        // No step was given more time than was left (+ the cleanup grace).
        for (m, step) in f.b.st().timeouts.iter() {
            assert!(step.unwrap() <= t + crate::wait::CLEANUP_GRACE + 1e-9, "--timeout {timeout}: {m} got {step:?}");
        }
    }
}

#[test]
fn a_frozen_world_step_is_bounded_too() {
    // Everything answers except Page.createIsolatedWorld (15 s each before).
    let f = fx();
    let clock = f.sys.clock.clone();
    let b = f.b.clone();
    f.b.set_hook(move |m, _| {
        if m != "Page.createIsolatedWorld" {
            return Hook::Pass;
        }
        let t = b.st().timeouts.last().and_then(|(_, t)| *t).unwrap();
        clock.set(clock.get() + t);
        Hook::Return(Err(Error::new("timed out waiting for Page.createIsolatedWorld")))
    });
    let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", "5", "--follow"]);
    assert_eq!(code, 75, "stalled at the deadline");
    assert_eq!(events(&lines), vec!["stall"]);
    let elapsed = lines.last().unwrap()["elapsed"].as_f64().unwrap();
    assert!((5.0..=6.0).contains(&elapsed), "{elapsed}");
}

#[test]
fn lookups_get_no_more_time_than_is_left() {
    // /json/version never answers: every lookup is a stall.
    let mut f = fx();
    f.sys_version_fail(|_| Some(Error::new("timed out")));
    let (code, _, _) = wait(&f, &["--url", "x", "--timeout", "3"]);
    assert_eq!(code, 75);
    let probes = f.sys.version_timeouts.borrow().clone();
    assert!(!probes.is_empty());
    assert!(probes.iter().all(|t| *t <= 3.0), "{probes:?}");
    assert!((probes[0] - 3.0).abs() < 1e-9, "the first probe gets what is left: {probes:?}");
    // With more time left, a lookup step still gets the shared step timeout.
    let mut f = fx();
    f.sys_version_fail(|_| Some(Error::new("timed out")));
    wait(&f, &["--url", "x", "--timeout", "60"]);
    assert_eq!(f.sys.version_timeouts.borrow()[0], crate::tab::LOOKUP_STEP_TIMEOUT);
}

#[test]
fn settling_a_gone_tab_stops_at_the_deadline() {
    // The lookup confirms the tab gone; then /json/version stops answering
    // but the unit stays active (a browser stuck shutting down): settle
    // polls the unit until the deadline, then reports the tab gone.
    let mut f = fx();
    let n = Rc::new(Cell::new(0));
    let n2 = n.clone();
    // Lookups: 1 (found), 2 and 3 (the two misses); settle's probe is 4.
    f.sys_version_fail(move |_| {
        n2.set(n2.get() + 1);
        (n2.get() > 3).then(|| Error::os("<urlopen error [Errno 111] Connection refused>"))
    });
    close_mid_wait(&f);
    let (code, lines, _) = wait(&f, &["--text", "Welcome", "--timeout", "2"]);
    assert_eq!(code, 4, "{lines:?}");
    let elapsed = lines[0]["elapsed"].as_f64().unwrap();
    assert!((1.9..=2.0 + crate::wait::CLEANUP_GRACE).contains(&elapsed), "{elapsed}");
    assert_eq!(n.get(), 4, "one settle probe");
}

// ------------------------------------------------------------ refusals

#[test]
fn an_explicit_port_is_checked_once_like_other_tab_commands() {
    // tab::explicit_port: reserved or someone else's port is refused (1),
    // unregistered with nothing listening is a browser that went away (5).
    // Either way at once: no retries, no CDP.
    for (port, free, code) in [("9222", false, 1), ("9333", false, 1), ("9426", false, 1), ("70000", false, 1), ("9426", true, 5)] {
        let mut f = fx();
        f.sys.port_free = Some(free);
        f.sys.sleeps.borrow_mut().clear();
        // A long (fake-time) timeout, not 0: a regression then fails
        // instead of hanging the suite.
        let (got, lines, err) = wait(&f, &["--port", port, "--url", "x", "--timeout", "3600"]);
        assert_eq!(got, code, "--port {port} (free: {free}): {lines:?} {err}");
        if code == 1 {
            assert!(lines.is_empty());
            assert!(err.contains(&format!("port {port}")) && err.contains("refusing") || err.contains("reserved"), "{err}");
        } else {
            assert_eq!(lines.len(), 1, "a browser-down line");
            assert_eq!(lines[0]["result"], "browser-down");
            assert_eq!(lines[0]["reason"], "no agent-display browser on port 9426 (stopped?)");
        }
        assert!(f.sys.sleeps.borrow().is_empty(), "--port {port}: retried");
        assert!(f.calls().is_empty(), "--port {port}: no CDP");
        assert_eq!(*f.sys.version_calls.borrow(), 0, "--port {port}: no HTTP probe");
    }
    // With --gone, a browser that went away is what was asked.
    let mut f = fx();
    f.sys.port_free = Some(true);
    let (code, lines, _) = wait(&f, &["--port", "9426", "--gone"]);
    assert_eq!((code, &lines[0]["result"], &lines[0]["reason"]), (0, &json!("matched"), &json!("browser-down")));
}

#[test]
fn a_registered_port_whose_browser_stops_is_browser_down_not_a_refusal() {
    // Valid --port; the browser stops mid-wait: its unit goes inactive and
    // the registry drops it, so later lookups say "not an agent-display
    // browser". That is the browser going down (5), not a refusal (1).
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/", "t", json!({"url": false})),
            // The browser exits: unregistered, connections refused.
            Step::Do(Box::new(|b: &FakeBrowser| {
                b.st().down = true;
                b.st().targets.clear();
            })),
            Step::Err(Error::new("CDP closed the connection")),
        ],
    );
    let (code, lines, err) = wait(&f, &["--port", "9425", "--url", "x", "--timeout", "30"]);
    assert_eq!(code, 5, "{lines:?} {err}");
    let fin = lines.last().unwrap();
    assert_eq!((&fin["result"], &fin["reason"]), (&json!("browser-down"), &json!("agent-display-browser is not running")));
}

#[test]
fn a_target_that_is_not_a_page_exits_1() {
    let f = fx();
    f.sys.sleeps.borrow_mut().clear();
    let (code, out, err) = f.cli(&["wait", "FRAME00000000001", "--url", "x", "--timeout", "3600"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(err, "agent-display: FRAME00000000001 is a iframe, not a tab\n");
    assert!(f.sys.sleeps.borrow().is_empty());
}

#[test]
fn beforeunload_during_a_wait_is_accepted_by_default() {
    let f = fx();
    dialog_during_wait(&f, "beforeunload");
    let (code, lines, _) = wait(&f, &["--text", "Welcome", "--follow"]);
    assert_eq!(code, 0);
    assert_eq!(f.b.st().dialog_policy, Some(false), "the policy stays dismiss");
    assert_eq!(lines[2], json!({"event": "dialog-closed", "tab": TAB, "t": 0.0, "type": "beforeunload", "handled": "accepted"}));
}

// ------------------------------------------------------------ round ends

/// Method names of the first round: the calls before the second
/// `Target.attachToTarget` (the reconnect), barrier marked as "barrier".
fn first_round(f: &Fx) -> Vec<String> {
    let calls = f.calls();
    let attaches: Vec<usize> = calls.iter().enumerate().filter(|(_, c)| c.0 == "Target.attachToTarget").map(|(i, _)| i).collect();
    assert!(attaches.len() >= 2, "expected a reconnect: {:?}", f.methods());
    let mut round: Vec<String> = calls[attaches[0]..attaches[1]]
        .iter()
        .map(|c| if c.0 == "Runtime.evaluate" && c.1["expression"] == "0" { "barrier".to_string() } else { c.0.clone() })
        .collect();
    // The next round's lookup (by id, on the browser endpoint) isn't part of it.
    while round.last().is_some_and(|m| m == "Target.getTargetInfo" || m == "Target.getTargets") {
        round.pop();
    }
    round
}

/// A stall that leaves our session attached ends with the dialog barrier
/// and a detach before the reconnect, for every step that can fail so.
#[test]
fn stalls_with_a_live_session_run_the_barrier_before_reconnecting() {
    let cases: &[(&str, u32)] = &[
        ("Page.enable", 1),
        ("Inspector.enable", 1),
        ("Page.getFrameTree", 1),
        ("Page.createIsolatedWorld", 51), // more than WORLD_RETRIES
        ("Runtime.callFunctionOn", 1),    // neither timeout, detach nor context-gone
    ];
    for (method, times) in cases {
        let f = fx();
        let left = Rc::new(Cell::new(*times));
        let l = left.clone();
        let b = f.b.clone();
        let injected = Rc::new(Cell::new(false));
        let method = *method;
        f.b.set_hook(move |m, p| {
            if m == method && l.get() > 0 {
                l.set(l.get() - 1);
                return Hook::Return(Err(Error::new(format!("{m}: Internal error"))));
            }
            if m == "Runtime.evaluate" && p["expression"] == "0" {
                // A dialog opened just before the round ended: the barrier's
                // round trip is what gets it answered.
                if !injected.get() {
                    injected.set(true);
                    let accepted = crate::cdp::answer_for("alert", b.st().dialog_policy.unwrap());
                    b.st().dialogs.push(Dialog { kind: "alert".into(), accepted });
                }
                return Hook::Pass;
            }
            if m == "Runtime.callFunctionOn" {
                let st = json!({"url": "https://a/home", "title": "Home", "matched": {"url": true}});
                return Hook::Return(Ok(json!({"result": {"type": "object", "value": {"key": st.to_string(), "state": st}}})));
            }
            Hook::Pass
        });
        let (code, lines, err) = wait(&f, &["--url", "home", "--follow", "--timeout", "60"]);
        assert_eq!(code, 0, "{method}: {lines:?} {err}");
        let round = first_round(&f);
        let n = round.len();
        assert_eq!(&round[n - 2..], ["barrier", "Target.detachFromTarget"], "{method}: {round:?}");
        // The dialog the barrier caught is reported as the round ends,
        // before the stall that sends the waiter to reconnect.
        let ev = events(&lines);
        assert_eq!(ev, ["dialog", "dialog-closed", "stall", "resumed", "state"], "{method}");
    }
}

#[test]
fn a_detached_session_is_not_detached_again() {
    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"url": false})),
            Step::Err(Error::new(format!("Runtime.callFunctionOn: {DETACHED}"))),
            state("https://a/home", "Home", json!({"url": true})),
        ],
    );
    let (code, _, _) = wait(&f, &["--url", "home"]);
    assert_eq!(code, 0);
    let round = first_round(&f);
    // The round ends at the failed check; the next one starts with the
    // shared lookup (by id), with no barrier or detach on the dead session.
    assert_eq!(round.last().map(String::as_str), Some("Runtime.callFunctionOn"), "{round:?}");
    assert!(!round.iter().any(|m| m == "barrier" || m == "Target.detachFromTarget"), "{round:?}");
}

#[test]
fn closed_crashed_and_browser_down_skip_the_barrier() {
    let barrier = |f: &Fx| f.calls().iter().any(|c| c.0 == "Runtime.evaluate" && c.1["expression"] == "0");
    let f = fx();
    close_mid_wait(&f);
    assert_eq!(wait(&f, &["--text", "Welcome"]).0, 4);
    assert!(!barrier(&f), "closed: {:?}", f.methods());

    let f = fx();
    script(
        &f,
        vec![
            state("https://a/login", "Log in", json!({"text": false})),
            Step::Do(Box::new(|b: &FakeBrowser| {
                b.st().events.push(json!({"method": "Inspector.targetCrashed", "params": {}, "sessionId": SESSION}));
            })),
            Step::Same,
        ],
    );
    assert_eq!(wait(&f, &["--text", "Welcome"]).0, 4);
    assert!(!barrier(&f), "crashed: {:?}", f.methods());

    let mut f = fx();
    browser_gone(&mut f);
    assert_eq!(wait(&f, &["--text", "Welcome"]).0, 5);
    assert!(!barrier(&f), "browser down: {:?}", f.methods());
}

// ------------------------------------------------------------ unit state

/// The browser stops answering mid-wait (the connection drops, every
/// lookup fails) while systemctl reports the unit as `state`.
fn browser_unreachable(f: &mut Fx) {
    let unreachable = Rc::new(Cell::new(false));
    let u = unreachable.clone();
    f.sys_version_fail(move |_| u.get().then(|| Error::os("<urlopen error [Errno 111] Connection refused>")));
    script(
        f,
        vec![
            state("https://a/", "t", json!({"url": false})),
            Step::Do(Box::new(move |_| unreachable.set(true))),
            Step::Err(Error::new("CDP closed the connection")),
        ],
    );
}

#[test]
fn an_unknown_unit_state_is_not_browser_down() {
    // systemctl can't answer (the same host stall): the browser may still
    // run, so this is a stall until the deadline (75), never 5.
    let mut f = fx();
    browser_unreachable(&mut f);
    *f.sys.states_unknown.borrow_mut() = true;
    let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", "30", "--follow"]);
    assert_eq!(code, 75, "{lines:?}");
    assert_eq!(events(&lines), vec!["state", "stall"]);
    assert_eq!(lines.last().unwrap()["result"], "stalled");
}

#[test]
fn a_unit_confirmed_stopped_is_browser_down() {
    let mut f = fx();
    browser_unreachable(&mut f);
    f.sys.active = Some(false); // systemd answers: inactive
    let (code, lines, _) = wait(&f, &["--url", "x", "--timeout", "30"]);
    assert_eq!(code, 5, "{lines:?}");
    assert_eq!(lines[0]["reason"], "agent-display-browser is not running");
}

#[test]
fn a_unit_mid_transition_is_not_browser_down_yet() {
    // `deactivating` (or `activating`): units_active says unknown.
    let mut f = fx();
    browser_unreachable(&mut f);
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-browser".into());
    let (code, _, _) = wait(&f, &["--url", "x", "--timeout", "10"]);
    assert_eq!(code, 75);
}
