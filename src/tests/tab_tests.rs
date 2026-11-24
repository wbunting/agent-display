//! `tab` against a stubbed CDP. Like the Python (TabOutputTest subclasses
//! TabTest), every TabTest case runs twice: without a display (plain
//! background window) and with a fake sway (own output per tab).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Value, json};

use super::fakes::*;
use crate::ctx::*;
use crate::util::Error;

type UnitPred = Rc<RefCell<Box<dyn Fn(&str) -> bool>>>;

pub struct Fx {
    pub t: TempHome,
    pub b: FakeBrowser,
    pub sway: Option<FakeSway>,
    pub sys: FakeSys,
    pub ready: UnitPred,
}

impl Fx {
    pub fn new(with_sway: bool) -> Fx {
        let t = TempHome::new();
        let b = FakeBrowser::new();
        let ready: UnitPred = Rc::new(RefCell::new(Box::new(|_| true)));
        let r = ready.clone();
        let sway = with_sway.then(FakeSway::new);
        b.st().sway = sway.clone();
        let sys = FakeSys {
            browser: Some(b.clone()),
            listener: Some(Box::new(move |_, unit| (r.borrow())(unit))),
            display: Some(if with_sway {
                Ok(Disp { runtime: String::new(), wayland: String::new(), swaysock: "x".into(), bus: String::new() })
            } else {
                Err(Error::new("no display"))
            }),
            sway: sway.clone(),
            active: Some(true),
            ..Default::default()
        };
        *sys.browsers.borrow_mut() = Some(vec![info("agent-display-browser", 9425)]);
        Fx { t, b, sway, sys, ready }
    }

    pub fn cli(&self, argv: &[&str]) -> (i32, String, String) {
        let mut full = vec!["tab"];
        full.extend_from_slice(argv);
        self.t.cli(&self.sys, &full)
    }

    pub fn open(&self, argv: &[&str]) -> Value {
        let mut a = vec!["open"];
        a.extend_from_slice(argv);
        let (code, out, err) = self.cli(&a);
        assert_eq!(code, 0, "{err}");
        serde_json::from_str(&out).unwrap()
    }

    pub fn methods(&self) -> Vec<String> {
        self.b.methods()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.b.st().calls.clone()
    }

    pub fn markers(&self) -> BTreeMap<String, String> {
        let dir = self.t.rt.join("agent-display/tab-outputs");
        let mut out = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                out.insert(e.file_name().to_string_lossy().into(), std::fs::read_to_string(e.path()).unwrap());
            }
        }
        out
    }

    pub fn sys_version_fail(&mut self, f: impl Fn(u16) -> Option<Error> + 'static) {
        self.sys.version_fail = Some(Box::new(f));
    }

    pub fn sw(&self) -> &FakeSway {
        self.sway.as_ref().unwrap()
    }
}

fn pos(v: &[String], m: &str) -> usize {
    v.iter().position(|x| x == m).unwrap_or_else(|| panic!("{m} not in {v:?}"))
}

macro_rules! both {
    ($name:ident, $body:expr) => {
        mod $name {
            use super::*;
            #[test]
            fn plain() {
                ($body)(Fx::new(false))
            }
            #[test]
            fn with_sway() {
                ($body)(Fx::new(true))
            }
        }
    };
}

macro_rules! sway_only {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            ($body)(Fx::new(true))
        }
    };
}

both!(happy_path, |f: Fx| {
    let opened = f.open(&[]);
    let tab = opened["tab"].as_str().unwrap().to_string();
    assert_eq!(opened["port"], 9425);
    let create = f.calls().into_iter().find(|c| c.0 == "Target.createTarget").unwrap();
    // Own window, never activated.
    assert_eq!(create.1["background"], true);
    assert_eq!(create.1["newWindow"], true);

    let (code, out, _) = f.cli(&["goto", &tab, "https://example.com/"]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["url"], "https://example.com/");
    assert!(f.methods().contains(&"wait:Page.loadEventFired".to_string()));

    let (code, out, _) = f.cli(&["eval", &tab, "document.title"]);
    assert_eq!((code, serde_json::from_str::<Value>(&out).unwrap()), (0, json!("Example")));
    let ev = f.calls().into_iter().rfind(|c| c.0 == "Runtime.evaluate" && c.1["expression"] == "document.title").unwrap();
    assert!(ev.1["returnByValue"] == true && ev.1["awaitPromise"] == true);

    let shot = f.t.root.join("shot.png");
    let (code, out, _) = f.cli(&["shot", &tab, shot.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), shot.to_str().unwrap());
    assert_eq!(std::fs::read(&shot).unwrap(), b"\x89PNG fake");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&shot).unwrap().permissions().mode() & 0o777, 0o600);

    let (_, out, _) = f.cli(&["info", &tab]);
    let info: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(info["url"], "https://example.com/");
    assert_eq!(out.trim_end(), format!(r#"{{"tab": "{tab}", "url": "https://example.com/", "title": ""}}"#));

    let (code, _, _) = f.cli(&["close", &tab]);
    assert_eq!(code, 0);
    assert!(!f.b.has(&tab));
    for m in ["Target.activateTarget", "Page.bringToFront"] {
        assert!(!f.methods().contains(&m.to_string()));
    }
    // Every page command went to our tab's session only.
    let sessions: std::collections::HashSet<String> = f.calls().into_iter().filter_map(|c| c.2).collect();
    assert_eq!(sessions, std::collections::HashSet::from([format!("S-{tab}")]));
});

both!(goto_wait_ms_skips_load_event, |f: Fx| {
    let (code, _, _) = f.cli(&["goto", "OTHERTAB00000001", "https://b/", "--wait-ms", "250"]);
    assert_eq!(code, 0);
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.25]);
    assert!(!f.methods().contains(&"wait:Page.loadEventFired".to_string()));
});

