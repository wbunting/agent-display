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

both!(refuses_foreign_and_reserved_ports, |f: Fx| {
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
    let (code, _, err) = f.cli(&["eval", "OTHERTAB00000001", "1"]);
    assert_eq!(code, 1);
    assert!(err.contains("refusing"), "{err}");
    assert!(f.calls().is_empty());
});

both!(reserved_port_never_trusted_even_if_registered, |f: Fx| {
    *f.sys.browsers.borrow_mut() = Some(vec![info("agent-display-browser", 9333)]);
    let (code, _, _) = f.cli(&["list"]);
    assert_eq!(code, 1);
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
    for (tab, why) in [("NOSUCHTAB0000001", "no tab"), ("FRAME00000000001", "not a tab"), ("../../etc", "invalid")] {
        let (code, _, err) = f.cli(&["close", tab]);
        assert_eq!(code, 1);
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
    let (code, _, err) = f.cli(&["info", "NOSUCHTAB0000001"]);
    assert_eq!(code, 1);
    assert!(err.contains("skipped unhealthy browsers: 9001"), "{err}");
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
    assert_eq!((code, out.as_str()), (1, ""));
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
    f.sw().closed(a["tab"].as_str().unwrap()); // window gone, tab never closed
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
    assert_eq!(code, 1);
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
    assert_eq!(code, 1);
    assert_eq!(close_calls(&f), 1);
    assert!(f.sys.sleeps.borrow().is_empty());
}

both!(close_never_retries_refusals, |f: Fx| {
    *f.ready.borrow_mut() = Box::new(|_| false);
    let (code, _, err) = f.cli(&["close", "OTHERTAB00000001"]);
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
