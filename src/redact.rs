//! URL and secret hygiene for everything `tab` prints.
//!
//! One-time sign-in links, OAuth codes and reset tokens travel in query
//! strings, fragments and sometimes path segments. Whatever a command
//! prints lands in the agent's transcript (and the model provider's logs),
//! so `tab` output redacts them unless `--show-url` is given.

use crate::bail;
use crate::ctx::Ctx;
use crate::util::Result;

/// Query/fragment values become `…`, keeping parameter names; token-like
/// path segments and userinfo become `…`. `data:` URLs keep only the media
/// type. Unparseable strings are returned as is (they're not URLs).
pub fn redact_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("data:") {
        let mime = rest.split([',', ';']).next().unwrap_or("");
        return format!("data:{mime},…");
    }
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b)) {
        return url.to_string();
    }
    let (before_frag, frag) = match rest.split_once('#') {
        Some((a, f)) => (a, Some(f)),
        None => (rest, None),
    };
    let (before_q, query) = match before_frag.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (before_frag, None),
    };
    let (authority, path) = match before_q.find('/') {
        Some(i) => (&before_q[..i], &before_q[i..]),
        None => (before_q, ""),
    };
    let host = match authority.rsplit_once('@') {
        Some((_, h)) => format!("…@{h}"),
        None => authority.to_string(),
    };
    let path: Vec<String> =
        path.split('/').map(|s| if token_like(s) { "…".to_string() } else { s.to_string() }).collect();
    let mut out = format!("{scheme}://{host}{}", path.join("/"));
    if let Some(q) = query {
        out.push('?');
        out.push_str(&redact_params(q));
    }
    if let Some(f) = frag {
        out.push('#');
        // `#/route` and `#section` are navigation; `#access_token=...` isn't.
        if f.contains('=') {
            out.push_str(&redact_params(f));
        } else if token_like(f) {
            out.push('…');
        } else {
            out.push_str(f);
        }
    }
    out
}

fn redact_params(q: &str) -> String {
    q.split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if !v.is_empty() => format!("{k}=…"),
            Some((k, _)) => format!("{k}="),
            None if token_like(kv) => "…".to_string(),
            None => kv.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Long opaque segment: ≥ 20 URL-safe chars with letters and digits (JWTs,
/// base64url tokens, hex digests, UUIDs). Ordinary slugs rarely qualify.
fn token_like(s: &str) -> bool {
    s.len() >= 20
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~%=".contains(&b))
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_alphabetic())
}

/// A DevTools target title (`/json/list`), redacted when it is Chromium's
/// URL fallback. For a page without a `<title>` (or still loading) the title
/// is the URL formatted for display: no `http://`, HTML-escaped, `%20`
/// shown as a space, query and fragment kept. Titles that start with the
/// page's host, or with any host (`name.tld`, `localhost`, an IP or `[v6]`,
/// optional `:port`) directly followed by `/`, `?` or `#`, or that look like
/// a URL (no spaces and a `/`, `?` or `#`), or that are themselves a URL
/// (`data:...`), go through `redact_url`; other titles are left alone.
pub fn redact_title(title: &str, url: &str) -> String {
    let t = html_unescape(title);
    if t.is_empty() {
        return title.to_string();
    }
    let has_scheme = |s: &str| {
        s.split_once(':').is_some_and(|(sch, _)| {
            !sch.is_empty() && sch.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        }) && (s.starts_with("data:") || s.contains("://") && s.find("://") == s.find(':'))
    };
    if has_scheme(&t) {
        return redact_url(&t);
    }
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .map(|a| a.rsplit_once('@').map(|(_, h)| h).unwrap_or(a))
        .unwrap_or("");
    let bare = host.strip_prefix("www.").unwrap_or(host);
    let from_host = !bare.is_empty() && (t.starts_with(host) || t.starts_with(bare));
    let urlish = !t.contains(char::is_whitespace) && t.contains(['/', '?', '#']);
    if !(from_host || urlish || starts_with_host(&t)) {
        return title.to_string();
    }
    const P: &str = "x-title://";
    let r = redact_url(&format!("{P}{t}"));
    r.strip_prefix(P).map(String::from).unwrap_or(r)
}

/// `title` starts like a schemeless URL: a host, an optional `:port`, then
/// `/`, `?` or `#`. Spaces may follow (Chromium shows `%20` as a space), so
/// this catches what `urlish` can't, without knowing the page's URL.
fn starts_with_host(t: &str) -> bool {
    let rest = if let Some(v6) = t.strip_prefix('[') {
        match v6.find(']') {
            Some(i) if i > 0 && v6[..i].bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.') => &v6[i + 1..],
            _ => return false,
        }
    } else {
        let end = t.find(|c: char| !(c.is_alphanumeric() || c == '.' || c == '-')).unwrap_or(t.len());
        let host = &t[..end];
        let named = host.contains('.')
            && !host.starts_with(['.', '-'])
            && !host.ends_with(['.', '-'])
            && !host.contains("..")
            && host.chars().any(char::is_alphanumeric);
        if !(named || host.eq_ignore_ascii_case("localhost")) {
            return false;
        }
        &t[end..]
    };
    let rest = match rest.strip_prefix(':') {
        Some(r) => {
            let digits = r.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || digits > 5 {
                return false;
            }
            &r[digits..]
        }
        None => rest,
    };
    rest.starts_with(['/', '?', '#'])
}

/// A window title (sway node name). Chromium titles a tab's window
/// "<page title> - Chromium", and for a page without `<title>` the page
/// title is the URL formatted for display, query included (`data:` pages:
/// the whole data URL). Redact the title, or the part before the last
/// " - " (the app name), when `redact_title` would; leave others alone.
pub fn redact_window_title(title: &str) -> String {
    if let Some((head, app)) = title.rsplit_once(" - ") {
        let h = redact_title(head, "");
        if h != head {
            return format!("{h} - {app}");
        }
    }
    redact_title(title, "")
}

fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// Redact URL-valued fields of a printed object in place.
pub fn redact_value(v: &mut serde_json::Value, show: bool) {
    if show {
        return;
    }
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if k == "url" {
                    if let serde_json::Value::String(s) = x {
                        *s = redact_url(s);
                    }
                } else {
                    redact_value(x, false);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| redact_value(x, false)),
        _ => {}
    }
}

