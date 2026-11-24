//! Command line, parsed the way the Python's argparse parser does it, so
//! flags, positionals, abbreviations, `--`, usage errors (exit 2) and `-h`
//! behave and print the same. Usage/help texts live in `cli_text.rs`
//! (argparse's format; generated from the Python parser at the port, edited
//! by hand since: a test checks each parser here has a matching entry).

use std::collections::{HashMap, HashSet};

use crate::cli_text::TEXTS;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ty {
    Str,
    Int,
    Float,
    Size,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Nargs {
    Zero,
    One,
    Two,
    Opt,
    Star,
    Remainder,
    Parser,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Store,
    StoreTrue,
    Append,
    Help,
    Sub,
}

struct Arg {
    strings: Vec<&'static str>, // option strings; empty for positionals
    dest: &'static str,
    nargs: Nargs,
    ty: Ty,
    choices: Vec<String>,
    act: Act,
    group: Option<u8>,
    default: Val,
}

impl Arg {
    fn name(&self) -> String {
        if self.strings.is_empty() { self.dest.to_string() } else { self.strings.join("/") }
    }
    fn is_option(&self) -> bool {
        !self.strings.is_empty()
    }
    fn required(&self) -> bool {
        !self.is_option() && matches!(self.nargs, Nargs::One | Nargs::Two | Nargs::Parser)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    None,
    Bool(bool),
    Str(String),
    Int(i64),
    Float(f64),
    List(Vec<Val>),
}

struct Parser {
    prog: String,
    args: Vec<Arg>,
    subs: Vec<(&'static str, Parser)>,
}

/// What argparse would do instead of returning a namespace.
#[derive(Debug, PartialEq)]
pub struct Exit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

type Ns = HashMap<&'static str, Val>;

fn texts(prog: &str) -> (&'static str, &'static str) {
    TEXTS
        .iter()
        .find(|(p, _, _)| *p == prog)
        .map(|(_, u, h)| (*u, *h))
        .unwrap_or(("", ""))
}

impl Parser {
    fn new(prog: &str) -> Self {
        let mut p = Parser { prog: prog.to_string(), args: Vec::new(), subs: Vec::new() };
        p.args.push(Arg {
            strings: vec!["-h", "--help"],
            dest: "help",
            nargs: Nargs::Zero,
            ty: Ty::Str,
            choices: vec![],
            act: Act::Help,
            group: None,
            default: Val::None,
        });
        p
    }

    fn opt(&mut self, s: &'static str, dest: &'static str, ty: Ty, default: Val) -> &mut Arg {
        self.args.push(Arg {
            strings: vec![s],
            dest,
            nargs: Nargs::One,
            ty,
            choices: vec![],
            act: Act::Store,
            group: None,
            default,
        });
        self.args.last_mut().unwrap()
    }

    fn flag(&mut self, s: &'static str, dest: &'static str) -> &mut Arg {
        self.args.push(Arg {
            strings: vec![s],
            dest,
            nargs: Nargs::Zero,
            ty: Ty::Str,
            choices: vec![],
            act: Act::StoreTrue,
            group: None,
            default: Val::Bool(false),
        });
        self.args.last_mut().unwrap()
    }

    fn pos(&mut self, dest: &'static str, nargs: Nargs, ty: Ty, default: Val) -> &mut Arg {
        self.args.push(Arg {
            strings: vec![],
            dest,
            nargs,
            ty,
            choices: vec![],
            act: Act::Store,
            group: None,
            default,
        });
        self.args.last_mut().unwrap()
    }

    fn sub(&mut self, dest: &'static str, subs: Vec<(&'static str, Parser)>) {
        let choices = subs.iter().map(|(n, _)| n.to_string()).collect();
        self.args.push(Arg {
            strings: vec![],
            dest,
            nargs: Nargs::Parser,
            ty: Ty::Str,
            choices,
            act: Act::Sub,
            group: None,
            default: Val::None,
        });
        self.subs = subs;
    }

    fn child(&self, name: &str) -> Parser {
        Parser::new(&format!("{} {}", self.prog, name))
    }
}

fn choices(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn build() -> Parser {
    let mut p = Parser::new("agent-display");
    let mut subs = Vec::new();
    let c = |n: &str| p.child(n);
    subs.push(("check", c("check")));
    let mut s = c("start");
    s.opt("--size", "size", Ty::Size, Val::Str("1920x1080".into()));
    s.opt("--renderer", "renderer", Ty::Str, Val::None).choices = choices(&["gles2", "pixman", "vulkan"]);
    subs.push(("start", s));
    subs.push(("stop", c("stop")));
    subs.push(("status", c("status")));
    subs.push(("env", c("env")));
    let mut e = c("exec");
    e.pos("argv", Nargs::Remainder, Ty::Str, Val::None);
    subs.push(("exec", e));
    let mut la = c("launch");
    la.pos("name", Nargs::One, Ty::Str, Val::None);
    la.pos("argv", Nargs::Remainder, Ty::Str, Val::None);
    subs.push(("launch", la));
    subs.push(("windows", c("windows")));
    let mut f = c("focus");
    f.pos("id", Nargs::One, Ty::Int, Val::None);
    subs.push(("focus", f));
    let mut sc = c("screenshot");
    sc.pos("path", Nargs::One, Ty::Str, Val::None);
    sc.opt("--window", "window", Ty::Int, Val::None);
    subs.push(("screenshot", sc));
    let mut t = c("type");
    t.pos("text", Nargs::One, Ty::Str, Val::None);
    subs.push(("type", t));
    let mut k = c("key");
    k.pos("key", Nargs::One, Ty::Str, Val::None);
    let m = k.opt("--mod", "mod", Ty::Str, Val::List(vec![]));
    m.act = Act::Append;
    m.choices = choices(&["shift", "ctrl", "alt", "logo", "altgr"]);
    subs.push(("key", k));
    let mut mv = c("move");
    mv.pos("x", Nargs::One, Ty::Int, Val::None);
    mv.pos("y", Nargs::One, Ty::Int, Val::None);
    subs.push(("move", mv));
    let mut cl = c("click");
    cl.pos("button", Nargs::Opt, Ty::Str, Val::Str("left".into())).choices = choices(&["left", "middle", "right"]);
    cl.opt("--at", "at", Ty::Int, Val::None).nargs = Nargs::Two;
    cl.opt("--count", "count", Ty::Int, Val::Int(1)).choices = choices(&["1", "2", "3"]);
    subs.push(("click", cl));
    let mut w = c("scroll");
    w.pos("direction", Nargs::One, Ty::Str, Val::None).choices = choices(&["up", "down", "left", "right"]);
    w.pos("steps", Nargs::Opt, Ty::Int, Val::Int(3));
    subs.push(("scroll", w));

    let mut b = c("browser");
    let mut bst = b.child("start");
    bst.flag("--temp", "temp");
    bst.opt("--port", "port", Ty::Int, Val::None);
    bst.flag("--audio", "audio");
    bst.pos("url", Nargs::Star, Ty::Str, Val::None);
    let mut bsp = b.child("stop");
    bsp.pos("unit", Nargs::Opt, Ty::Str, Val::None);
    bsp.flag("--all", "all");
    let bl = b.child("list");
    b.sub("browser_command", vec![("start", bst), ("stop", bsp), ("list", bl)]);
    subs.push(("browser", b));

    let mut tb = c("tab");
    let tab_cmd = |tb: &Parser, name: &str, tab: bool| {
        let mut sp = tb.child(name);
        sp.opt("--port", "port", Ty::Int, Val::None);
        sp.opt("--timeout", "timeout", Ty::Float, Val::Float(30.0));
        if tab {
            sp.pos("tab", Nargs::One, Ty::Str, Val::None);
        }
        sp
    };
    let mut to = tab_cmd(&tb, "open", false);
    to.pos("url", Nargs::Opt, Ty::Str, Val::None);
    let mut tg = tab_cmd(&tb, "goto", true);
    tg.pos("url", Nargs::One, Ty::Str, Val::None);
    tg.flag("--wait-load", "wait_load").group = Some(1);
    tg.opt("--wait-ms", "wait_ms", Ty::Int, Val::None).group = Some(1);
    let mut te = tab_cmd(&tb, "eval", true);
    te.pos("js", Nargs::One, Ty::Str, Val::None);
    let mut tsh = tab_cmd(&tb, "shot", true);
    tsh.pos("path", Nargs::One, Ty::Str, Val::None);
    tsh.flag("--full-page", "full_page");
    let ti = tab_cmd(&tb, "info", true);
    let tc = tab_cmd(&tb, "close", true);
    let tl = tab_cmd(&tb, "list", false);
    // show: --timeout is how long the view may stay up, not a CDP step.
    let mut tsw = tb.child("show");
    tsw.opt("--port", "port", Ty::Int, Val::None);
    tsw.opt("--timeout", "show_timeout", Ty::Float, Val::Float(900.0));
    tsw.flag("--view-only", "view_only");
    tsw.opt("--until-url", "until_url", Ty::Str, Val::None);
    tsw.flag("--until-closed", "until_closed");
    tsw.flag("--wait-viewer", "wait_viewer");
    tsw.pos("tab", Nargs::One, Ty::Str, Val::None);
    let mut tus = tb.child("unshow");
    tus.pos("tab", Nargs::One, Ty::Str, Val::None);
    // wait: --timeout bounds the whole wait (see wait.rs).
    let mut tw = tab_cmd(&tb, "wait", true);
    for (s, d) in [
        ("--url", "url"), ("--url-not", "url_not"), ("--title", "title"), ("--text", "text"), ("--text-not", "text_not"),
        ("--selector", "selector"), ("--selector-not", "selector_not"), ("--js", "js"),
    ] {
        tw.opt(s, d, Ty::Str, Val::None);
    }
    tw.flag("--gone", "gone");
    tw.flag("--any", "any");
    tw.flag("--follow", "follow");
    tw.opt("--stable-ms", "stable_ms", Ty::Int, Val::Int(0));
    tw.opt("--tick-ms", "tick_ms", Ty::Int, Val::Int(1000));
    tw.opt("--dialog", "dialog", Ty::Str, Val::Str("dismiss".into())).choices = choices(&["accept", "dismiss"]);
    tb.sub(
        "tab_command",
        vec![
            ("open", to),
            ("goto", tg),
            ("eval", te),
            ("shot", tsh),
            ("info", ti),
            ("close", tc),
            ("list", tl),
            ("show", tsw),
            ("unshow", tus),
            ("wait", tw),
        ],
    );
    subs.push(("tab", tb));

    let mut v = c("view");
    v.opt("--port", "port", Ty::Int, Val::Int(5910));
    v.flag("--view-only", "view_only");
    v.flag("--exit-on-disconnect", "exit_on_disconnect");
    subs.push(("view", v));
    subs.push(("unview", c("unview")));
    subs.push(("hold-input", c("hold-input")));
    p.sub("command", subs);
    p
}

// ------------------------------------------------------------ matching

/// One greedy quantified character class of an argparse nargs pattern.
#[derive(Clone, Copy)]
struct Item {
    set: &'static str,
    min: usize,
    max: usize,
    group: usize,
}

const INF: usize = usize::MAX;

fn positional_items(nargs: Nargs, group: usize) -> Vec<Item> {
    let it = |set, min, max| Item { set, min, max, group };
    match nargs {
        Nargs::One => vec![it("-", 0, INF), it("A", 1, 1), it("-", 0, INF)],
        Nargs::Two => vec![it("-", 0, INF), it("A", 1, 1), it("-", 0, INF), it("A", 1, 1), it("-", 0, INF)],
        Nargs::Opt => vec![it("-", 0, INF), it("A", 0, 1), it("-", 0, INF)],
        Nargs::Star => vec![it("-", 0, INF), it("A-", 0, INF)],
        Nargs::Remainder => vec![it("-AO", 0, INF)],
        Nargs::Parser => vec![it("-", 0, INF), it("A", 1, 1), it("-AO", 0, INF)],
        Nargs::Zero => vec![],
    }
}

/// Python `re.match` of the concatenated groups against `s`: the group
/// lengths of the first match in greedy backtracking order.
fn re_match(items: &[Item], groups: usize, s: &[u8]) -> Option<Vec<usize>> {
    fn go(items: &[Item], i: usize, pos: usize, s: &[u8], lens: &mut Vec<usize>) -> bool {
        if i == items.len() {
            return true;
        }
        let it = items[i];
        let mut avail = 0;
        while pos + avail < s.len() && avail < it.max && it.set.as_bytes().contains(&s[pos + avail]) {
            avail += 1;
        }
        if avail < it.min {
            return false;
        }
        for n in (it.min..=avail).rev() {
            lens[it.group] += n;
            if go(items, i + 1, pos + n, s, lens) {
                return true;
            }
            lens[it.group] -= n;
        }
        false
    }
    let mut lens = vec![0; groups];
    go(items, 0, 0, s, &mut lens).then_some(lens)
}

fn match_partial(actions: &[&Arg], pattern: &[u8]) -> Vec<usize> {
    for i in (1..=actions.len()).rev() {
        let items: Vec<Item> = actions[..i]
            .iter()
            .enumerate()
            .flat_map(|(g, a)| positional_items(a.nargs, g))
            .collect();
        if let Some(mut result) = re_match(&items, i, pattern) {
            let end: usize = result.iter().sum();
            if end < pattern.len() && pattern[end] == b'O' {
                while result.last() == Some(&0) {
                    result.pop();
                }
            }
            return result;
        }
    }
    vec![]
}

/// Arguments an option takes from the pattern after it, or argparse's error.
fn match_option(a: &Arg, pattern: &[u8]) -> Result<usize, String> {
    let ok = |n: usize, set: &[u8]| pattern.len() >= n && pattern[..n].iter().all(|c| set.contains(c));
    match a.nargs {
        Nargs::Zero => Ok(0),
        Nargs::One if ok(1, b"A") => Ok(1),
        Nargs::One => Err("expected one argument".into()),
        Nargs::Two if ok(2, b"AO") => Ok(2),
        Nargs::Two => Err("expected 2 arguments".into()),
        _ => Ok(0),
    }
}

// ------------------------------------------------------------ parsing

struct Fail(Exit);

fn error(p: &Parser, msg: &str) -> Fail {
    let (usage, _) = texts(&p.prog);
    Fail(Exit { code: 2, stdout: String::new(), stderr: format!("{usage}{}: error: {msg}\n", p.prog) })
}

fn arg_error(p: &Parser, a: &Arg, msg: &str) -> Fail {
    error(p, &format!("argument {}: {msg}", a.name()))
}

fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let v: i64 = digits.replace('_', "").parse().ok()?;
    Some(if neg { -v } else { v })
}

fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let body = lower.trim_start_matches(['+', '-']);
    if matches!(body, "inf" | "infinity" | "nan") {
        return lower.parse().ok().or(Some(if body == "nan" { f64::NAN } else if lower.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }));
    }
    if t.contains("__") || t.starts_with('_') || t.ends_with('_') {
        return None;
    }
    let cleaned = t.replace('_', "");
    if cleaned.chars().any(|c| !(c.is_ascii_digit() || "+-.eE".contains(c))) {
        return None;
    }
    cleaned.parse().ok()
}

