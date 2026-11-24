# `agent-display tab wait`

Block until one tab reaches a state: a URL, a title, text on the page, an
element, a DOM expression, or the tab going away. For waits of seconds to
hours, such as a sign-in the user approves on their phone, an SMS code page
appearing, or an email link being opened.

```
agent-display tab wait TAB [--port PORT] [--timeout S]
    [--url RE] [--url-not RE] [--title RE] [--text RE] [--text-not RE]
    [--selector CSS] [--selector-not CSS] [--js EXPR] [--gone]
    [--any] [--stable-ms N] [--follow] [--tick-ms N] [--dialog accept|dismiss]
    [--show-url]
```

## Conditions

| flag | holds when |
|---|---|
| `--url RE` / `--url-not RE` | `location.href` matches / doesn't |
| `--title RE` | `document.title` matches |
| `--text RE` / `--text-not RE` | `document.body.innerText` matches / doesn't |
| `--selector CSS` / `--selector-not CSS` | `document.querySelector` finds an element / none |
| `--js EXPR` | `EXPR` is truthy |
| `--gone` | the tab was closed, crashed, or its browser went down |

- All given conditions must hold; `--any` makes one enough.
- `RE` is a JavaScript regex: `RE`, or `/RE/flags` for flags (`/approved/i`).
  The `g` and `y` flags are ignored.
- `--js` runs in an isolated world: it sees the DOM but not the page's
  JavaScript globals. It is compiled with the checker, so page CSP doesn't
  block it. An expression that throws ends the wait with exit 1; use `?.`
  for elements that may not exist yet. Start it with `-`? Use `--js=EXPR`.
- `--gone` with page conditions needs `--any` (a closed tab has no page).
- `--stable-ms N`: the match must hold for N ms. Use it when a redirect
  chain passes through the target state and bounces back (SSO).
- Conditions are checked in the main frame only (not inside iframes).

## Output and exit status

One JSON line when the wait ends:

```json
{"tab": "FD23...", "result": "matched", "url": "https://bank.example/home", "title": "Home", "matched": {"url_not": true}, "elapsed": 47.9}
```

| exit | `result` | meaning |
|---|---|---|
| 0 | `matched` | the conditions hold (with `--gone`: the tab or its browser went away) |
| 1 | – | error (stderr): `--port` is reserved or someone else's, TAB isn't a page, an invalid tab id, an invalid regex, the condition threw. Final: never retried. |
| 2 | – | usage error, with the usage line: no condition, `--gone` with page conditions but no `--any`, a negative `--timeout`/`--stable-ms`, `--tick-ms` ≤ 0 |
| 3 | `timeout` | `--timeout` passed (default 30 s; `0` waits forever) with the page answering: the condition never held |
| 4 | `gone` | the tab was closed (`reason: closed`) or crashed (`reason: crashed`) |
| 5 | `browser-down` | the browser went down: unregistered, or its unit confirmed stopped (`reason`). Also at once for a `--port` with no agent-display browser registered and nothing listening |
| 75 | `stalled` | `--timeout` passed while the browser was stalled (no answer since the stall began): the page's state is unknown, so try again |

These are the shared `tab` exit statuses (`util::exit`, `agent-display tab
-h`), with the same meanings as for the other `tab` commands.

`--timeout` bounds the whole wait, stalls included: every lookup and CDP
step gets at most the time left, and only cleanup (answering a last dialog,
detaching) may run past it, so the command returns within `--timeout` + 1 s.
Give a monitor's own timeout at least that much margin.

`url`, `title` and `matched` are the last state seen (null if none was).
Like every `tab` command's output, `url` is redacted unless `--show-url`:
query and fragment values, userinfo and token-like path segments print as
`…` (`https://bank.example/approve?token=…`), and so does a `title` that is
URL-like. Conditions are evaluated in the page against the full URL.
`timeout` and `stalled` lines include `stalled`: the error the browser was
stalled on at the end (null for a plain timeout). With `--follow`, a line per event comes first:

| `event` | fields | when |
|---|---|---|
| `state` | `url`, `title`, `matched` | the URL, title or any condition changed (also the first state) |
| `stall` | `error` | the browser stopped answering or the connection broke (once per stall) |
| `resumed` | | it answers again |
| `dialog` | `type` | the page opened `alert`/`confirm`/`prompt`/`beforeunload` |
| `dialog-closed` | `type`, `handled` | it was `dismissed` or `accepted` (`--dialog`; `beforeunload` is always `accepted`) |

Every line also has `tab` and `t` (seconds since the start). Lines never
contain the page's text or dialog messages: only the URL, the title and
which conditions hold.

## With a background monitor

Wait for the outcome: one wake, nothing to pattern-match:

```
monitor start kind=watch fireOn=exit name="bank sign-in approved" timeoutSeconds=960 \
  command='agent-display tab wait "$TAB" --timeout 900 --any \
           --url-not "^https://bank\.example/login" --text-not "/waiting for your response/i"'
```

The exit status says what happened: 0 approved, 3 nobody approved in 15
minutes, 4 the tab is gone, 5 the browser is down, 75 the browser was stalled
at the end (run the wait again).

Wake on each screen change (phone approval, then an SMS code page, then an
email-link page), with `--follow` and a pattern:

```
monitor start kind=watch once=false name="bank sign-in steps" \
  pattern='"event": "state"|"result": ' \
  command='agent-display tab wait "$TAB" --follow --timeout 3600 --url "/home"'
```

Don't write poll loops around `tab info`/`tab eval` for this: each poll is a
fresh lookup and session, `tab info` on a stalled browser exits 75 after
retrying for up to 15 s, and a poll script has to turn exit statuses into
monitor output itself. `tab wait` holds one session, sees changes within
~100 ms, and its exit status is the outcome.

## How it works

- **One session, own target only.** `tab wait` finds TAB with the lookup
  every `tab` command uses (`tab::locate`: by id with `Target.getTargetInfo`
  on the browser endpoint, never by scanning `/json/list`, which leaves out
  a live page while its main frame is swapped) and attaches one flat CDP
  session to it, with the same ownership checks. It never attaches to
  other targets, never activates or focuses anything, and never sends
  `Runtime.enable` (whose side effects pages can detect). An explicit
  `--port` is checked once up front, as by the other commands.
- **Isolated world.** The checker runs in a world made with
  `Page.createIsolatedWorld`, so page scripts can't see or patch it.
- **Event driven.** Each check is one `Runtime.callFunctionOn` that returns
  at once if the state differs from the last one seen. Otherwise it waits
  in the page for a DOM mutation, `hashchange` or `popstate` that changes it
  (checked at most every 100 ms), or for the `--tick-ms` heartbeat. A check
  that gets no answer within tick + 5 s is reported as a stall and asked
  again on the same session. A
  navigation destroys the world; the call fails at once and the next check
  runs in the new document. Changes are seen within about 100 ms; an idle
  wait costs one small round trip per tick.
- **Evidence, not guesses.**
  - *Closed*: the shared lookup's rule: two definitive "no such target"
    answers ~250 ms apart (after `Target.getTargets` creates DevTools
    hosts). When our session detaches, the tab is looked up again at once.
    Then the browser must still answer 0.5 s later: `browser stop` closes
    every tab before it exits.
  - *Crashed*: `Inspector.targetCrashed` on our session.
  - *Browser down*: the browser is unregistered (its registration is
    dropped only once systemd confirms the unit stopped), or systemd
    confirms its unit stopped. An unknown state (`systemctl` failing or
    timing out during the same host stall, a unit `deactivating`) is not
    down.
  - Anything else is a **stall**: timeouts, refused connections, a lookup
    without a definitive answer while the browser is frozen or restarting.
    It is reported once (`stall`, then `resumed`) and retried with backoff
    (0.5, 1, 2, 4, 5 s). A stall never ends the wait early and never
    extends it: each step's timeout is cut to the time left. If the
    deadline passes during one, the exit is 75, not 3.