/// A value that must not be printed: typed from stdin, no Debug/Display.
/// Zeroed on drop, but that covers only this buffer: the CDP request built
/// from it (JSON value, WebSocket frame) is not zeroed. What protects it in
/// memory is `PR_SET_DUMPABLE 0` (`Sys::no_core_dumps`) and the short life
/// of the process.
pub struct Secret(String);

impl Secret {
    pub const MAX: usize = 64 * 1024;

    /// Read all of stdin. Refuses a terminal (it would echo), empty input
    /// (a failed producer in a pipe without pipefail), oversize and
    /// non-UTF-8 input. Strips exactly one trailing newline (`op read`,
    /// `echo`) unless `keep_newline`. Errors never quote the input.
    pub fn from_stdin(ctx: &Ctx, keep_newline: bool) -> Result<Secret> {
        if ctx.sys.stdin_is_tty() {
            bail!("stdin is a terminal; pipe the value in (e.g. `op read ... | agent-display tab type ... --stdin`)");
        }
        let mut buf = ctx.sys.read_stdin(Self::MAX + 1)?;
        if buf.len() > Self::MAX {
            zero(&mut buf);
            bail!("stdin is larger than {} bytes; refusing", Self::MAX);
        }
        if !keep_newline {
            if buf.ends_with(b"\r\n") {
                buf.truncate(buf.len() - 2);
            } else if buf.ends_with(b"\n") {
                buf.truncate(buf.len() - 1);
            }
        }
        if buf.is_empty() {
            bail!("stdin was empty; did the command that produces the value fail? (use `set -o pipefail`)");
        }
        match String::from_utf8(buf) {
            Ok(s) => Ok(Secret(s)),
            Err(e) => {
                let mut b = e.into_bytes();
                zero(&mut b);
                bail!("stdin is not UTF-8 text");
            }
        }
    }