both!(full_page_shot_resets_emulation, |f: Fx| {
    let shot = f.t.root.join("full.png");
    let (code, _, _) = f.cli(&["shot", "OTHERTAB00000001", shot.to_str().unwrap(), "--full-page"]);
    assert_eq!(code, 0);
    let m = f.methods();
    assert!(pos(&m, "Emulation.setDeviceMetricsOverride") < pos(&m, "Page.captureScreenshot"));
    assert!(pos(&m, "Page.captureScreenshot") < pos(&m, "Emulation.clearDeviceMetricsOverride"));
});

both!(shot_retries_hung_capture_with_resize, |f: Fx| {
    let mut hung = false;
    f.b.set_hook(move |m, _| {
        if m == "Page.captureScreenshot" && !hung {
            hung = true;
            return Hook::Return(Err(Error::new("timed out waiting for Page.captureScreenshot")));
        }
        Hook::Pass
    });
    let shot = f.t.root.join("shot.png");
    let (code, _, _) = f.cli(&["shot", "OTHERTAB00000001", shot.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(std::fs::read(&shot).unwrap(), b"\x89PNG fake");
    let heights: Vec<i64> = f
        .calls()
        .into_iter()
        .filter(|c| c.0 == "Emulation.setDeviceMetricsOverride")
        .map(|c| c.1["height"].as_i64().unwrap())
        .collect();
    assert_eq!(heights, vec![599, 600, 599, 600]);
    let m = f.methods();
    assert_eq!(m[m.len() - 2], "Emulation.clearDeviceMetricsOverride");
});

both!(shot_first_attempt_is_short_and_scales_for_full_page, |f: Fx| {
    let d = &f.t.root;
    f.cli(&["shot", "OTHERTAB00000001", d.join("a.png").to_str().unwrap()]);
    f.cli(&["shot", "OTHERTAB00000001", d.join("b.png").to_str().unwrap(), "--full-page"]);
    let waits: Vec<f64> =
        f.b.st().timeouts.iter().filter(|(m, _)| m == "Page.captureScreenshot").map(|(_, t)| t.unwrap()).collect();
    assert!((waits[0] - 0.5).abs() < 1e-9);
    assert!((waits[1] - 0.5 * 5000.0 / 600.0).abs() < 1e-9);
});

both!(shot_failure_leaves_no_file, |f: Fx| {
    f.b.set_hook(|m, _| {
        if m == "Page.captureScreenshot" {
            return Hook::Return(Err(Error::new("Page.captureScreenshot: target crashed")));
        }
        Hook::Pass
    });
    let shot = f.t.root.join("shot.png");
    let (code, out, _) = f.cli(&["shot", "OTHERTAB00000001", shot.to_str().unwrap()]);
    assert_eq!((code, out.as_str()), (1, ""));
    let m = f.methods();
    assert_eq!(m[m.len() - 2], "Emulation.clearDeviceMetricsOverride");
    let left: Vec<_> = std::fs::read_dir(&f.t.root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("shot")).collect();
    assert!(left.is_empty());
});

both!(shot_cleanup_failure_does_not_mask_error, |f: Fx| {
    f.b.set_hook(|m, _| match m {
        "Page.captureScreenshot" => Hook::Return(Err(Error::new("Page.captureScreenshot: target crashed"))),
        "Emulation.clearDeviceMetricsOverride" => Hook::Return(Err(Error::new("CDP closed the connection"))),
        _ => Hook::Pass,
    });
    let (code, _, err) = f.cli(&["shot", "OTHERTAB00000001", f.t.root.join("s.png").to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("target crashed"), "{err}");
});

both!(eval_exception_is_nonzero_with_text, |f: Fx| {
    f.b.st().eval_result = json!({"result": {"type": "object"}, "exceptionDetails": {
        "text": "Uncaught", "exception": {"description": "TypeError: x is not a function"}}});
    let (code, out, err) = f.cli(&["eval", "OTHERTAB00000001", "x()"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("TypeError: x is not a function"), "{err}");
});

both!(refuses_foreign_and_reserved_ports, |mut f: Fx| {
    f.sys.port_free = Some(false); // something else listens on 9500
    for port in ["9333", "9222", "9500"] {
        f.b.st().calls.clear();
        let (code, _, err) = f.cli(&["open", "--port", port]);
        assert_eq!(code, 1);
        assert!(err.contains("not an agent-display browser"), "{err}");
        assert!(f.calls().is_empty());
    }
});

both!(refuses_listener_outside_unit, |f: Fx| {
    *f.ready.borrow_mut() = Box::new(|_| false);
    let (code, _, err) = f.cli(&["eval", "--port", "9425", "OTHERTAB00000001", "1"]);
    assert_eq!(code, 1);
    assert!(err.contains("refusing"), "{err}");
    assert!(f.calls().is_empty());
    assert!(f.sys.sleeps.borrow().is_empty(), "refusals are not retried");
});

both!(unowned_listener_during_lookup_is_retried_then_tempfail, |f: Fx| {
    // Without --port, a registered browser whose listener can't be proven
    // (restarting, or not its own) gives no answer about the tab: retry,
    // then 75. Never "closed", never a CDP call to that port.
    *f.ready.borrow_mut() = Box::new(|_| false);
    let (code, _, err) = f.cli(&["eval", "OTHERTAB00000001", "1"]);
    assert_eq!(code, 75, "{err}");
    assert!(err.contains("not served from agent-display-browser"), "{err}");
    assert!(!err.contains("closed"), "{err}");
    assert!(f.calls().is_empty());
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.5, 1.0, 2.0, 4.0]);
});

both!(unregistered_free_port_is_browser_down, |mut f: Fx| {
    f.sys.port_free = Some(true); // nothing listens there any more
    for argv in [&["open", "--port", "9500"][..], &["info", "--port", "9500", "OTHERTAB00000001"]] {
        let (code, _, err) = f.cli(argv);
        assert_eq!(code, 5, "{argv:?}: {err}");
        assert!(err.contains("no agent-display browser on port 9500"), "{err}");
    }
    assert!(f.calls().is_empty());
});

both!(no_browser_running_is_browser_down, |f: Fx| {
    *f.sys.browsers.borrow_mut() = Some(vec![]);
    for argv in [&["info", "OTHERTAB00000001"][..], &["list"], &["open"], &["close", "OTHERTAB00000001"]] {
        let (code, _, err) = f.cli(argv);
        assert_eq!(code, 5, "{argv:?}: {err}");
        assert!(err.contains("no agent-display browser is running"), "{err}");
    }
});

both!(reserved_port_never_trusted_even_if_registered, |f: Fx| {
    *f.sys.browsers.borrow_mut() = Some(vec![info("agent-display-browser", 9333)]);
    let (code, _, _) = f.cli(&["list"]);
    assert_eq!(code, 5, "a reserved port is never a usable browser");
    assert!(f.calls().is_empty());
});

both!(close_only_affects_given_tab, |f: Fx| {
    let mine = f.open(&[])["tab"].as_str().unwrap().to_string();
    let other_agent = f.open(&[])["tab"].as_str().unwrap().to_string();
    let (code, _, _) = f.cli(&["close", &mine]);
    assert_eq!(code, 0);
    let mut want = vec!["FRAME00000000001".to_string(), "OTHERTAB00000001".to_string(), other_agent];
    want.sort();
    assert_eq!(f.b.ids(), want);
    let closes: Vec<Call> = f.calls().into_iter().filter(|c| c.0 == "Target.closeTarget").collect();
    assert_eq!(closes, vec![("Target.closeTarget".to_string(), json!({"targetId": mine}), None)]);
});

both!(unknown_or_non_page_targets_refused, |f: Fx| {
    for (tab, code_want, why) in
        [("NOSUCHTAB0000001", 4, "no tab"), ("FRAME00000000001", 1, "not a tab"), ("../../etc", 1, "invalid")]
    {
        let (code, _, err) = f.cli(&["close", tab]);
        assert_eq!(code, code_want, "{tab}: {err}");
        assert!(err.contains(why), "{tab}: {err}");
    }
    assert!(!f.methods().contains(&"Target.closeTarget".to_string()));
});

both!(open_url_closes_tab_when_navigation_fails, |f: Fx| {
    f.b.set_hook(|m, _| {
        if m == "Page.navigate" {
            return Hook::Return(Ok(json!({"frameId": "F", "errorText": "net::ERR_NAME_NOT_RESOLVED"})));
        }
        Hook::Pass
    });
    let before = f.b.ids();
    let (code, out, err) = f.cli(&["open", "https://nx.invalid/"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("ERR_NAME_NOT_RESOLVED"), "{err}");
    assert_eq!(f.b.ids(), before);
});

both!(lookup_skips_unhealthy_other_browser, |f: Fx| {
    *f.sys.browsers.borrow_mut() =
        Some(vec![info("agent-display-browser-tmp-a", 9001), info("agent-display-browser", 9425)]);
    *f.ready.borrow_mut() = Box::new(|unit| !unit.contains("tmp"));
    let (code, out, _) = f.cli(&["info", "OTHERTAB00000001"]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["url"], "https://a/");
    // The tab may be in the browser that didn't answer: not "gone".
    let (code, _, err) = f.cli(&["info", "NOSUCHTAB0000001"]);
    assert_eq!(code, 75, "{err}");
    assert!(err.contains("try again: 9001 (CDP on port 9001 is not served from"), "{err}");
    assert!(!err.contains("closed"), "{err}");
});

both!(list_shows_pages_only, |f: Fx| {
    let (_, out, _) = f.cli(&["list"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    let ids: Vec<&str> = v["tabs"].as_array().unwrap().iter().map(|t| t["tab"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["OTHERTAB00000001"]);
    assert!(out.starts_with("{\n  \"port\": 9425,\n  \"tabs\": [\n    {\n      \"tab\": \"OTHERTAB00000001\","));
});

// ------------------------------------------------------------ outputs

sway_only!(open_puts_window_alone_on_new_output_and_keeps_focus, |f: Fx| {
    let out = f.open(&[]);
    let tab = out["tab"].as_str().unwrap();
    assert_eq!(out["output"], "HEADLESS-2");
    assert_eq!(f.sw().window_of(tab).output, "HEADLESS-2");
    assert_eq!(f.sw().st().focus, Some(5));
    assert_eq!(f.markers(), BTreeMap::from([("HEADLESS-2".to_string(), tab.to_string())]));
    assert_eq!(f.sw().st().wins[&5].output, "HEADLESS-1");
});

sway_only!(each_tab_gets_its_own_output, |f: Fx| {
    let (a, b) = (f.open(&[]), f.open(&[]));
    assert_eq!((a["output"].as_str(), b["output"].as_str()), (Some("HEADLESS-2"), Some("HEADLESS-3")));
    assert_eq!(f.sw().st().focus, Some(5));
});

sway_only!(close_unplugs_only_its_own_output, |f: Fx| {
    let (a, b) = (f.open(&[]), f.open(&[]));
    let (code, out, _) = f.cli(&["close", a["tab"].as_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["output"], "HEADLESS-2");
    assert_eq!(f.sw().st().outputs, vec!["HEADLESS-1", "HEADLESS-3"]);
    let btab = b["tab"].as_str().unwrap();
    assert_eq!(f.markers(), BTreeMap::from([("HEADLESS-3".to_string(), btab.to_string())]));
    assert_eq!(f.sw().window_of(btab).output, "HEADLESS-3");
});

sway_only!(falls_back_to_plain_window_without_output, |f: Fx| {
    f.sw().st().fail_create = true;
    let out = f.open(&[]);
    assert!(out["output"].is_null());
    assert_eq!(f.sw().window_of(out["tab"].as_str().unwrap()).output, "HEADLESS-1");
    assert_eq!(f.sw().st().focus, Some(5));
    assert!(f.markers().is_empty());
});

sway_only!(open_url_failure_removes_tab_and_output, |f: Fx| {
    f.b.set_hook(|m, _| {
        if m == "Page.navigate" {
            return Hook::Return(Ok(json!({"frameId": "F", "errorText": "net::ERR_NAME_NOT_RESOLVED"})));
        }
        Hook::Pass
    });
    let (code, _, _) = f.cli(&["open", "https://nx.invalid/"]);
    assert_eq!(code, 1);
    assert_eq!(f.sw().st().outputs, vec!["HEADLESS-1"]);
    assert!(f.markers().is_empty());
});

sway_only!(create_timeout_closes_late_tab_and_removes_output, |f: Fx| {
    f.b.set_hook(|m, _| {
        if m == "Target.createTarget" {
            // Chromium creates it late.
            return Hook::DefaultThen(Error::new("timed out waiting for Target.createTarget"));
        }
        Hook::Pass
    });
    let (code, out, err) = f.cli(&["open"]);
    assert_eq!((code, out.as_str()), (75, ""), "a stalled createTarget: try again");
    assert!(err.contains("timed out"), "{err}");
    assert_eq!(f.sw().st().outputs, vec!["HEADLESS-1"]);
    assert!(f.markers().is_empty());
    assert_eq!(f.b.ids(), vec!["FRAME00000000001", "OTHERTAB00000001"]);
});

sway_only!(window_never_found_reports_no_output, |f: Fx| {
    f.sw().st().hide_new = true;
    *f.sys.hide_window.borrow_mut() = true;
    let out = f.open(&[]);
    assert!(out["output"].is_null());
    f.sw().st().hide_new = false;
    *f.sys.hide_window.borrow_mut() = false;
    f.open(&[]); // next open's gc removes the empty marked output
    assert!(!f.sw().st().outputs.contains(&"HEADLESS-2".to_string()));
});

sway_only!(close_succeeds_when_output_cleanup_fails, |f: Fx| {
    let a = f.open(&[]);
    let tab = a["tab"].as_str().unwrap();
    f.sw().st().fail_tree = true;
    let (code, out, err) = f.cli(&["close", tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!({"closed": tab, "port": 9425, "output": null}));
    assert_eq!(out.trim_end(), format!(r#"{{"closed": "{tab}", "port": 9425, "output": null}}"#));
    assert!(!f.b.has(tab));
});

sway_only!(focus_returns_to_main_output_when_nothing_was_focused, |f: Fx| {
    f.sw().st().wins.remove(&5);
    f.sw().st().focus = None;
    let out = f.open(&[]);
    assert_eq!(out["output"], "HEADLESS-2");
    assert_eq!(f.sw().st().focused_output.as_deref(), Some("HEADLESS-1"));
});

sway_only!(tab_output_matches_main_output_size, |f: Fx| {
    let out = f.open(&[]);
    assert_eq!(f.sw().st().sizes[out["output"].as_str().unwrap()], (1280, 800));
});

sway_only!(failed_resize_falls_back_and_removes_output, |f: Fx| {
    f.sw().st().fail_mode = true;
    let out = f.open(&[]);
    assert!(out["output"].is_null());
    assert_eq!(f.sw().st().outputs, vec!["HEADLESS-1"]);
    assert!(f.markers().is_empty());
    assert_eq!(f.sw().window_of(out["tab"].as_str().unwrap()).output, "HEADLESS-1");
});

sway_only!(gc_removes_leaked_output_but_nothing_unmarked, |f: Fx| {
    let a = f.open(&[]);
    // Its agent died: the tab went with the browser window, `tab close`
    // never ran.
    let atab = a["tab"].as_str().unwrap().to_string();
    f.sw().closed(&atab);
    f.b.st().targets.retain(|(k, _)| *k != atab);
    {
        let mut st = f.sw().st();
        st.outputs.push("HEADLESS-9".into()); // someone else's empty output
        st.sizes.insert("HEADLESS-9".into(), (1920, 1080));
    }
    let d = f.t.rt.join("agent-display/tab-outputs");
    std::fs::write(d.join("HEADLESS-1"), "x").unwrap(); // never the main output
    std::fs::write(d.join("bad; exit"), "x").unwrap(); // never a crafted name
    f.open(&[]);
    let st = f.sw().st();
    assert!(!st.outputs.contains(&"HEADLESS-2".to_string()));
    assert!(st.outputs.contains(&"HEADLESS-9".to_string()));
    assert!(st.outputs.contains(&"HEADLESS-1".to_string()));
    let unplugs: Vec<&String> = st.commands.iter().filter(|c| c.ends_with("unplug")).collect();
    assert_eq!(unplugs, vec!["output HEADLESS-2 unplug"]);
});

// ------------------------------------------------------------ fix 1

fn close_calls(f: &Fx) -> usize {
    f.methods().iter().filter(|m| *m == "Target.closeTarget").count()
}

fn timed_out_close() -> Error {
    Error::new("timed out waiting for Target.closeTarget")
}

both!(ownership_check_waits_15s_for_a_stalled_browser, |f: Fx| {
    let (code, _, _) = f.cli(&["list"]);
    assert_eq!(code, 0);
    assert_eq!(*f.sys.version_timeouts.borrow(), vec![15.0]);
    assert_eq!(*f.sys.targets_timeouts.borrow(), vec![15.0]);
});

both!(close_retries_after_a_timeout, |f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    let mut first = true;
    f.b.set_hook(move |m, _| {
        if m == "Target.closeTarget" && first {
            first = false;
            return Hook::Return(Err(timed_out_close()));
        }
        Hook::Pass
    });
    f.sys.sleeps.borrow_mut().clear();
    let (code, out, err) = f.cli(&["close", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["closed"], tab.as_str());
    assert!(!f.b.has(&tab));
    assert_eq!(close_calls(&f), 2);
    assert_eq!(f.sys.sleeps.borrow().iter().filter(|s| **s >= 1.0).count(), 1);
});

both!(close_that_went_through_late_counts_as_closed, |f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    // Chromium closed it but the reply never came in time.
    f.b.set_hook(|m, _| if m == "Target.closeTarget" { Hook::DefaultThen(timed_out_close()) } else { Hook::Pass });
    let (code, out, err) = f.cli(&["close", &tab]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["port"], 9425);
    assert!(!f.b.has(&tab));
    assert_eq!(close_calls(&f), 1);
});

both!(close_gives_up_after_three_attempts, |f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.b.set_hook(|m, _| if m == "Target.closeTarget" { Hook::Return(Err(timed_out_close())) } else { Hook::Pass });
    f.sys.sleeps.borrow_mut().clear();
    let (code, _, err) = f.cli(&["close", &tab]);
    assert_eq!(code, 75, "a close that keeps stalling: try again later");
    assert!(err.contains("timed out waiting for Target.closeTarget"), "{err}");
    assert_eq!(close_calls(&f), 3);
    let backoff: Vec<f64> = f.sys.sleeps.borrow().iter().copied().filter(|s| *s >= 1.0).collect();
    assert_eq!(backoff, vec![1.0, 2.0]);
});

#[test]
fn close_fails_fast_when_the_browser_unit_is_not_active() {
    let mut f = Fx::new(false);
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.b.set_hook(|m, _| if m == "Target.closeTarget" { Hook::Return(Err(timed_out_close())) } else { Hook::Pass });
    f.sys.active = Some(false); // browsers() is stubbed; only the retry asks systemd
    let (code, _, _) = f.cli(&["close", &tab]);
    assert_eq!(code, 5, "its browser is down");
    assert_eq!(close_calls(&f), 1);
    assert!(f.sys.sleeps.borrow().is_empty());
}

both!(close_never_retries_refusals, |f: Fx| {
    *f.ready.borrow_mut() = Box::new(|_| false);
    let (code, _, err) = f.cli(&["close", "--port", "9425", "OTHERTAB00000001"]);
    assert_eq!(code, 1);
    assert!(err.contains("not served from"), "{err}");
    assert!(f.sys.sleeps.borrow().is_empty());
    assert!(f.calls().is_empty());
});

both!(close_retries_when_lookup_skipped_a_stalled_browser, |mut f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    let stalls = Rc::new(RefCell::new(1));
    let s2 = stalls.clone();
    f.sys_version_fail(move |_| {
        let mut n = s2.borrow_mut();
        if *n > 0 {
            *n -= 1;
            return Some(Error::os("timed out"));
        }
        None
    });
    let (code, _, err) = f.cli(&["close", &tab]);
    assert_eq!(code, 0, "{err}");
    assert!(!f.b.has(&tab));
    assert_eq!(*stalls.borrow(), 0);
});

// ------------------------------------------------------------ lookup by id

fn count(f: &Fx, m: &str) -> usize {
    f.methods().iter().filter(|x| *x == m).count()
}

fn no_target() -> Hook {
    Hook::Return(Err(Error::new("Target.getTargetInfo: No target with given id found")))
}

both!(lookup_asks_by_id_never_scans_json_list, |f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.sys.targets_timeouts.borrow_mut().clear();
    let shot = f.t.root.join("s.png");
    for argv in [
        vec!["info", tab.as_str()],
        vec!["eval", tab.as_str(), "1"],
        vec!["goto", tab.as_str(), "https://x/"],
        vec!["shot", tab.as_str(), shot.to_str().unwrap()],
    ] {
        let (code, _, err) = f.cli(&argv);
        assert_eq!(code, 0, "{argv:?}: {err}");
    }
    assert!(f.sys.targets_timeouts.borrow().is_empty(), "no /json/list");
    assert_eq!(count(&f, "Target.getTargetInfo"), 4);
    let infos: Vec<Value> = f.calls().into_iter().filter(|c| c.0 == "Target.getTargetInfo").map(|c| c.1).collect();
    assert!(infos.iter().all(|p| *p == json!({"targetId": tab})));
});

both!(lookup_connection_keeps_the_callers_timeout, |f: Fx| {
    // The lookup connects with a short step timeout; page commands on that
    // connection must still get --timeout (here: the default call timeout).
    let (code, _, _) = f.cli(&["eval", "--timeout", "42", "OTHERTAB00000001", "1"]);
    assert_eq!(code, 0);
    let eff = f.b.st().effective.clone();
    let of = |m: &str| eff.iter().find(|(x, _)| x == m).unwrap().1;
    assert_eq!(of("Target.getTargetInfo"), 5.0);
    assert_eq!(of("Target.attachToTarget"), 42.0);
    assert_eq!(of("Runtime.evaluate"), 42.0);
});

both!(tab_missing_from_one_pass_is_found_on_recheck, |f: Fx| {
    // The flaky case from the field: a live tab that a browser briefly
    // doesn't report. Two misses in one pass (before and after
    // Target.getTargets), then found: exit 0, not "gone".
    let mut misses = 2;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" && misses > 0 {
            misses -= 1;
            return no_target();
        }
        Hook::Pass
    });
    let (code, out, err) = f.cli(&["info", "OTHERTAB00000001"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["url"], "https://a/");
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.25]);
    assert_eq!(count(&f, "Target.getTargets"), 1, "hosts are created before a miss counts");
});

both!(miss_then_host_creation_finds_the_tab_in_one_pass, |f: Fx| {
    let mut misses = 1;
    f.b.set_hook(move |m, _| {
        if m == "Target.getTargetInfo" && misses > 0 {
            misses -= 1;
            return no_target();
        }
        Hook::Pass
    });
    let (code, _, err) = f.cli(&["info", "OTHERTAB00000001"]);
    assert_eq!(code, 0, "{err}");
    assert!(f.sys.sleeps.borrow().is_empty());
});

both!(gone_takes_two_definitive_misses_and_exits_4, |f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.b.st().targets.retain(|(k, _)| *k != tab); // crashed / closed elsewhere
    f.b.st().calls.clear();
    f.sys.sleeps.borrow_mut().clear();
    for cmd in ["info", "eval", "close"] {
        let mut argv = vec![cmd, tab.as_str()];
        if cmd == "eval" {
            argv.push("1");
        }
        let (code, out, err) = f.cli(&argv);
        assert_eq!((code, out.as_str()), (4, ""), "{cmd}: {err}");
        assert!(err.contains(&format!("no tab {tab} in an agent-display browser: it was closed or crashed (checked twice)")), "{err}");
        assert!(!err.contains("closed?"), "{err}");
    }
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.25; 3]);
    assert_eq!(count(&f, "Target.getTargetInfo"), 3 * 4, "2 passes x (ask, getTargets, ask)");
    assert!(!f.methods().contains(&"Target.closeTarget".to_string()));
});

