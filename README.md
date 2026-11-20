# agent-display

Rust port of the `agent-display` tool from `wbunting/dotfiles`
(`skills/agent-display/agent-display`, Python, at c598528): a private
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
  (`tools/gen_cli_text.py` → `src/cli_text.rs`, 80 columns).
- The sway config's `exec ... hold-input` runs this binary.
- `type` splits text into wtype runs in-tree (ported from the `host-gui`
  skill, which the Python imports at runtime).

## Build and test

```sh
cargo build --release          # target/release/agent-display
cargo test                     # unit tests (stub systemd, sway and CDP)
cargo clippy --all-targets -- -D warnings
node isolation_check.mjs <cdp-port> <browser-unit>   # live, see below
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

Also: sway IPC over `SWAYSOCK` instead of `swaymsg` subprocesses (and a
5 ms window poll in `tab open`/`close` instead of 20 ms); one `systemctl show`
for all registered browser units instead of `systemctl is-active` per unit.

Known differences: `-h` and usage texts are always the 80-column argparse
text (Python wraps to the terminal width and colours it on a TTY); error
texts that quote `swaymsg` or urllib internals read `sway IPC: ...` or
`<urlopen error [Errno N] ...>`-style approximations; a JSON integer beyond
64 bits from `tab eval` prints as a float.

## Verification (omarchy, sway 1.12, Chromium on the shared display)

- `cargo test`: 128 tests (the Python's 87 cases, with every `tab` case run
  with and without the sway fake like the Python's subclass; plus parser,
  JSON, IPC, WebSocket, wire-format, /proc ownership and retry tests).
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
  `unview`, `launch`.

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