## JavaScript dialogs

A dialog that opens while a DevTools session is attached to the tab belongs
to that session. If the session detaches without answering it, the renderer
stays blocked in the dialog, and the tab is wedged for every later client:
`Runtime.evaluate` and `Page.enable` hang, a new session gets "No dialog is
showing", and nothing is drawn. Only closing the tab helps.

So every `tab` command that attaches (`goto`, `eval`, `shot`, `info`,
`wait`) enables Page events and answers each dialog as it opens:

- `alert`, `confirm`, `prompt`:
  - `goto`, `eval`, `shot`, `info` dismiss them and note it on stderr
    (`agent-display: dismissed a JavaScript confirm dialog in tab ...`).
    stdout is unchanged.
  - `wait` follows `--dialog dismiss` (default) or `accept`, reported as
    `dialog` and `dialog-closed` with `--follow`.
- `beforeunload` ("Leave site? Changes you made may not be saved") is
  always **accepted**, by every command, whatever `--dialog` says. It
  belongs to a navigation someone started: `tab goto`, the agent's click,
  or the user. Dismissing it ("Stay on page") cancels that navigation with
  `net::ERR_ABORTED`, so a `tab goto` away from a filled-in form would fail,
  and a background `tab wait` would silently block the agent's own
  navigations. With several sessions attached, each one answers and the
  first answer wins; since all of them accept, they agree.

The pending command then finishes normally. For example, `goto` on a page
whose inline script calls `alert()` gets its load event. Before detaching,
each command makes one round trip to the page, so a dialog opened by what it
just ran (a load handler, a short `setTimeout`) is answered too.

Known limit: a dialog that opens while **no** command is attached is
Chromium's to handle. Chromium dismisses it unless the tab's window is its
most recently activated window. Each `tab open` window is activated briefly
when it's created, so the newest tab can then block in a dialog nobody sees.
Measured on Chromium 155: older tabs dismissed 3 of 3, the newest blocked 1
of 3. Running `tab wait` across the time a page may open a dialog avoids it,
because `tab wait` stays attached.

## Why not Playwright, chrome-devtools-mcp or agent-browser?

They are fine on a `--temp` browser that one agent owns, but none can stay
on one tab of the shared browser:

- **Playwright `connectOverCDP`** (and Playwright MCP) attaches to every
  target with `Target.setAutoAttach {waitForDebuggerOnStart: true}`. It
  sends `Runtime.enable`, `Network.enable`, focus emulation and an init
  script to other agents' tabs. While a Playwright client was stopped, a new
  tab another agent opened stayed paused, and its `tab open URL` failed after
  30 s.
- **chrome-devtools-mcp** auto-attaches the same way (Puppeteer), and
  `select_page` brings the tab to the front. Its only wait is for text.
- **agent-browser** and **browser-use** keep an "active tab" and switch
  between all tabs.

None of them tells a stalled browser from a closed tab.

## Testing

- Unit tests (`cargo test`):
  - `src/tests/wait_tests.rs` scripts the page side against the fake CDP,
    with fake time. It covers every condition, `--any`, `--stable-ms`,
    `--follow`, stalls with reconnect and resume, closed (two listings),
    crashed, `tab close` vs `browser stop`, dialogs, timeouts and errors,
    and a frozen browser not pushing the wait past `--timeout` + 1 s.
  - `check_fn_semantics_in_node` runs the in-page checker in node against a
    small DOM stand-in (skipped with a note if node is missing).
  - `src/cdp.rs` tests answer dialogs on a socket pair.
  - `src/tests/tab_tests.rs` covers dialog handling in `goto`, `eval`,
    `shot` and `info`.
- Live: see the PR that added this command for the end-to-end run on a temp
  browser (each condition, a frozen browser, `tab close`, `browser stop`,
  timeouts, and dialogs during `wait` and `goto`).