fn stall_n(f: &mut Fx, n: usize) -> Rc<RefCell<usize>> {
    let left = Rc::new(RefCell::new(n));
    let l2 = left.clone();
    f.sys_version_fail(move |_| {
        let mut k = l2.borrow_mut();
        if *k > 0 {
            *k -= 1;
            return Some(Error::os("timed out"));
        }
        None
    });
    left
}

both!(stalled_browser_is_tempfail_never_gone, |mut f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    stall_n(&mut f, usize::MAX);
    f.sys.sleeps.borrow_mut().clear();
    let shot = f.t.root.join("s.png");
    for argv in [
        vec!["info", tab.as_str()],
        vec!["eval", tab.as_str(), "1"],
        vec!["shot", tab.as_str(), shot.to_str().unwrap()],
        vec!["close", tab.as_str()],
    ] {
        let (code, out, err) = f.cli(&argv);
        assert_eq!((code, out.as_str()), (75, ""), "{argv:?}: {err}");
        assert!(err.contains(&format!("tab {tab}: no definitive answer after 5 attempts")), "{err}");
        assert!(err.contains("try again: 9425 (agent-display-browser is not answering CDP on 9425: CDP: timed out)"), "{err}");
        assert!(!err.contains("closed"), "{err}");
    }
    assert_eq!(f.sys.sleeps.borrow()[..4], [0.5, 1.0, 2.0, 4.0]);
    assert!(f.b.has(&tab));
});

