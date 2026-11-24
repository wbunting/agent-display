# agent-display

Rust port of the `agent-display` tool from `wbunting/dotfiles`
(`skills/agent-display/agent-display`, Python, at c598528; that repo is private): a private
headless sway display, input and agent-only Chromium for agents on the
Omarchy host, plus `agent-display tab` commands to drive a tab over CDP.
The skill docs (`SKILL.md` next to the Python) describe the behaviour; this
port keeps it.

The Python version stays the reference and the installed tool
(`~/.local/bin/agent-display`) until this port is switched in.

## Compatibility

Same subcommands, flags, JSON output (key order, `", "`/`": "` separators,
`ensure_ascii` where the Python uses it), exit codes, stderr messages, safety
rules and files under `$XDG_RUNTIME_DIR/agent-display/` and
`~/.local/state/agent-display/`, so it can take over from the Python with no
state migration and both can be used against the same running display
(`browsers/*.json`, `tab-outputs/` markers and the `tab-outputs.lock` flock
are shared).

- The command line is parsed with the same algorithm as Python 3.14's
  argparse; usage and `-h` texts are generated from the Python parser
  (`tools/gen_cli_text.py` → `src/cli_text.rs`, 80 columns). Texts for
  commands added or changed in Rust since (`tab` and its subcommands,
  `windows`, `browser start`, `view`) come from
  `python3 -I tools/gen_cli_text_rust.py src/cli_text.rs` (argparse, same
  format; `--check` tells whether the file is current, and a unit test runs
  it with python3 >= 3.14). Edit the generator, not `cli_text.rs`.
- The sway config's `exec ... hold-input` runs this binary.
- `type` splits text into wtype runs in-tree (ported from the `host-gui`
  skill, which the Python imports at runtime).

## Typing, clicking and secrets

Added in Rust (no Python equivalent):

```sh
TAB=$(agent-display tab open | jq -r .tab)
echo "$ONE_TIME_LINK" | agent-display tab goto "$TAB" -     # URL on stdin, not argv
agent-display tab snapshot "$TAB"                           # outline with refs; fields show [filled]/[empty]
agent-display tab type "$TAB" --label Email agent@example.com
set -o pipefail
op read -n 'op://Vault/Item/password' | agent-display tab type "$TAB" --label Password --stdin
agent-display tab type "$TAB" --label 'Verification code' --stdin --submit < "$CODE_FILE"
agent-display tab click "$TAB" --role button --name 'Sign in'
```

- **Finding elements**: `--label` (label, `aria-label`, placeholder,
  title), `--role` [`--name`], `--text` (click only), `--css`, `--ref` (from
  `tab snapshot`) or `--focused`, plus `--nth N`. Names match exactly, then
  ignoring case and spacing, then by substring; the best tier with any
  match decides (a hidden exact match is waited for, never swapped for a
  looser one). Several visible matches are an error that lists them. Every
  frame is searched, including cross-origin (out-of-process) iframes; `--css`
  runs in each frame's document but, like `querySelectorAll`, not inside
  shadow roots (`--label`/`--role` find shadow-DOM fields). `type`/`click` wait up to `--timeout`
  (default 10 s) for the element, polling every 100 ms backing off to 1 s.
- **Refs** are `<doc>.<node>`: the 4-character document tag changes when a
  frame loads a new document, so a ref from an old page fails ("no longer
  loaded") instead of hitting a node of the new one, and an iframe's refs
  never collide with the page's.
- **Secrets**: `tab type --stdin` reads the value from stdin (refused if
  stdin is a terminal, empty, over 64 KiB or not UTF-8; one trailing newline
  is dropped unless `--keep-newline`). It never appears in argv, stdout,
  stderr or errors, only in `Input.insertText`/`Input.dispatchKeyEvent`;
  the process sets `PR_SET_DUMPABLE 0` first. Password fields and
  `autocomplete=one-time-code`/`current-password`/`new-password`/`cc-number`/
  `cc-csc` fields refuse a value on the command line. The field is checked by
  length only: exit **6** if its length afterwards differs (maxlength, input
  masks). `--keys` sends key presses (split one-time-code boxes).
- **Clicks** happen at the element's centre once it is visible, not moving,
  enabled and the topmost node there in every frame up to the page
  (`DOM.getNodeForLocation`, which like the mouse skips `pointer-events:
  none` layers), else e.g. `is covered by <div#modal> "..."`.
- **Focus**: agent tabs never have OS focus, so `type`/`click` turn on
  `Emulation.setFocusEmulationEnabled` while they run and off again when they
  finish (also on errors). Chromium's own CDP input focuses the tab's widget
  (as under Playwright), so after `type` the page may report
  `document.hasFocus()` true; sway focus is never touched.
- **URLs**: `tab open/goto/info/list/snapshot/wait` print URLs with query and
  fragment values, userinfo and token-like path segments as `…`
  (`--show-url` prints them in full); error messages are always redacted.
  `tab list` also redacts titles that are Chromium's URL fallback (a page
  without `<title>` is listed with its URL, query included, as the title),
  and so does `windows` for window titles ("<url> - Chromium"; `windows
  --show-url` prints them in full).
  `tab open -`, `tab goto TAB -` and `browser start -` read URLs from stdin.