fn convert(p: &Parser, a: &Arg, s: &str) -> Result<Val, Fail> {
    let bad = |ty: &str| arg_error(p, a, &format!("invalid {ty} value: {}", crate::util::py_repr(s)));
    match a.ty {
        Ty::Str => Ok(Val::Str(s.to_string())),
        Ty::Int => py_int(s).map(Val::Int).ok_or_else(|| bad("int")),
        Ty::Float => py_float(s).map(Val::Float).ok_or_else(|| bad("float")),
        Ty::Size => {
            let ok = s.split_once('x').is_some_and(|(w, h)| {
                [w, h].iter().all(|d| (3..=5).contains(&d.len()) && d.chars().all(|c| c.is_ascii_digit()))
            });
            if ok {
                Ok(Val::Str(s.to_string()))
            } else {
                Err(arg_error(p, a, "expected WIDTHxHEIGHT, e.g. 1920x1080"))
            }
        }
    }
}

fn val_str(v: &Val) -> String {
    match v {
        Val::Str(s) => s.clone(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => crate::json::float_repr(*f),
        _ => String::new(),
    }
}

fn check_choice(p: &Parser, a: &Arg, v: &Val) -> Result<(), Fail> {
    if a.choices.is_empty() {
        return Ok(());
    }
    let s = val_str(v);
    if a.choices.contains(&s) {
        return Ok(());
    }
    let list = a.choices.iter().map(|c| crate::util::py_repr(c)).collect::<Vec<_>>().join(", ");
    Err(arg_error(p, a, &format!("invalid choice: {} (choose from {list})", crate::util::py_repr(&s))))
}

fn get_values(p: &Parser, a: &Arg, args: &[String]) -> Result<Val, Fail> {
    if a.act == Act::StoreTrue {
        return Ok(Val::Bool(true));
    }
    if args.is_empty() && a.nargs == Nargs::Opt {
        return Ok(match &a.default {
            Val::Str(s) => convert(p, a, s)?,
            other => other.clone(),
        });
    }
    if args.is_empty() && a.nargs == Nargs::Star && !a.is_option() {
        return Ok(Val::List(vec![]));
    }
    if args.len() == 1 && matches!(a.nargs, Nargs::One | Nargs::Opt) {
        let v = convert(p, a, &args[0])?;
        check_choice(p, a, &v)?;
        return Ok(v);
    }
    if a.nargs == Nargs::Remainder {
        return Ok(Val::List(args.iter().map(|s| Val::Str(s.clone())).collect()));
    }
    if a.nargs == Nargs::Parser {
        let first = Val::Str(args[0].clone());
        check_choice(p, a, &first)?;
        return Ok(Val::List(args.iter().map(|s| Val::Str(s.clone())).collect()));
    }
    let mut out = Vec::new();
    for s in args {
        let v = convert(p, a, s)?;
        check_choice(p, a, &v)?;
        out.push(v);
    }
    Ok(Val::List(out))
}

struct OptMatch {
    arg: Option<usize>,
    string: String,
    explicit: Option<String>,
}

fn parse_optional(p: &Parser, s: &str) -> Result<Option<Vec<OptMatch>>, Fail> {
    if s.is_empty() || !s.starts_with('-') {
        return Ok(None);
    }
    let find = |o: &str| p.args.iter().position(|a| a.strings.contains(&o));
    if let Some(i) = find(s) {
        return Ok(Some(vec![OptMatch { arg: Some(i), string: s.into(), explicit: None }]));
    }
    if s.len() == 1 {
        return Ok(None);
    }
    if let Some((o, v)) = s.split_once('=')
        && let Some(i) = find(o)
    {
        return Ok(Some(vec![OptMatch { arg: Some(i), string: o.into(), explicit: Some(v.into()) }]));
    }
    let mut tuples = Vec::new();
    let all: Vec<(usize, &str)> =
        p.args.iter().enumerate().flat_map(|(i, a)| a.strings.iter().map(move |s| (i, *s))).collect();
    if s.starts_with("--") {
        let (prefix, explicit) = match s.split_once('=') {
            Some((o, v)) => (o, Some(v.to_string())),
            None => (s, None),
        };
        for (i, o) in &all {
            if o.starts_with(prefix) {
                tuples.push(OptMatch { arg: Some(*i), string: o.to_string(), explicit: explicit.clone() });
            }
        }
    } else {
        let (prefix, explicit) = match s.split_once('=') {
            Some((o, v)) => (o, Some(v.to_string())),
            None => (s, None),
        };
        let short = &s[..s.char_indices().nth(2).map(|(i, _)| i).unwrap_or(s.len())];
        let short_rest = &s[short.len()..];
        for (i, o) in &all {
            if *o == short {
                tuples.push(OptMatch { arg: Some(*i), string: o.to_string(), explicit: Some(short_rest.to_string()) });
            } else if o.starts_with(prefix) {
                tuples.push(OptMatch { arg: Some(*i), string: o.to_string(), explicit: explicit.clone() });
            }
        }
    }
    if !tuples.is_empty() {
        return Ok(Some(tuples));
    }
    // Looks like a negative number (`-\.?\d`) or contains a space: positional.
    let b = s.as_bytes();
    let negative = b.len() > 1 && (b[1].is_ascii_digit() || (b[1] == b'.' && b.len() > 2 && b[2].is_ascii_digit()));
    if negative || s.contains(' ') {
        return Ok(None);
    }
    Ok(Some(vec![OptMatch { arg: None, string: s.into(), explicit: None }]))
}

struct State<'a> {
    p: &'a Parser,
    ns: Ns,
    seen: HashSet<usize>,
    seen_non_default: HashSet<usize>,
    extras: Vec<String>,
}

