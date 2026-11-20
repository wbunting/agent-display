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