- **Browser hygiene**: `browser start URL...` launches Chromium on
  `about:blank` and opens the URLs over CDP, so they are not in the unit's
  `ExecStart` or `/proc/PID/cmdline`. Browser units get `LimitCORE=0` (a
  renderer core holds what was typed). Before each start the profile's
  `Default/Preferences` gets the password manager, auto sign-in and
  address/card autofill turned off.

Exit codes: the shared `tab` statuses (Changes 5 below); `tab type` adds
6 for a length mismatch (then `--submit` does not press Enter).

## Build and test

```sh
cargo build --release          # target/release/agent-display
cargo test                     # unit tests (stub systemd, sway and CDP)
cargo clippy --all-targets -- -D warnings
node isolation_check.mjs <cdp-port> <browser-unit>   # live, see below
tests/e2e/phase0.sh [CHURN_SECONDS]                  # live, isolated, see below
```

## Dependencies

- `serde_json` (`preserve_order`): JSON for CDP, sway IPC and output; object
  key order must match the Python's dicts.
- `libc`: `memfd_create` + `sendmsg(SCM_RIGHTS)` for the hold-input keymap,
  `poll`, `flock`, `umask`, `pidfd_open`, `bind`.

HTTP, WebSocket (CDP), base64, sway IPC and the Wayland wire protocol are
small in-tree implementations. No async runtime: every command is a short
sequence of blocking request/response steps with deadlines.

## Changes from the Python

1. **Browser stalls** (shared browser pausing for seconds under host load):
   the ownership check's `/json/version` and `/json/list` wait 15 s (5 s in
   the Python), and `tab close` retries up to 3 times (1 s, 2 s backoff) on
   stall-type failures while the tab's browser unit is active. If an earlier
   attempt sent `closeTarget` and the tab is gone, it counts as closed. A
   stopped browser still fails at once; refusals are never retried.
2. **Listener ownership** without `ss -Hltnp`: LISTEN socket inodes for the
   port from `/proc/net/tcp{,6}`, then an fd for one of them in the browser
   unit's cgroup pids (`ControlGroup` from systemd, `cgroup.procs`
   recursively; main pid scanned first). Same property as before.
3. **Startup**: no interpreter or imports (~0.5 ms vs ~80 ms per command).
4. **Tab lookup by id** (`Target.getTargetInfo` on the browser endpoint),
   not by scanning `/json/list`, which leaves out a live page while its main
   frame is swapped (cross-site navigation) and made agents see their tab
   as closed. A miss counts only after `Target.getTargets` (DevTools hosts
   are created lazily) and only when it repeats ~250 ms later.
5. **Exit statuses** shared by all `tab` commands (`util::exit`, also in
   `agent-display tab -h`): 0 ok, 1 error, 2 usage, 3 timed out
   (`tab show`/`tab wait --timeout`), 4 tab gone (closed or crashed, checked twice),
   5 browser down, 6 verification mismatch (`tab type`; then `--submit`
   does not press Enter), 75 no definitive answer (stalled or restarting browser,
   after retries within 15 s): try again. A lookup never guesses "closed".
   With or without `--port`: an explicit port is only checked for
   ownership up front (a foreign or hijacked port is refused, 1); whether
   it answers is ridden out like any lookup. `tab open`/`tab list` on a
   stalled browser exit 75 too.
6. **Unknown is not stopped**: `units_active` answers `Option<bool>`; a
   failed or timed-out `systemctl` (or a unit mid-transition) no longer
   deletes a running browser's registration. The same rule holds wherever a
   state decides an outcome: a stalled `tab close` is 5 only if its browser
   is unregistered or confirmed stopped (unknown keeps retrying, then 75);
   `browser stop` closes a browser whose state is unknown and forgets it
   only once it is gone; viewers are handled likewise (item 9).
7. **Ownership from /proc**: the first full check (systemd cgroup scan)
   records who serves a browser's CDP port in `browsers/<unit>.owner`
   (pid, `/proc/PID/stat` start time, LISTEN socket inode). Later commands
   re-prove that from /proc alone and fall back to the full check on any
   mismatch; with a valid record a tab command calls no `systemctl`.
8. **Output gc guard**: an empty tab output is unplugged only once its tab
   is confirmed gone, so another agent's `tab open` no longer unplugs a live
   tab's output while its window is briefly elsewhere. One
   `Target.getTargets` per browser covers all such outputs (plus one
   recheck ~250 ms later), only when there are any.