impl State<'_> {
    fn take(&mut self, idx: usize, args: &[String]) -> Result<(), Fail> {
        let p = self.p;
        let a = &p.args[idx];
        self.seen.insert(idx);
        let value = get_values(p, a, args)?;
        if a.is_option() || !args.is_empty() {
            self.seen_non_default.insert(idx);
            if let Some(g) = a.group {
                for (j, other) in p.args.iter().enumerate() {
                    if j != idx && other.group == Some(g) && self.seen_non_default.contains(&j) {
                        return Err(arg_error(p, a, &format!("not allowed with argument {}", other.name())));
                    }
                }
            }
        }
        match a.act {
            Act::Help => {
                let (_, help) = texts(&p.prog);
                return Err(Fail(Exit { code: 0, stdout: help.to_string(), stderr: String::new() }));
            }
            Act::Append => {
                let slot = self.ns.entry(a.dest).or_insert(Val::List(vec![]));
                if let Val::List(l) = slot {
                    l.push(value);
                }
            }
            Act::Sub => {
                let Val::List(vals) = value else { unreachable!() };
                let name = val_str(&vals[0]);
                self.ns.insert(a.dest, Val::Str(name.clone()));
                let sub = &p.subs.iter().find(|(n, _)| *n == name).unwrap().1;
                let rest: Vec<String> = vals[1..].iter().map(val_str).collect();
                let (subns, extras) = parse_known(sub, &rest)?;
                self.ns.extend(subns);
                self.extras.extend(extras);
            }
            _ => {
                self.ns.insert(a.dest, value);
            }
        }
        Ok(())
    }
}