    pub fn literal(s: &str) -> Secret {
        Secret(s.to_string())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Length as the page counts it (`input.value.length`, UTF-16 units).
    pub fn js_len(&self) -> usize {
        self.0.encode_utf16().count()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zeros are valid UTF-8.
        zero(unsafe { self.0.as_bytes_mut() });
    }
}

pub fn zero(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: valid &mut u8; volatile so the store isn't elided.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}

/// One URL from stdin for `URL` arguments given as `-`, so one-time links
/// stay out of argv (`ps`, /proc/PID/cmdline) and the command transcript.
pub fn url_from_stdin(ctx: &Ctx) -> Result<String> {
    let s = Secret::from_stdin(ctx, false)?;
    let u = s.expose().trim();
    if u.is_empty() || u.contains(char::is_whitespace) {
        bail!("stdin must hold exactly one URL");
    }
    Ok(u.to_string())
}

/// `-` reads the URL from stdin; anything else is the URL.
pub fn url_arg(ctx: &Ctx, url: &str) -> Result<String> {
    if url == "-" { url_from_stdin(ctx) } else { Ok(url.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_query_fragment_userinfo_and_tokens() {
        assert_eq!(
            redact_url("https://bank.example/device/approve?token=abc123&lang=en#x"),
            "https://bank.example/device/approve?token=…&lang=…#x"
        );
        assert_eq!(redact_url("https://a.example/cb#access_token=zzz&state=1"), "https://a.example/cb#access_token=…&state=…");
        assert_eq!(redact_url("https://a.example/#/accounts/summary"), "https://a.example/#/accounts/summary");
        assert_eq!(redact_url("https://u:p@a.example/x"), "https://…@a.example/x");
        assert_eq!(
            redact_url("https://a.example/reset/3f9a1c2b7e4d5f6a8b9c0d1e2f3a4b5c/confirm"),
            "https://a.example/reset/…/confirm"
        );
        assert_eq!(redact_url("https://a.example/settings/notifications"), "https://a.example/settings/notifications");
        assert_eq!(redact_url("about:blank"), "about:blank");
        assert_eq!(redact_url("data:text/html,<title>x</title>"), "data:text/html,…");
        assert_eq!(redact_url("https://a.example/?q="), "https://a.example/?q=");
    }

    #[test]
    fn window_titles_redact_the_url_fallback_before_the_app_name() {
        // Live sway node names of agent tabs (Chromium 155).
        assert_eq!(
            redact_window_title("127.0.0.1:38267/approve.html?token=SECRET-ONE-TIME-abc123&lang=en#frag=zz - Chromium"),
            "127.0.0.1:38267/approve.html?token=…&lang=…#frag=… - Chromium"
        );
        assert_eq!(redact_window_title("data:text/html,<p>secret in data SECRET2</p> - Chromium"), "data:text/html,… - Chromium");
        assert_eq!(
            redact_window_title("bank.example/device/approve?token=SECRET&lang=en - Google Chrome"),
            "bank.example/device/approve?token=…&lang=… - Google Chrome"
        );
        assert_eq!(redact_window_title("https://x.example/cb#access_token=z"), "https://x.example/cb#access_token=…");
        // Chromium shows %20 as a space, so these aren't `urlish`; and
        // `windows` has no URL to anchor on (live titles, Chromium 155).
        assert_eq!(
            redact_window_title("127.0.0.1:41781/approve.html?scope=openid profile&token=SECRET-ONE-TIME-abc123 - Chromium"),
            "127.0.0.1:41781/approve.html?scope=…&token=… - Chromium"
        );
        assert_eq!(
            redact_window_title("localhost:41781/approve.html?q=a - b&token=SECRET-TWO#state=x y - Chromium"),
            "localhost:41781/approve.html?q=…&token=…#state=… - Chromium"
        );
        assert_eq!(
            redact_window_title("bank.example/approve?scope=openid profile&token=SECRET - Chromium"),
            "bank.example/approve?scope=…&token=… - Chromium"
        );
        assert_eq!(redact_window_title("[::1]:8080/cb?code=SECRET x - Chromium"), "[::1]:8080/cb?code=… - Chromium");
        assert_eq!(redact_window_title("bänk.example/a?t=SECRET y - Chromium"), "bänk.example/a?t=… - Chromium");
        for t in [
            "about:blank - Chromium",
            "Fake Bank login - Chromium",
            "Sign in - Bank - Chromium",
            "agent-display-tab-0123456789abcdef - Chromium",
            "foot",
            "~/src/agent-display - nvim",
            "",
            // Host-like starts that aren't followed by / ? #.
            "bank.example - Home - Chromium",
            "v1.2 release notes - Chromium",
            "README.md - nvim",
            "1.5 hours ago? - Chromium",
            "localhost dev server - Chromium",
            ".example/x?y=1 z - Chromium",
            "[nope]/x?y=1 z - Chromium",
            "host.:80/x?y=1 z - Chromium",
        ] {
            assert_eq!(redact_window_title(t), t, "{t}");
        }
        for t in ["127.0.0.1:41781/x?a=b c", "localhost/?a=b c", "localhost?a=1 b", "a.b:1/x?y=1 z", "[::1]/#s=1 2"] {
            assert!(starts_with_host(t), "{t}");
        }
        for t in ["127.0.0.1:/x?a=1 b", "a.b:123456/x", "localhost:/x", "nohost/x?a=1 b", "-a.b/x", "a..b/x", "a.b"] {
            assert!(!starts_with_host(t), "{t}");
        }
    }

    #[test]
    fn titles_that_are_the_url_fallback_are_redacted() {
        let url = "https://bank.example/device/approve?token=SECRET&lang=en";
        // What Chromium's /json/list shows for a page without <title>.
        for t in [
            "bank.example/device/approve?token=SECRET&amp;lang=en",
            "bank.example/device/approve?token=SECRET&lang=en",
            "https://bank.example/device/approve?token=SECRET&lang=en",
        ] {
            let r = redact_title(t, url);
            assert!(!r.contains("SECRET"), "{t} -> {r}");
            assert!(r.contains("token=…"), "{t} -> {r}");
        }
        assert_eq!(redact_title("www.x.example/a?b=1", "https://www.x.example/a?b=1"), "www.x.example/a?b=…");
        assert_eq!(redact_title("x.example/a#access_token=z", "https://www.x.example/a#access_token=z"), "x.example/a#access_token=…");
        assert_eq!(redact_title("127.0.0.1:8080/r/3f9a1c2b7e4d5f6a8b9c0d1e", "http://127.0.0.1:8080/r/3f9a1c2b7e4d5f6a8b9c0d1e"), "127.0.0.1:8080/r/…");
        // data: URLs as titles.
        assert_eq!(redact_title("data:text/html,&lt;p&gt;SECRET&lt;/p&gt;", "data:text/html,<p>SECRET</p>"), "data:text/html,…");
        // Real titles are left as they are.
        for t in ["Fake Bank login", "Sign in – Bank & Co", "bank.example - Home", "Q&amp;A", "", "Dashboard"] {
            assert_eq!(redact_title(t, url), t);
        }
    }
}