both!(stall_that_clears_within_the_budget_is_ridden_out, |mut f: Fx| {
    let left = stall_n(&mut f, 2);
    let (code, out, err) = f.cli(&["info", "OTHERTAB00000001"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["tab"], "OTHERTAB00000001");
    assert_eq!(*left.borrow(), 0);
    assert_eq!(*f.sys.sleeps.borrow(), vec![0.5, 1.0]);
});

// ------------------------------------------------------------ ownership cache

both!(ownership_is_remembered_and_rechecked_from_proc, |mut f: Fx| {
    std::fs::create_dir_all(f.t.rt.join("agent-display/browsers")).unwrap();
    let owner_file = f.t.rt.join("agent-display/browsers/agent-display-browser.owner");
    // First command: systemd + cgroup scan, remembered.
    assert_eq!(f.cli(&["info", "OTHERTAB00000001"]).0, 0);
    assert_eq!(*f.sys.listener_scans.borrow(), 1);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&owner_file).unwrap()).unwrap();
    assert_eq!(saved, json!({"port": 9425, "pid": 4242, "start_time": 1, "inode": 7}));
    // /proc still confirms that process and socket: no scan.
    f.sys.owner_ok = Some(Box::new(|o| o.pid == 4242 && o.inode == 7));
    for _ in 0..3 {
        assert_eq!(f.cli(&["info", "OTHERTAB00000001"]).0, 0);
    }
    assert_eq!(*f.sys.listener_scans.borrow(), 1, "cache hits");
    assert!(*f.sys.owner_checks.borrow() >= 3);
    // Mismatch (browser restarted: new pid/socket): back to the full scan,
    // which re-proves and re-remembers ownership.
    f.sys.owner_ok = Some(Box::new(|_| false));
    assert_eq!(f.cli(&["info", "OTHERTAB00000001"]).0, 0);
    assert_eq!(*f.sys.listener_scans.borrow(), 2);
    // ... or refuses, and forgets the stale owner.
    *f.ready.borrow_mut() = Box::new(|_| false);
    let (code, _, err) = f.cli(&["info", "--port", "9425", "OTHERTAB00000001"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not served from"), "{err}");
    assert!(!owner_file.exists());
});

// ------------------------------------------------------------ gc guard

fn win_id(f: &Fx, tab: &str) -> i64 {
    *f.sw().st().wins.iter().find(|(_, w)| w.tab.as_deref() == Some(tab)).unwrap().0
}

fn unplugs(f: &Fx) -> Vec<String> {
    f.sw().st().commands.iter().filter(|c| c.ends_with(" unplug")).cloned().collect()
}

sway_only!(gc_keeps_the_output_of_a_live_tab_whose_window_is_away, |f: Fx| {
    let a = f.open(&[]);
    let atab = a["tab"].as_str().unwrap().to_string();
    assert_eq!(a["output"], "HEADLESS-2");
    // Its window is briefly off its output (remapped: fullscreen, dialog).
    let id = win_id(&f, &atab);
    f.sw().st().wins.get_mut(&id).unwrap().output = "HEADLESS-1".into();
    f.open(&[]); // another agent's open runs gc
    assert!(f.sw().st().outputs.contains(&"HEADLESS-2".to_string()));
    assert_eq!(f.markers().get("HEADLESS-2"), Some(&atab));
    assert!(unplugs(&f).is_empty(), "{:?}", unplugs(&f));
    assert!(count(&f, "Target.getTargets") >= 1, "asked the browser before unplugging");
    // Once the tab really is gone, the next gc removes its output.
    f.b.st().targets.retain(|(k, _)| *k != atab);
    f.sw().closed(&atab);
    f.open(&[]);
    assert_eq!(unplugs(&f), vec!["output HEADLESS-2 unplug"]);
    assert!(!f.markers().contains_key("HEADLESS-2"));
});

#[test]
fn gc_keeps_outputs_while_the_browser_does_not_answer() {
    let mut f = Fx::new(true);
    let a = f.open(&[]);
    let atab = a["tab"].as_str().unwrap().to_string();
    f.sw().closed(&atab); // window gone, but we can't ask about the tab
    stall_n(&mut f, usize::MAX);
    let _lock = crate::tab::OutputsLock::acquire(&f.t.ctx(&f.sys)).unwrap();
    let ctx = f.t.ctx(&f.sys);
    let disp = f.sys.display(&ctx).unwrap();
    assert!(crate::tab::gc_tab_outputs(&ctx, &disp).unwrap().is_empty());
    assert!(f.sw().st().outputs.contains(&"HEADLESS-2".to_string()));
}

sway_only!(gc_asks_each_browser_at_most_twice_for_all_empty_outputs, |f: Fx| {
    // Three agents' tabs, all with windows briefly away from their outputs,
    // one of them really gone: one getTargets per pass for all of them, one
    // 250 ms pause, only the gone tab's output unplugged.
    let tabs: Vec<(String, String)> = (0..3)
        .map(|_| {
            let o = f.open(&[]);
            (o["tab"].as_str().unwrap().to_string(), o["output"].as_str().unwrap().to_string())
        })
        .collect();
    for (t, _) in &tabs {
        let id = win_id(&f, t);
        f.sw().st().wins.get_mut(&id).unwrap().output = "HEADLESS-1".into();
    }
    f.b.st().targets.retain(|(k, _)| *k != tabs[0].0);
    f.b.st().calls.clear();
    f.sys.sleeps.borrow_mut().clear();
    f.open(&[]);
    assert_eq!(count(&f, "Target.getTargets"), 2);
    assert_eq!(count(&f, "Target.getTargetInfo"), 0);
    assert_eq!(f.sys.sleeps.borrow().iter().filter(|s| **s == 0.25).count(), 1);
    assert_eq!(unplugs(&f), vec![format!("output {} unplug", tabs[0].1)]);
    // All alive: one pass, no pause.
    f.b.st().calls.clear();
    f.sys.sleeps.borrow_mut().clear();
    f.open(&[]);
    assert_eq!(count(&f, "Target.getTargets"), 1);
    assert!(!f.sys.sleeps.borrow().contains(&0.25));
    // No empty outputs: the browser isn't asked at all.
    for (t, o) in &tabs[1..] {
        let id = win_id(&f, t);
        f.sw().st().wins.get_mut(&id).unwrap().output = o.clone();
    }
    f.b.st().calls.clear();
    f.open(&[]);
    assert_eq!(count(&f, "Target.getTargets"), 0);
});

sway_only!(close_unplugs_its_output_without_asking_again, |f: Fx| {
    let a = f.open(&[]);
    let tab = a["tab"].as_str().unwrap().to_string();
    f.b.st().calls.clear();
    let (code, out, _) = f.cli(&["close", &tab]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["output"], "HEADLESS-2");
    // One lookup before the close; none for the gc of its own output.
    assert_eq!(count(&f, "Target.getTargetInfo"), 1);
    assert!(f.sys.sleeps.borrow().iter().all(|s| *s != 0.25));
});

// ------------------------------------------------------------ --port stalls
// With an explicit --port a stall is still "try again" (75), never a
// refusal (1); ownership refusals stay 1 and are never retried.

both!(stall_with_explicit_port_is_tempfail_for_lookups, |mut f: Fx| {
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    stall_n(&mut f, usize::MAX);
    f.sys.sleeps.borrow_mut().clear();
    for argv in [
        vec!["info", "--port", "9425", tab.as_str()],
        vec!["eval", "--port", "9425", tab.as_str(), "1"],
        vec!["close", "--port", "9425", tab.as_str()],
    ] {
        let (code, out, err) = f.cli(&argv);
        assert_eq!((code, out.as_str()), (75, ""), "{argv:?}: {err}");
        assert!(err.contains("no definitive answer"), "{err}");
        assert!(!err.contains("refusing") && !err.contains("closed"), "{err}");
    }
    // Retried within the budget, like without --port.
    assert_eq!(f.sys.sleeps.borrow()[..4], [0.5, 1.0, 2.0, 4.0]);
    assert!(f.b.has(&tab));
});

both!(stall_with_explicit_port_that_clears_is_ridden_out, |mut f: Fx| {
    stall_n(&mut f, 2);
    let (code, out, err) = f.cli(&["info", "--port", "9425", "OTHERTAB00000001"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["tab"], "OTHERTAB00000001");
});

both!(stalled_browser_on_open_and_list_is_tempfail, |mut f: Fx| {
    stall_n(&mut f, usize::MAX);
    for argv in [&["open", "--port", "9425"][..], &["open"], &["list", "--port", "9425"], &["list"]] {
        let (code, out, err) = f.cli(argv);
        assert_eq!((code, out.as_str()), (75, ""), "{argv:?}: {err}");
        assert!(err.contains("agent-display-browser is not answering CDP on 9425: CDP: timed out; try again"), "{err}");
    }
    assert!(!f.methods().contains(&"Target.createTarget".to_string()));
});

both!(explicit_port_refusal_is_still_1_and_not_retried, |f: Fx| {
    *f.ready.borrow_mut() = Box::new(|_| false);
    for argv in [&["info", "--port", "9425", "OTHERTAB00000001"][..], &["open", "--port", "9425"]] {
        let (code, _, err) = f.cli(argv);
        assert_eq!(code, 1, "{argv:?}: {err}");
        assert!(err.contains("not served from agent-display-browser; refusing"), "{err}");
    }
    assert!(f.sys.sleeps.borrow().is_empty());
    assert!(f.calls().is_empty());
});

// ------------------------------------------------------------ closeTarget "no such target"

fn close_says_no_target() -> Hook {
    Hook::Return(Err(Error::new("Target.closeTarget: No target with given id found")))
}

both!(close_whose_earlier_attempt_went_through_late_is_closed, |f: Fx| {
    // Attempt 1 times out while Chromium still tears the tab down; attempt
    // 2's lookup still finds it, and its closeTarget says "no such target":
    // our first close did it. Exit 0, output removed.
    let opened = f.open(&[]);
    let tab = opened["tab"].as_str().unwrap().to_string();
    let mut n = 0;
    let sway = f.sway.clone();
    let t2 = tab.clone();
    f.b.set_hook(move |m, _| {
        if m == "Target.closeTarget" {
            n += 1;
            if n == 1 {
                return Hook::Return(Err(timed_out_close()));
            }
            // By now the first close has finished: the window is gone.
            if let Some(s) = &sway {
                s.closed(&t2);
            }
            return close_says_no_target();
        }
        Hook::Pass
    });
    let (code, out, err) = f.cli(&["close", &tab]);
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["closed"], tab.as_str());
    assert_eq!(close_calls(&f), 2);
    if f.sway.is_some() {
        assert_eq!(v["output"], opened["output"]);
        assert!(f.markers().is_empty());
    }
});

both!(close_of_a_tab_closed_by_someone_else_meanwhile_is_gone, |f: Fx| {
    // Found by the lookup, closed by another caller before our closeTarget.
    let opened = f.open(&[]);
    let tab = opened["tab"].as_str().unwrap().to_string();
    let sway = f.sway.clone();
    let t2 = tab.clone();
    f.b.set_hook(move |m, _| {
        if m == "Target.closeTarget" {
            if let Some(s) = &sway {
                s.closed(&t2); // its window went with it
            }
            return close_says_no_target();
        }
        Hook::Pass
    });
    let (code, out, err) = f.cli(&["close", &tab]);
    assert_eq!((code, out.as_str()), (4, ""), "{err}");
    assert!(err.contains("it was closed just before this close"), "{err}");
    assert_eq!(close_calls(&f), 1);
    assert!(f.sys.sleeps.borrow().iter().all(|s| *s < 1.0), "not retried");
    if f.sway.is_some() {
        assert!(f.markers().is_empty(), "its output was removed");
        assert!(!f.sw().st().outputs.contains(&opened["output"].as_str().unwrap().to_string()));
    }
});

// ------------------------------------------------------------ unknown browser state on close

#[test]
fn close_stall_with_the_browser_state_unknown_retries_then_75() {
    // closeTarget keeps timing out and systemctl can't read the browser
    // unit's state (the same host stall): retry, then 75 — never 5.
    let f = Fx::new(false);
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.b.set_hook(|m, _| if m == "Target.closeTarget" { Hook::Return(Err(timed_out_close())) } else { Hook::Pass });
    *f.sys.unknown_prefix.borrow_mut() = Some("agent-display-browser".into());
    f.sys.sleeps.borrow_mut().clear();
    let (code, _, err) = f.cli(&["close", &tab]);
    assert_eq!(code, 75, "{err}");
    assert_eq!(close_calls(&f), 3);
    let backoff: Vec<f64> = f.sys.sleeps.borrow().iter().copied().filter(|s| *s >= 1.0).collect();
    assert_eq!(backoff, vec![1.0, 2.0]);
    assert!(f.b.has(&tab));
}

#[test]
fn close_stall_with_the_browser_confirmed_stopped_is_still_5() {
    let mut f = Fx::new(false);
    let tab = f.open(&[])["tab"].as_str().unwrap().to_string();
    f.b.set_hook(|m, _| if m == "Target.closeTarget" { Hook::Return(Err(timed_out_close())) } else { Hook::Pass });
    f.sys.active = Some(false); // systemd answers: inactive
    let (code, _, _) = f.cli(&["close", &tab]);
    assert_eq!(code, 5);
    assert_eq!(close_calls(&f), 1);
}