fn parse_known(p: &Parser, args: &[String]) -> Result<(Ns, Vec<String>), Fail> {
    let mut st = State { p, ns: Ns::new(), seen: HashSet::new(), seen_non_default: HashSet::new(), extras: vec![] };
    for a in &p.args {
        if a.act != Act::Help && a.act != Act::Sub {
            st.ns.insert(a.dest, a.default.clone());
        }
    }
    // Classify: 'O' option, 'A' argument, '-' the first `--`.
    let mut pattern = Vec::with_capacity(args.len());
    let mut option_at: HashMap<usize, Vec<OptMatch>> = HashMap::new();
    let mut dd = false;
    for (i, s) in args.iter().enumerate() {
        if dd {
            pattern.push(b'A');
        } else if s == "--" {
            pattern.push(b'-');
            dd = true;
        } else {
            match parse_optional(p, s)? {
                None => pattern.push(b'A'),
                Some(t) => {
                    option_at.insert(i, t);
                    pattern.push(b'O');
                }
            }
        }
    }
    let mut positionals: Vec<usize> =
        p.args.iter().enumerate().filter(|(_, a)| !a.is_option()).map(|(i, _)| i).collect();

    let consume_positionals = |st: &mut State, positionals: &mut Vec<usize>, mut start: usize| -> Result<usize, Fail> {
        let acts: Vec<&Arg> = positionals.iter().map(|&i| &p.args[i]).collect();
        let counts = match_partial(&acts, &pattern[start..]);
        for (k, &n) in counts.iter().enumerate() {
            let idx = positionals[k];
            let a = &p.args[idx];
            let mut vals: Vec<String> = args[start..start + n].to_vec();
            if a.nargs == Nargs::Parser {
                if pattern.get(start) == Some(&b'-') {
                    vals.remove(0);
                }
            } else if a.nargs != Nargs::Remainder
                && pattern[start..start + n].contains(&b'-')
                && let Some(pos) = vals.iter().position(|v| v == "--")
            {
                vals.remove(pos);
            }
            start += n;
            st.take(idx, &vals)?;
        }
        positionals.drain(..counts.len());
        Ok(start)
    };

    let max_opt = option_at.keys().copied().max();
    let mut start = 0usize;
    if let Some(max_opt) = max_opt {
        while start <= max_opt {
            let mut next = start;
            while next <= max_opt && !option_at.contains_key(&next) {
                next += 1;
            }
            if start != next {
                let end = consume_positionals(&mut st, &mut positionals, start)?;
                if end > start {
                    start = end;
                    continue;
                }
                start = end;
            }
            if !option_at.contains_key(&start) {
                st.extras.extend(args[start..next].iter().cloned());
                start = next;
            }
            // consume_optional
            let tuples = &option_at[&start];
            if tuples.len() > 1 {
                let opts = tuples.iter().map(|t| t.string.as_str()).collect::<Vec<_>>().join(", ");
                return Err(error(p, &format!("ambiguous option: {} could match {opts}", args[start])));
            }
            let t = &tuples[0];
            let Some(idx) = t.arg else {
                st.extras.push(args[start].clone());
                start += 1;
                continue;
            };
            let a = &p.args[idx];
            let (stop, vals) = match &t.explicit {
                Some(explicit) => {
                    let n = match_option(a, b"A").map_err(|m| arg_error(p, a, &m))?;
                    if n == 0 && !t.string.starts_with("--") && !explicit.is_empty() {
                        // Only -h is single-dash here: `-hX` still means help.
                        (start + 1, vec![])
                    } else if n == 1 {
                        (start + 1, vec![explicit.clone()])
                    } else {
                        return Err(arg_error(p, a, &format!("ignored explicit argument {}", crate::util::py_repr(explicit))));
                    }
                }
                None => {
                    let n = match_option(a, &pattern[start + 1..]).map_err(|m| arg_error(p, a, &m))?;
                    (start + 1 + n, args[start + 1..start + 1 + n].to_vec())
                }
            };
            st.take(idx, &vals)?;
            start = stop;
        }
    }
    let stop = consume_positionals(&mut st, &mut positionals, start)?;
    st.extras.extend(args[stop..].iter().cloned());

    let missing: Vec<String> = p
        .args
        .iter()
        .enumerate()
        .filter(|(i, a)| a.required() && !st.seen.contains(i))
        .map(|(_, a)| a.name())
        .collect();
    if !missing.is_empty() {
        return Err(error(p, &format!("the following arguments are required: {}", missing.join(", "))));
    }
    Ok((st.ns, st.extras))
}