9. **`tab show TAB` / `tab unshow TAB`**: hand one tab to a human. A wayvnc
   capturing only that tab's output, on a Unix socket in the runtime dir
   (`vncviewer /run/user/1000/agent-display/vnc/HEADLESS-N.sock`), that
   exits when the viewer disconnects or after `--timeout` (default 900 s;
   wayvnc's own kill timer runs 2 s later, so a waiting `show` exits 3
   first). Showing a tab that is already shown reports the running
   viewer's mode and remaining time, refuses a different `--view-only`
   mode, and never stops a viewer it didn't start (each viewer's state
   file holds a random token; a `show` stops a viewer only while the file
   still holds the token it wrote). Re-showing right after a viewer exited
   waits out its still-stopping unit. Concurrent shows of one tab are
   serialised (`vnc/HEADLESS-N.lock`): one starts the viewer and writes its
   state once wayvnc runs, the others reuse it. Stopping a viewer (end of a
   view, `tab unshow`, gc) happens under the same lock and treats an
   unknown unit state as possibly running: it stops the unit and removes
   the viewer's files only once that worked (a failed stop is 75 for
   `unshow`, and the files stay for a retry). `unshow` also finds a running
   viewer whose files were lost. Starting follows the same rule: a `show`
   that can't read the viewer unit's state while a viewer's files exist
   exits 75 instead of starting over it, and a stopping unit (systemd says
   `deactivating`) is waited out, never one whose state is unknown. On someone else's viewer, `--wait-viewer` ends by its expiry
   at the latest; `--until-*` wait their own `--timeout`.
   `--wait-viewer`, `--until-url REGEX` and `--until-closed` keep it in the
   foreground, printing `{"viewer": "connected"}` / `"disconnected"` JSON
   lines and ending the view on the condition (the URL is never printed).
   With `--until-*`, exit 0 means the condition was met: if the human
   leaves first (`{"viewer": "stopped"}`), the tab is still watched until
   the condition, `--timeout` (3) or the tab going away (4; 5 if its whole
   browser went away), since a sign-in can finish on the human's phone
   after they close the viewer. A poll that fails on a broken connection
   (browser exited or restarted) looks the tab up afresh at once, and
   reconnects if it is still there; a stalled browser counts as busy for
   5 s first. With `--until-closed`, the tab going away is "closed" (0).
   `--wait-viewer` alone ends (0) when the viewer disconnects. The
   show-then-wait handoff (`tab show TAB`, give the human the viewer
   command, then `tab show TAB --until-url ...`) works: a waiting show takes
   over a viewer that a show returning at once started (nobody else would
   end it) and ends it. A viewer another waiting show owns is left to that
   one; the last line then says so (`{"done": TAB, "reason": ...,
   "still_shown": true}`) instead of `unshown`.
   `tab close` and the output gc stop a tab's viewer. If the viewer
   event stream (`wayvncctl`) dies while wayvnc runs, before or after a
   human connected, it is reopened (without `-w`, so it ends at once if
   wayvnc is gone) and the unit's state is checked with a backoff (0.5 s
   up to 5 s), not every poll; systemd, not whether a viewer was seen,
   decides that the viewer is gone. A tab that its browser reports gone is
   confirmed with that browser only, so an unrelated stalled browser can't
   keep a closed tab's view waiting.
   `view` gained `--exit-on-disconnect`; reusing a running `view` says
   whether that viewer stops by itself (it keeps the options it was
   started with).