// ------------------------------------------------------------ typed

#[derive(Debug, Clone, PartialEq)]
pub struct TabOpts {
    pub port: Option<u16>,
    pub port_raw: Option<i64>,
    pub timeout: f64,
}

/// `tab show` options (see show.rs).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowOpts {
    pub view_only: bool,
    pub until_url: Option<String>,
    pub until_closed: bool,
    pub wait_viewer: bool,
    /// Seconds the view may stay up.
    pub timeout: f64,
}

/// `tab wait` conditions (regexes are JavaScript, `RE` or `/RE/flags`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WaitSpec {
    pub url: Option<String>,
    pub url_not: Option<String>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub text_not: Option<String>,
    pub selector: Option<String>,
    pub selector_not: Option<String>,
    pub js: Option<String>,
    pub gone: bool,
    pub any: bool,
    pub follow: bool,
    pub stable_ms: i64,
    pub tick_ms: i64,
    pub dialog_accept: bool,
    pub port: Option<i64>,
}

impl WaitSpec {
    pub fn has_condition(&self) -> bool {
        self.gone || self.has_page_condition()
    }
    pub fn has_page_condition(&self) -> bool {
        self.page_spec().as_object().is_some_and(|m| m.values().any(|v| !v.is_null()))
    }
    /// The conditions evaluated in the page, as the check function's `S`.
    pub fn page_spec(&self) -> serde_json::Value {
        serde_json::json!({
            "url": self.url, "url_not": self.url_not, "title": self.title, "text": self.text,
            "text_not": self.text_not, "selector": self.selector, "selector_not": self.selector_not, "js": self.js,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Check,
    Start { size: String, renderer: Option<String> },
    Stop,
    Status,
    Env,
    Exec { argv: Vec<String> },
    Launch { name: String, argv: Vec<String> },
    Windows,
    Focus { id: i64 },
    Screenshot { path: String, window: Option<i64> },
    Type { text: String },
    Key { key: String, mods: Vec<String> },
    Move { x: i64, y: i64 },
    Click { button: String, at: Option<(i64, i64)>, count: i64 },
    Scroll { direction: String, steps: i64 },
    BrowserStart { temp: bool, port: Option<i64>, audio: bool, urls: Vec<String> },
    BrowserStop { unit: Option<String>, all: bool },
    BrowserList,
    TabOpen { o: TabOpts, url: Option<String> },
    TabGoto { o: TabOpts, tab: String, url: String, wait_ms: Option<i64> },
    TabEval { o: TabOpts, tab: String, js: String },
    TabShot { o: TabOpts, tab: String, path: String, full_page: bool },
    TabInfo { o: TabOpts, tab: String },
    TabClose { o: TabOpts, tab: String },
    TabList { o: TabOpts },
    TabShow { o: TabOpts, tab: String, show: ShowOpts },
    TabUnshow { tab: String },
    TabWait { o: TabOpts, tab: String, spec: WaitSpec },
    View { port: i64, view_only: bool, exit_on_disconnect: bool },
    Unview,
    HoldInput,
}

fn s(ns: &Ns, k: &str) -> String {
    match ns.get(k) {
        Some(Val::Str(s)) => s.clone(),
        _ => String::new(),
    }
}
fn os_(ns: &Ns, k: &str) -> Option<String> {
    match ns.get(k) {
        Some(Val::Str(s)) => Some(s.clone()),
        _ => None,
    }
}
fn i(ns: &Ns, k: &str) -> Option<i64> {
    match ns.get(k) {
        Some(Val::Int(v)) => Some(*v),
        _ => None,
    }
}
fn b(ns: &Ns, k: &str) -> bool {
    matches!(ns.get(k), Some(Val::Bool(true)))
}
fn list(ns: &Ns, k: &str) -> Vec<String> {
    match ns.get(k) {
        Some(Val::List(l)) => l.iter().map(val_str).collect(),
        _ => vec![],
    }
}

/// The subparser at `path` under `p` (e.g. ["tab", "wait"]).
fn sub_parser<'a>(p: &'a Parser, path: &[&str]) -> &'a Parser {
    path.iter().fold(p, |p, name| &p.subs.iter().find(|(n, _)| n == name).expect("known subcommand").1)
}

/// Option combinations argparse can't express, reported like its errors
/// (usage, exit 2), so a bad invocation never looks like a page-side error.
fn check_wait(p: &Parser, o: &TabOpts, spec: &WaitSpec) -> Result<(), Exit> {
    let fail = |msg: &str| Err(error(p, msg).0);
    if !spec.has_condition() {
        return fail(
            "give a condition: --url, --url-not, --title, --text, --text-not, --selector, --selector-not, --js or --gone",
        );
    }
    if spec.gone && spec.has_page_condition() && !spec.any {
        return fail("argument --gone: with page conditions, also pass --any (a closed tab has no page)");
    }
    if o.timeout.is_nan() || o.timeout < 0.0 {
        return fail("argument --timeout: must be >= 0 (0: no limit)");
    }
    if spec.stable_ms < 0 {
        return fail("argument --stable-ms: must be >= 0");
    }
    if spec.tick_ms <= 0 {
        return fail("argument --tick-ms: must be > 0");
    }
    Ok(())
}

/// Parse argv (without the program name) like `parser().parse_args(argv)`.
pub fn parse(argv: &[String]) -> Result<Cmd, Exit> {
    let top = build();
    let (ns, extras) = parse_known(&top, argv).map_err(|f| f.0)?;
    if !extras.is_empty() {
        return Err(error(&top, &format!("unrecognized arguments: {}", extras.join(" "))).0);
    }
    let tab_opts = |ns: &Ns| {
        let raw = i(ns, "port");
        TabOpts {
            port: raw.and_then(|p| u16::try_from(p).ok()),
            port_raw: raw,
            timeout: match ns.get("timeout") {
                Some(Val::Float(f)) => *f,
                _ => 30.0,
            },
        }
    };
    let cmd = match s(&ns, "command").as_str() {
        "check" => Cmd::Check,
        "start" => Cmd::Start { size: s(&ns, "size"), renderer: os_(&ns, "renderer") },
        "stop" => Cmd::Stop,
        "status" => Cmd::Status,
        "env" => Cmd::Env,
        "exec" => Cmd::Exec { argv: list(&ns, "argv") },
        "launch" => Cmd::Launch { name: s(&ns, "name"), argv: list(&ns, "argv") },
        "windows" => Cmd::Windows,
        "focus" => Cmd::Focus { id: i(&ns, "id").unwrap_or(0) },
        "screenshot" => Cmd::Screenshot { path: s(&ns, "path"), window: i(&ns, "window") },
        "type" => Cmd::Type { text: s(&ns, "text") },
        "key" => Cmd::Key { key: s(&ns, "key"), mods: list(&ns, "mod") },
        "move" => Cmd::Move { x: i(&ns, "x").unwrap_or(0), y: i(&ns, "y").unwrap_or(0) },
        "click" => {
            let at = match ns.get("at") {
                Some(Val::List(l)) if l.len() == 2 => match (&l[0], &l[1]) {
                    (Val::Int(x), Val::Int(y)) => Some((*x, *y)),
                    _ => None,
                },
                _ => None,
            };
            Cmd::Click { button: s(&ns, "button"), at, count: i(&ns, "count").unwrap_or(1) }
        }
        "scroll" => Cmd::Scroll { direction: s(&ns, "direction"), steps: i(&ns, "steps").unwrap_or(3) },
        "browser" => match s(&ns, "browser_command").as_str() {
            "start" => Cmd::BrowserStart {
                temp: b(&ns, "temp"),
                port: i(&ns, "port"),
                audio: b(&ns, "audio"),
                urls: list(&ns, "url"),
            },
            "stop" => Cmd::BrowserStop { unit: os_(&ns, "unit"), all: b(&ns, "all") },
            _ => Cmd::BrowserList,
        },
        "tab" => {
            let o = tab_opts(&ns);
            match s(&ns, "tab_command").as_str() {
                "open" => Cmd::TabOpen { o, url: os_(&ns, "url") },
                "goto" => Cmd::TabGoto { o, tab: s(&ns, "tab"), url: s(&ns, "url"), wait_ms: i(&ns, "wait_ms") },
                "eval" => Cmd::TabEval { o, tab: s(&ns, "tab"), js: s(&ns, "js") },
                "shot" => Cmd::TabShot { o, tab: s(&ns, "tab"), path: s(&ns, "path"), full_page: b(&ns, "full_page") },
                "info" => Cmd::TabInfo { o, tab: s(&ns, "tab") },
                "close" => Cmd::TabClose { o, tab: s(&ns, "tab") },
                "show" => Cmd::TabShow {
                    o,
                    tab: s(&ns, "tab"),
                    show: ShowOpts {
                        view_only: b(&ns, "view_only"),
                        until_url: os_(&ns, "until_url"),
                        until_closed: b(&ns, "until_closed"),
                        wait_viewer: b(&ns, "wait_viewer"),
                        timeout: match ns.get("show_timeout") {
                            Some(Val::Float(f)) => *f,
                            _ => 900.0,
                        },
                    },
                },
                "unshow" => Cmd::TabUnshow { tab: s(&ns, "tab") },
                "wait" => Cmd::TabWait {
                    o,
                    tab: s(&ns, "tab"),
                    spec: WaitSpec {
                        url: os_(&ns, "url"),
                        url_not: os_(&ns, "url_not"),
                        title: os_(&ns, "title"),
                        text: os_(&ns, "text"),
                        text_not: os_(&ns, "text_not"),
                        selector: os_(&ns, "selector"),
                        selector_not: os_(&ns, "selector_not"),
                        js: os_(&ns, "js"),
                        gone: b(&ns, "gone"),
                        any: b(&ns, "any"),
                        follow: b(&ns, "follow"),
                        stable_ms: i(&ns, "stable_ms").unwrap_or(0),
                        tick_ms: i(&ns, "tick_ms").unwrap_or(1000),
                        dialog_accept: s(&ns, "dialog") == "accept",
                        port: None,
                    },
                },
                _ => Cmd::TabList { o },
            }
        }
        "view" => Cmd::View {
            port: i(&ns, "port").unwrap_or(5910),
            view_only: b(&ns, "view_only"),
            exit_on_disconnect: b(&ns, "exit_on_disconnect"),
        },
        "unview" => Cmd::Unview,
        _ => Cmd::HoldInput,
    };
    if let Cmd::TabWait { o, spec, .. } = &cmd {
        check_wait(sub_parser(&top, &["tab", "wait"]), o, spec)?;
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cmd, Exit> {
        let argv: Vec<String> = s.split('|').filter(|x| !x.is_empty()).map(String::from).collect();
        parse(&argv)
    }
    fn err(s: &str) -> String {
        let e = p(s).unwrap_err();
        assert_eq!(e.code, 2, "{s}");
        e.stderr.lines().last().unwrap().to_string()
    }
    fn o() -> TabOpts {
        TabOpts { port: None, port_raw: None, timeout: 30.0 }
    }

    #[test]
    fn options_and_positionals_intermix_like_argparse() {
        assert_eq!(p("tab|open|--port|9425|https://x").unwrap(),
                   Cmd::TabOpen { o: TabOpts { port: Some(9425), port_raw: Some(9425), timeout: 30.0 }, url: Some("https://x".into()) });
        assert_eq!(p("tab|open|https://x|--port|9425").unwrap(),
                   Cmd::TabOpen { o: TabOpts { port: Some(9425), port_raw: Some(9425), timeout: 30.0 }, url: Some("https://x".into()) });
        assert_eq!(p("tab|goto|T|--wait-ms|5|U").unwrap(),
                   Cmd::TabGoto { o: o(), tab: "T".into(), url: "U".into(), wait_ms: Some(5) });
        assert_eq!(p("tab|shot|--full|T|P").unwrap(),
                   Cmd::TabShot { o: o(), tab: "T".into(), path: "P".into(), full_page: true });
        assert_eq!(
            p("tab|wait|T|--url-not|^https://x/login|--text|/approved/i|--any|--stable-ms|500|--timeout|0|--follow").unwrap(),
            Cmd::TabWait {
                o: TabOpts { timeout: 0.0, ..o() },
                tab: "T".into(),
                spec: WaitSpec {
                    url_not: Some("^https://x/login".into()),
                    text: Some("/approved/i".into()),
                    any: true,
                    follow: true,
                    stable_ms: 500,
                    tick_ms: 1000,
                    ..WaitSpec::default()
                },
            }
        );
        assert_eq!(p("tab|open|--t|3").unwrap(), Cmd::TabOpen { o: TabOpts { timeout: 3.0, ..o() }, url: None });
        assert_eq!(p("tab|open|--timeout=1_0").unwrap(), Cmd::TabOpen { o: TabOpts { timeout: 10.0, ..o() }, url: None });
        assert_eq!(p("click|--count|2|right").unwrap(), Cmd::Click { button: "right".into(), at: None, count: 2 });
        assert_eq!(p("click|--at|-5|3").unwrap(), Cmd::Click { button: "left".into(), at: Some((-5, 3)), count: 1 });
        assert_eq!(p("move|-5|3").unwrap(), Cmd::Move { x: -5, y: 3 });
        assert_eq!(p("focus|+5").unwrap(), Cmd::Focus { id: 5 });
        assert_eq!(p("key|a|--mod|ctrl|--mod|shift").unwrap(), Cmd::Key { key: "a".into(), mods: vec!["ctrl".into(), "shift".into()] });
        assert_eq!(p("tab|eval|T|-1").unwrap(), Cmd::TabEval { o: o(), tab: "T".into(), js: "-1".into() });
        assert_eq!(p("tab|eval|T|--|-x").unwrap(), Cmd::TabEval { o: o(), tab: "T".into(), js: "-x".into() });
        assert_eq!(p("tab|eval|--|T|x").unwrap(), Cmd::TabEval { o: o(), tab: "T".into(), js: "x".into() });
        assert_eq!(p("tab|eval|T|--|--").unwrap(), Cmd::TabEval { o: o(), tab: "T".into(), js: "--".into() });
        assert_eq!(p("browser|start|--temp|a|b").unwrap(),
                   Cmd::BrowserStart { temp: true, port: None, audio: false, urls: vec!["a".into(), "b".into()] });
        assert_eq!(p("exec|--|ls|-la").unwrap(), Cmd::Exec { argv: vec!["--".into(), "ls".into(), "-la".into()] });
        assert_eq!(p("exec|ls|-la").unwrap(), Cmd::Exec { argv: vec!["ls".into(), "-la".into()] });
        assert_eq!(p("exec").unwrap(), Cmd::Exec { argv: vec![] });
        assert_eq!(p("scroll|down").unwrap(), Cmd::Scroll { direction: "down".into(), steps: 3 });
        assert_eq!(p("start|--size=800x600").unwrap(), Cmd::Start { size: "800x600".into(), renderer: None });
    }

    #[test]
    fn errors_match_argparse() {
        for (argv, msg) in [
            ("tab|goto", "agent-display tab goto: error: the following arguments are required: tab, url"),
            ("click|foo", "agent-display click: error: argument button: invalid choice: 'foo' (choose from 'left', 'middle', 'right')"),
            ("click|--count|5", "agent-display click: error: argument --count: invalid choice: '5' (choose from '1', '2', '3')"),
            ("focus|x", "agent-display focus: error: argument id: invalid int value: 'x'"),
            ("tab|goto|a|b|--wait-ms|3|--wait-load", "agent-display tab goto: error: argument --wait-load: not allowed with argument --wait-ms"),
            ("start|--size|12", "agent-display start: error: argument --size: expected WIDTHxHEIGHT, e.g. 1920x1080"),
            ("", "agent-display: error: the following arguments are required: command"),
            ("-x", "agent-display: error: the following arguments are required: command"),
            ("tab|open|--full", "agent-display: error: unrecognized arguments: --full"),
            ("browser|start|a|--temp|b", "agent-display: error: unrecognized arguments: b"),
            ("exec|-x", "agent-display: error: unrecognized arguments: -x"),
            ("tab|eval|T|-x", "agent-display tab eval: error: the following arguments are required: js"),
            ("tab|eval|T|a|b", "agent-display: error: unrecognized arguments: b"),
            ("scroll|down|x", "agent-display scroll: error: argument steps: invalid int value: 'x'"),
            ("tab|open|--port", "agent-display tab open: error: argument --port: expected one argument"),
            ("tab|open|--port|--timeout|3", "agent-display tab open: error: argument --port: expected one argument"),
            ("click|--at|1", "agent-display click: error: argument --at: expected 2 arguments"),
            ("click|--at=1", "agent-display click: error: argument --at: expected 2 arguments"),
            ("browser|start|--temp=x", "agent-display browser start: error: argument --temp: ignored explicit argument 'x'"),
            ("move|1|x", "agent-display move: error: argument y: invalid int value: 'x'"),
            ("browser", "agent-display browser: error: the following arguments are required: browser_command"),
            ("tab|list|--port|x", "agent-display tab list: error: argument --port: invalid int value: 'x'"),
        ] {
            assert_eq!(err(argv), msg, "{argv}");
        }
        let full = p("tab|goto").unwrap_err().stderr;
        assert!(full.starts_with("usage: agent-display tab goto [-h] [--port PORT] [--timeout TIMEOUT]\n"));
    }

    #[test]
    fn tab_show_and_unshow_parse() {
        let show = |view_only, until_url: Option<&str>, until_closed, wait_viewer, timeout| ShowOpts {
            view_only,
            until_url: until_url.map(String::from),
            until_closed,
            wait_viewer,
            timeout,
        };
        assert_eq!(p("tab|show|T").unwrap(), Cmd::TabShow { o: o(), tab: "T".into(), show: show(false, None, false, false, 900.0) });
        assert_eq!(
            p("tab|show|--view-only|--until-url|^https://x/home|--until-closed|--wait-viewer|--timeout|60|T").unwrap(),
            Cmd::TabShow { o: o(), tab: "T".into(), show: show(true, Some("^https://x/home"), true, true, 60.0) }
        );
        // --timeout is the view's lifetime; CDP steps keep their default.
        let Cmd::TabShow { o: opts, .. } = p("tab|show|--port|9425|--timeout|5|T").unwrap() else { panic!() };
        assert_eq!((opts.port, opts.timeout), (Some(9425), 30.0));
        assert_eq!(p("tab|unshow|T").unwrap(), Cmd::TabUnshow { tab: "T".into() });
        assert_eq!(p("view|--exit-on-disconnect").unwrap(), Cmd::View { port: 5910, view_only: false, exit_on_disconnect: true });
        assert_eq!(err("tab|show"), "agent-display tab show: error: the following arguments are required: tab");
        assert_eq!(err("tab|unshow|--port|1|T"), "agent-display: error: unrecognized arguments: --port T");
        assert_eq!(err("tab|show|T|--timeout|x"), "agent-display tab show: error: argument --timeout: invalid float value: 'x'");
    }

    #[test]
    fn new_help_texts() {
        let h = |s: &str| {
            let e = p(s).unwrap_err();
            assert_eq!(e.code, 0, "{s}");
            e.stdout
        };
        let tab = h("tab|-h");
        assert!(tab.starts_with("usage: agent-display tab [-h]\n                         {open,goto,eval,shot,info,close,list,show,unshow,wait} ...\n"));
        assert!(tab.contains("    show                let a human see and use this tab's window over VNC\n"));
        assert!(tab.contains("  4   tab gone: closed or crashed (checked twice, ~250 ms apart)\n"));
        assert!(tab.contains("  75  no definitive answer (browser stalled or restarting): try again\n"));
        let show = h("tab|show|--help");
        assert!(show.starts_with("usage: agent-display tab show [-h] [--port PORT] [--timeout SECONDS]\n"));
        assert!(show.contains("  --until-url REGEX  end the view once the tab's URL matches REGEX"));
        assert!(h("tab|unshow|-h").starts_with("usage: agent-display tab unshow [-h] tab\n"));
        assert!(h("tab|unshow|-h").contains("75 if\nthe viewer could not be stopped (try again)"));
        assert!(show.contains("{\"done\": TAB, \"reason\": ..., \"still_shown\": true}"));
        assert!(h("view|-h").contains("  --exit-on-disconnect  stop the viewer when the last client disconnects\n"));
        // Usage errors print the subcommand's own usage.
        assert!(p("tab|show").unwrap_err().stderr.starts_with("usage: agent-display tab show [-h] [--port PORT]"));
    }

    /// cli_text.rs is maintained by hand now: every parser must have an
    /// entry whose usage names all its options, positionals and subcommands.
    #[test]
    fn every_parser_has_matching_help_text() {
        fn walk(p: &Parser, n: &mut usize) {
            *n += 1;
            let (usage, help) = texts(&p.prog);
            assert!(!usage.is_empty() && !help.is_empty(), "no cli_text entry for {:?}", p.prog);
            assert!(usage.starts_with(&format!("usage: {} [-h]", p.prog)), "{usage}");
            assert!(help.starts_with(usage), "{}: help must start with usage", p.prog);
            let flat = usage.split_whitespace().collect::<Vec<_>>().join(" ");
            for a in &p.args {
                // argparse's usage names an option by its first string.
                if let Some(s) = a.strings.first() {
                    assert!(flat.contains(&format!("[{s}")) || flat.contains(&format!("| {s}")), "{}: {s} missing", p.prog);
                }
                if a.act == Act::Sub {
                    assert!(flat.contains(&format!("{{{}}}", a.choices.join(","))), "{}: subcommands", p.prog);
                } else if !a.is_option() && p.prog != "agent-display exec" && p.prog != "agent-display launch" {
                    assert!(flat.contains(a.dest) || !a.choices.is_empty(), "{}: {} missing", p.prog, a.dest);
                }
            }
            for (_, sub) in &p.subs {
                walk(sub, n);
            }
        }
        let mut n = 0;
        walk(&build(), &mut n);
        assert_eq!(n, TEXTS.len(), "cli_text.rs has entries for parsers that don't exist");
    }

    #[test]
    fn help_exits_zero_with_python_text() {
        let e = p("tab|open|-h").unwrap_err();
        assert_eq!(e.code, 0);
        assert!(e.stdout.starts_with("usage: agent-display tab open [-h] [--port PORT] [--timeout TIMEOUT] [url]\n"));
        assert!(e.stdout.contains("seconds per CDP step (default 30)"));
    }

    #[test]
    fn tab_help_lists_wait_and_wait_help_is_complete() {
        let e = p("tab|--help").unwrap_err();
        assert_eq!((e.code, e.stderr.as_str()), (0, ""));
        assert!(e.stdout.starts_with("usage: agent-display tab [-h]\n                         {open,goto,eval,shot,info,close,list,show,unshow,wait} ...\n"));
        assert!(e.stdout.contains("\n    show                let a human see and use this tab's window over VNC\n"), "{}", e.stdout);
        assert!(e.stdout.contains("\n    wait                wait until the tab reaches a state (URL, text, ...)\n"), "{}", e.stdout);
        assert!(e.stdout.contains("\n  3   timed out (tab show/wait --timeout)\n"), "{}", e.stdout);

        let e = p("tab|wait|--help").unwrap_err();
        assert_eq!(e.code, 0);
        let h = e.stdout;
        assert!(h.starts_with("usage: agent-display tab wait [-h] [--port PORT] [--timeout TIMEOUT]\n"));
        for want in [
            "[--dialog {accept,dismiss}]", "--url RE ", "--url-not RE ", "--title RE ", "--text RE ", "--text-not RE ",
            "--selector CSS ", "--selector-not CSS ", "--js EXPR ", "--gone ", "--any ", "--stable-ms N ", "--follow ",
            "--tick-ms N ", "stalls included", "  0   matched", "  1   error", "  2   usage error", "  3   timeout",
            "  4   the tab was closed or crashed", "  5   the browser went down", "  75  the deadline passed while the browser was stalled",
        ] {
            assert!(h.contains(want), "tab wait help lacks {want:?}:\n{h}");
        }
        // -h works after other arguments too, like argparse.
        assert_eq!(p("tab|wait|T|--url|x|-h").unwrap_err().stdout, h);
        // Usage errors print the wait usage.
        let e = p("tab|wait").unwrap_err();
        assert_eq!(e.code, 2);
        assert!(e.stderr.starts_with("usage: agent-display tab wait [-h]"), "{}", e.stderr);
        assert!(e.stderr.ends_with("agent-display tab wait: error: the following arguments are required: tab\n"), "{}", e.stderr);
    }

    #[test]
    fn parses_tab_wait_dialog_and_defaults() {
        let Cmd::TabWait { o, spec, .. } = p("tab|wait|T|--gone").unwrap() else { panic!() };
        assert_eq!((o.timeout, spec.dialog_accept, spec.stable_ms, spec.tick_ms, spec.any, spec.follow), (30.0, false, 0, 1000, false, false));
        let Cmd::TabWait { spec, .. } = p("tab|wait|T|--gone|--dialog|accept").unwrap() else { panic!() };
        assert!(spec.dialog_accept);
        // An expression starting with '-' needs the --js=EXPR form, as in argparse.
        let Cmd::TabWait { spec, .. } = p("tab|wait|T|--js=-1 < 0").unwrap() else { panic!() };
        assert_eq!(spec.js.as_deref(), Some("-1 < 0"));
    }

}