10. **JavaScript dialogs** don't wedge tabs: `goto`, `eval`, `shot` and `info`
    enable Page events and answer any dialog that opens while they are
    attached (noted on stderr; stdout unchanged): `alert`/`confirm`/`prompt`
    are dismissed, `beforeunload` is accepted so the navigation that raised
    it goes ahead. They make one round trip to the page before
    detaching. Before, a dialog that opened during a command (for example an
    inline `alert()` during `goto`) left the renderer blocked for every
    later client. See [docs/tab-wait.md](docs/tab-wait.md#javascript-dialogs).

Also: sway IPC over `SWAYSOCK` instead of `swaymsg` subprocesses (and a
5 ms window poll in `tab open`/`close` instead of 20 ms); one `systemctl show`
for all registered browser units instead of `systemctl is-active` per unit.

`-h` and usage texts are in `src/cli_text.rs`; entries for commands the
Python predates (`tab show`, `tab wait`, `tab type/click/snapshot`, ...) come
from `tools/gen_cli_text_rust.py` (see Compatibility); a test checks every
parser has a matching entry.

Known differences: `-h` and usage texts are always the 80-column argparse
text (Python wraps to the terminal width and colours it on a TTY); error
texts that quote `swaymsg` or urllib internals read `sway IPC: ...` or
`<urlopen error [Errno N] ...>`-style approximations; a JSON integer beyond
64 bits from `tab eval` prints as a float.

## `tab wait` (new in this port)

`agent-display tab wait TAB --url RE | --url-not RE | --title RE | --text RE |
--text-not RE | --selector CSS | --selector-not CSS | --js EXPR | --gone` blocks
until the tab reaches that state. It is driven by page events, not polling,
and rides out browser stalls, using the same lookup and exit statuses as the
other `tab` commands: 0 on a match, 3 on timeout, 4 if the tab was closed or
crashed, 5 if the browser went down, 75 if the deadline passed while the
browser was stalled. `--follow` prints
a JSON line per state change, for background monitors. Use it instead of
polling `tab info`/`tab list` when waiting for a human (phone approvals, SMS
codes, email links). Its `url` (and a URL-like `title`) is redacted like
every `tab` command's output unless `--show-url`; conditions such as `--url`
still match the full URL in the page. See [docs/tab-wait.md](docs/tab-wait.md).

## Verification (omarchy, sway 1.12, Chromium on the shared display)

- `cargo test`: the Python's 87 cases, with every `tab` case run with and
  without the sway fake like the Python's subclass; plus parser, JSON, IPC,
  WebSocket, wire-format, /proc ownership and retry tests, the lookup,
  exit-status, owner-cache, gc-guard and `tab show` tests, `tab wait`
  against a scripted page with fake time and its in-page checker in node
  (skipped without node), dialog handling on a socket pair and in
  `goto`/`eval`/`shot`/`info`, and the `tab type/click/snapshot`, URL
  redaction and browser hygiene tests (351 in all).
- `cargo clippy --all-targets --features bench-trace -- -D warnings`: clean.
- Live: `status`, `start` (already running), persistent `browser start`
  (`already_running`), `browser start --temp` / `browser stop UNIT`
  (`Browser.close`, no systemd stop, profile removed), the tab flow on a
  temp browser and on 9425, Python/Rust interop (each closes tabs and
  outputs the other opened), `windows`, `screenshot` (output and
  `--window`), `move`, `click --at`, `type`, `key`, `scroll`, `focus`,
  `exec`, `isolation_check.mjs PORT UNIT` PASS, hold-input in a private
  headless sway (devices registered; keyboard re-asserted after wtype exits),
  and stalls (SIGSTOP of a temp browser) for fix 1.
- Not run live (shared host rules): `stop`, a fresh `start`, `view`,
  `unview`, `launch`. (`start`, `stop`, `view` and the tab changes above
  run live in the isolated e2e below.)
- `tab type/click/snapshot`, URL redaction and browser hygiene: unit tests
  against a fake page behind the fake CDP (`src/tests/input_tests.rs`:
  frames incl. a cross-origin iframe session, AX tree, focus, hit testing,
  field values; fake clock) and a live pass on a `--temp` browser against
  `tests/fixtures/input-*.html` (served with `python3 -m http.server`;
  `input-frame.html` embeds the login page from `localhost` so it runs
  out of process).

## Live checks without touching a live display

`tests/e2e/phase0.sh` runs the real binary, with its real unit names, inside
a nested `systemd --user` started as one outer unit (`ad-e2e-spaces-mgr`,
`Delegate=yes`) with its own `XDG_RUNTIME_DIR`, `HOME` and a unit path of a
few stock targets only (none of the user's units start there). The tool's
`systemctl`/`systemd-run` reach only that manager; stopping the outer unit
removes everything. It checks: lookups during concurrent tab churn and
cross-site navigation, a SIGSTOPped browser (short stall ridden out, long
stall exits 75), the gc guard (optionally against an older binary,
`OLD_BIN=...`), `tab show` end to end with `tests/e2e/rfb.py` as the viewer
(frame, click and typing, events, auto-exit, no listener left),
`--until-url`, `unshow`, `view --exit-on-disconnect`, exit 4 after a real
close and 5 after `browser stop`, and that the live agent-display units are
unchanged. Only temp browsers; needs sway, chromium, wayvnc, python3, jq.

## Benchmark

`bench/rs_ab.py` (interleaved CLI A/B with screenshot freshness checks) and
`bench/phases.py` (phase breakdown; needs `--features bench-trace` builds)
were run against the shared browser on 9425; numbers are in the commit that
added this section.

## Switching over

```sh
cargo install --locked --path . --root ~/.local/opt/agent-display
ln -sfn ~/.local/opt/agent-display/bin/agent-display ~/.local/bin/agent-display
# back to the Python:
ln -sfn ~/.local/src/dotfiles/skills/agent-display/agent-display ~/.local/bin/agent-display
```

A display started by the Python keeps running its Python `hold-input`; the
next `agent-display start` after a stop runs this binary's.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
