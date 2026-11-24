# agent-display spaces: one private display + browser per agent

Status: research (2026-11-20). **Phase 0 (section 4) is implemented**; see
"Phase 0 as built" below. Phases 1–4 are proposals.

The numbers come from throwaway lab scripts (not kept). They ran against a
private sway and temp Chromium with their own units and runtime dir, never
the live display or the persistent profile. The live checks that ship with
phase 0 are `tests/e2e/` (a nested user manager; see `tests/e2e/lib.sh`).

## 1. Why `tab list` / `tab info` dropped a live tab

### What Chromium does

`/json/list` (Chromium 155, `devtools_http_handler.cc`) has no filter of its
own. It lists `DevToolsAgentHost::GetOrCreateAll()`, which walks
`WebContentsImpl::GetAllWebContents()`. A tab is left out while either is
true:

* its main-frame `RenderViewHost` is not `IsMainFrameActive()`
  (`main_frame_routing_id_ == none`, while the main frame is being swapped
  or unloaded), or
* `type` is not `"page"`: `ChromeDevToolsManagerDelegate::GetTargetType()`
  says `page` only while `tabs::TabInterface::MaybeGetFromContents()`
  finds the tab in a tab strip. `tab list` and `find_tab` drop anything
  that isn't a `page`.

`Target.getTargetInfo` / `Target.attachToTarget` use
`DevToolsAgentHost::GetForId()`, a global id map with neither filter.

### What the lab showed

Stress and unplug scripts against a temp Chromium 155 on a private headless
sway 1.12:

| scenario | open/close cycles | polls | victim missing |
|---|---|---|---|
| 4 churners, each tab on its own new output (create_output, move, unplug) | 243 | 1,149 | 0 |
| 8 churners, 90 s, victim navigating cross-site every 150 ms | 615 | 3,176 | 0 |
| 8 churners, 60 s, also checking `type == page` | 431 | 2,071 | 0 |
| victim self-navigating every 30 ms (same-site, renderer-initiated) | n/a | 10,136 | 0 |
| victim through 6-hop cross-site 302 chains + 4 churners | 142 | 1,360 | 0 |
| unplug / move-to-HEADLESS-1 / resize the victim's own output | n/a | ~50,000 | 0 |

So the obvious suspects don't reproduce it: output add/unplug, concurrent
`createTarget`/`closeTarget` and navigation. The Chromium-side windows above
exist but are narrow. What the real session saw (two agents' tabs gone for one
poll, then back unchanged) fits a page whose main frame was mid-swap, such as
a prerender/BFCache activation or an SPA sign-in redirect, which my synthetic
pages don't do. I can't prove it without the real pages.

### Tool-side defects (found by reading the code)

Each of these turns a transient failure into "tab gone":

1. **`find_tab` failures collapse into one message.** Any failure on the
   persistent port's ownership check, whether `systemctl show` timing out
   (15 s), a `/json/version` stall or a cgroup scan miss, makes
   `find_tab` *skip* that port and print `no tab X in an agent-display
   browser (closed?); skipped unhealthy browsers: ...`, exit 1. A waiting
   agent reads that as "closed". This is the most likely cause of the
   `tab info` false alarm.
2. **A transient `systemctl` failure deletes the browser registration.**
   `units_active()` turns a failed or timed-out `systemctl show` into
   "inactive" (`unwrap_or_default`), and `browsers_real()` then
   **unlinks** `browsers/agent-display-browser.json` for a browser that is
   still running. `browser start` on a running unit returns
   `already_running` without rewriting the file, so this doesn't heal.
3. **Every tab command does 2+ `systemctl` calls, an HTTP `/json/version`,
   a `/json/list` and a WebSocket** against the one browser and user manager
   shared by all agents. Shared load hits everyone's lookups at once.
4. **`tab list` has no retry and isn't scoped.** Any omission is shown as
   fact, and the agent compares a list of everyone's tabs.
5. **`gc_tab_outputs` unplugs any marked output that has 0 windows in the
   tree**, without checking that the tab still exists. If a window
   is momentarily off its output (Chromium remaps a surface, fullscreen,
   dialog), another agent's `tab open` can unplug it and drop the window
   onto HEADLESS-1, hidden in the tabbed stack. That's a cross-agent
   destabilisation path even when listing works.

### Fix (phase 0, independent of spaces)

* Look tabs up with `Target.getTargetInfo {targetId}` on the browser
  WebSocket, then attach. No `/json/list` scan, and it can't be filtered.
* Report three outcomes: `ok`; **gone** only when a healthy browser says
  `No target with given id found` twice, about 250 ms apart (exit 3);
  otherwise **unavailable** (exit 75, EX_TEMPFAIL) after internal retries
  with backoff of up to about 10 s. Never print "closed?" for a lookup that
  didn't get an answer.
* `units_active` returns `Option<bool>`. Only unlink a registration on a
  positive `inactive`/`not-found`.
* Cache ownership: at `browser start`, record the main pid, its
  `/proc/<pid>/stat` start time and the listener inode. Per command, check
  those three from `/proc`. Fall back to the systemd/cgroup scan only on a
  mismatch, so normal commands don't call `systemctl`.
* `gc_tab_outputs` only unplugs an empty marked output whose tab id
  `getTargetInfo` reports gone.
* `tab list` defaults to the caller's tabs (section 3); `--all` is explicit.

### Phase 0 as built

Differences from the plan above:

* Exit codes follow the contract shared by all `tab` commands
  (`util::exit`): 4 gone, 5 browser down, 75 no definitive answer, 3 is
  reserved for timeouts (`tab wait`, `tab show --timeout`), 6 for
  `tab type` read-back mismatches.
* The retry budget is 15 s (pauses 0.5, 1, 2, 4 s; each step at most 5 s),
  the same tolerance the old 15 s ownership timeout gave stalled browsers.
* A miss is asked again after `Target.getTargets` before it counts:
  Chromium creates DevTools hosts lazily, and getTargets creates them for
  every live frame.
* The owner record (`browsers/<unit>.owner`) is written by the first full
  check, not by `browser start`, so browsers started by older binaries get
  one too. With a valid record, `browser list` and tab lookups make no
  `systemctl` call for that browser.
* `tab list` is unchanged (still `/json/list`, all tabs). Scoping it needs
  spaces (phase 1).
* `tab show --until-url/--until-closed` exit 0 only when the condition is
  met. A viewer that leaves first doesn't end the wait (phone approvals
  finish after the human closes the viewer); `--timeout` gives 3.
* `tab show` doesn't move seat focus: on the shared display that would
  steal focus from other agents' OS input. The human's first click in the
  viewer focuses the window, and popups then open on its output.

## 2. Isolation units and their cost

Measured on this host (Ryzen, 32 threads, 121 GiB RAM, warm page cache):

| unit | start | memory | sign-ins persist | isolates |
|---|---|---|---|---|
| CDP browser context (`createBrowserContext`) in a shared browser | 37–57 ms | ~56 MB per context+tab | **no** (in-memory only) | cookies/storage only; same CDP endpoint, same crash domain, same seat |
| temp profile, own Chromium | 265 ms to CDP | ~237 MB PSS idle (+tabs) | no | cookies, CDP endpoint, crash domain |
| named persistent profile, own Chromium | ~265 ms (+profile size) | same | **yes** | as above, plus a persistent sign-in scope |
| own sway display | 215 ms | ~21 MB anon (86 MB RSS, mostly shared mesa) | n/a | seat (input, focus), clipboard, D-Bus, sway IPC, outputs, VNC view |
| Chromium over `--remote-debugging-pipe` | 224 ms | same as own Chromium | n/a | **no TCP listener at all**; only the pipe holder can drive it |
| separate Linux uid / container | minutes to set up, sudo | + per-user services | yes | real file and process boundary between agents |
| Hydra microVM (exists already) | seconds | GBs | per VM | strongest; heavy |

Notes:

* **The uid is the real limit.** Every agent runs as `will`. Any agent can
  read `~/.local/state/agent-display/*/` (cookies included), any 0700
  runtime dir, other agents' `/proc/<pid>/environ`, and any loopback port.
  `ptrace_scope=1` blocks only non-ancestor ptrace. Same-uid spaces stop
  accidents and casual cross-driving, not a deliberately hostile agent.
  For finance/admin credentials that need a hard boundary, see phase 4.
* **Named persistent profiles are single-process.** Chromium's
  `SingletonLock` allows one browser per `user-data-dir`, and CDP can't open
  a persistent profile as a context. So per-account profiles mean one
  Chromium per profile, held by one space at a time.
* **Contexts are too weak.** They don't persist sign-ins, and any client on
  the endpoint can see and drive every context's targets.
* **Seats:** sway supports `ext_transient_seat_v1`, but Chromium's Wayland
  backend uses one seat. In the lab, a wayvnc `--transient-seat` client moved
  the cursor but no clicks or keys reached the page. The same client on
  seat0 worked (`clicks=1 val=hello`). Separating a human's input from other
  agents' input therefore needs a **separate sway per space**, not a seat per
  viewer.

**Recommendation:** one space = own sway + own Chromium, with a temp profile
by default or a leased named profile. That's about 260 MB and about 0.5 s per
space. Ten concurrent spaces is about 2.6 GB on a 121 GB host.

## 3. Proposed design

### Concepts

* **Space**: `{sway instance, Chromium, CDP endpoint, optional VNC, lease}`.
  Name `[a-z0-9-]{1,32}`. Default name from `AGENT_DISPLAY_SPACE`, else a
  short hash of `T3CODE_THREAD_ID`, else an error (no silent sharing).
* **Profile**: `temp` (default; `/var/tmp`, deleted on release) or a named
  persistent profile `~/.local/state/agent-display/profiles/<name>/`
  (`finance`, `google-admin`, `github`, ...). A named profile is leased
  **exclusively** to one space. A second claimant gets `busy` (exit 75)
  with the holder's name and idle time, or blocks with `--wait`.
* **Lease** (`spaces/<name>/lease.json`): `owner` (thread id or explicit),
  `owner_pid` + start time (nearest agent-harness ancestor, e.g. `pi`),
  `created`, `last_used` (touched by every command), `ttl` (default 2 h
  idle), `viewer` (bool).

### Layout and units

```
$XDG_RUNTIME_DIR/agent-display/spaces/<name>/   0700
  env, sway.config, dbus.conf, wayland-1, sway-ipc.sock, dbus-*
  cdp.sock          broker socket (phase 2)
  vnc.sock vncctl   only while shown
  tabs.json         tab id -> output, opened_by, created
  lease.json
agent-display-<name>.slice          MemoryMax=4G, TasksMax, CPUWeight
  agent-display-sway@<name>.service
  agent-display-browser@<name>.service   BindsTo sway@
  agent-display-broker@<name>.service    (phase 2; owns the CDP pipe)
  agent-display-vnc@<name>.service       on demand, --exit-on-disconnect
```

Releasing a space: CDP `Browser.close` (flushes cookies), then
`systemctl --user stop agent-display-<name>.slice`. Temp profiles go with
`ExecStopPost`. The slice gives one teardown handle and per-space memory
limits, so one runaway page can't OOM other agents.

### Commands

```
agent-display space claim [NAME] [--profile P | --temp] [--wait[=SECS]] [--ttl 2h]
    -> {"space": "...", "profile": "temp|P", "cdp": {"socket": ".../cdp.sock",
        "ws": "ws://127.0.0.1:PORT/devtools/browser/<token>"}, "output": "HEADLESS-1"}
    Idempotent for the same owner. Starts sway + browser (~0.5 s).
agent-display space release [NAME]       Browser.close, stop slice, free profile
agent-display space list                 names, owner, profile, idle, viewer; no URLs
agent-display space status [NAME]

agent-display profile list | create NAME | rm NAME   (rm refuses while leased)

agent-display tab open [URL] | goto | eval | shot | info | close   (--space, default as above)
agent-display tab list                   this space's tabs only
    Exit codes: 0 ok, 3 tab gone (confirmed), 4 not in this space, 75 retry later

agent-display tab show TAB [--view-only] [--until-url REGEX] [--until-closed]
                          [--timeout 15m] [--wait-viewer]
    Hand exactly one tab to the human (below). Prints JSON lines:
    {"viewer_command": "vncviewer .../vnc.sock"} {"viewer":"connected"}
    {"viewer":"disconnected"} {"unshown":"url-matched|closed|timeout|disconnect"}
agent-display tab unshow TAB

agent-display cdp [--space]              Playwright endpoint for this space
# legacy (phase 3 removal): start/browser/view on the shared display
```

### Data flow

```
agent CLI ──(space name)──> spaces/<name>/lease.json, tabs.json
   │
   ├─ phase 1: TCP 127.0.0.1:<space port> ── Chromium(<name>)
   └─ phase 2: cdp.sock ── broker@<name> ──(fd 3/4 pipe)── Chromium(<name>)
                             │  Target.setDiscoverTargets: live target map,
                             │  answers lookups from memory, no polling
                             └─ optional loopback ws with random token for Playwright
sway@<name> ── its own seat0, outputs HEADLESS-1 + one per tab
vnc@<name>  ── wayvnc --output=<tab output> -u vnc.sock --exit-on-disconnect
```

### Enforcing ownership instead of documenting it

* **Phase 1 (scoping).** Each space has its own Chromium on its own port.
  The CLI resolves tabs only inside the caller's space, and
  `tab list`/`info` can't name another space's tab (exit 4). Another
  agent's tabs aren't reachable through the tool, and output/sway IPC
  operations only touch the space's own sway, so `gc`, `focus` and `move`
  can't destabilise another agent.
* **Phase 2 (structural).** Chromium runs with `--remote-debugging-pipe`.
  In the lab, Chromium 155 was CDP-ready in 224 ms with zero listening
  sockets. The per-space broker is the only holder of the pipe and serves
  `cdp.sock` in the 0700 space dir, so there's no port to discover and no
  `/json/list` for anyone to scrape. For Playwright (`connectOverCDP`
  needs `ws://`), the broker can open a loopback port with an unguessable
  path and no `/json/*` discovery, closed on release. Within a space the
  broker passes CDP through unfiltered: a space has a single owner, so it
  needs no per-method allowlist. A cross-context filtering proxy over one
  shared browser was considered and rejected. It would have to filter
  `Target.*` events, `Browser.*` (`close`, downloads), browser-level
  `Storage.getCookies` and auto-attach. That's fragile, and contexts can't
  persist sign-ins anyway.
* **Limit.** Same uid means a determined agent can still read another
  space's files. Phase 4 covers that.

### Human handoff (`tab show`)

Verified in the lab with wayvnc 0.10.2 and a stdlib RFB client (now
`tests/e2e/rfb.py`):

1. The tab already sits alone on its own output. `tab show` runs
   `focus output <tab output>` in the space's sway, so popups the page opens
   (OAuth, 2FA windows) land on that output too. This is safe because the
   space has one owner.
2. Start `vnc@<name>`:
   `wayvnc --output=HEADLESS-N --render-cursor --exit-on-disconnect -u <space>/vnc.sock --socket=<space>/vncctl`.
   The frame showed exactly that tab's window at its 1280x800 size. Clicks
   and typing reached the page. A Unix socket means no TCP port at all, and
   TigerVNC 1.16 connects with `vncviewer /run/user/1000/agent-display/spaces/<name>/vnc.sock`.
3. **Viewer detection:** `wayvncctl -S vncctl -j event-receive` emits
   `client-connected` / `client-disconnected` with a `connection_count`
   (verified). The tool sets `viewer: true` in the lease, prints JSON lines
   for the agent, and blocks the agent's own OS-level input (`click`,
   `type`) on that space while a human is connected. CDP still works.
4. **Auto-unview:** wayvnc exits on the last disconnect (verified: unit
   inactive right after). The tool also calls `wayvncctl wayvnc-exit` when
   the broker sees the tab's URL match `--until-url` (signed in), when the
   tab closes, on `--timeout` (default 15 min) or on `space release`. Nothing
   is left listening afterwards.
5. **Later, when a release ships upstream commit `3ef36130`** (merged
   2026-11-06, unreleased): `--toplevel=<foreign_toplevel_identifier>`
   (sway exposes it in `get_tree`) captures one window instead of an
   output. In 0.10.2 that flag is broken: it sets the source type to OUTPUT
   and fails with `No such output`.
6. The human still only ever sees one space's single output. The shared
   HEADLESS-1 view of everyone's windows goes away.

### Claim, release, cleanup

* `claim` writes the lease under a per-space flock, starts the units and
  records `owner_pid` + start time.
* Every command touches `last_used`.
* `agent-display-reaper.timer` (every 60 s, `agent-display space reap`)
  releases a space when its owner process is gone and it has been idle for
  5 min, or when idle > ttl. It skips spaces with a connected viewer. A
  named profile is released with `Browser.close`, so recent cookies survive
  even if the agent died.
* A crashed browser or sway takes only its own slice down (`BindsTo` inside
  the slice). The next command in that space reports `space-crashed`
  (exit 75), and `claim` restarts it with the same profile.

## 4. Migration from the shared persistent profile

**Phase 0 — harden today's tool (small, ship first).** The section 1 fixes:
`getTargetInfo` lookups, the three-outcome exit codes, never unlinking
registrations on a `systemctl` failure, cached ownership, the safe output
gc. Plus `view` with `--exit-on-disconnect` and `tab show TAB` (wayvnc
`--output=<tab output>`, Unix socket) on the *current* shared display, which
fixes the handoff failure from today's session.

**Phase 1 — spaces with temp profiles.** Instance-aware runtime dir, unit
templates, slices, leases, reaper, `space claim/release`, `--space` on all
commands. The skill tells agents to `claim` first. The legacy shared display
and persistent browser keep running unchanged for anything that needs the
existing sign-ins.

**Phase 2 — named profiles + broker.**
`profile create finance|google-admin|github|...`; the human signs in once
per profile via `space claim --profile X` + `tab show`. Start the named
profiles empty rather than cloning the shared one, so each holds only its
own account's cookies (least privilege). Cloning a full profile would copy
every sign-in into it. Switch spaces to `--remote-debugging-pipe` behind
the broker. Update the skill to map tasks to profiles ("finance work:
`--profile finance`").

**Phase 3 — retire the shared profile.** Once the named profiles cover its
accounts, stop `agent-display-browser` with `Browser.close`. Move
`browser-profile` to `profiles/legacy-shared`, leasable only with
`--profile legacy-shared`, and delete it after the user confirms. Remove
the 9425 default and `tab list` across agents.

**Phase 4 — optional hard boundary for finance/admin.** Run those profiles'
browser and broker as a dedicated system user (`agent-finance`) with
the profile owned by that uid, `cdp.sock` in a group-accessible dir, and
`SO_PEERCRED` plus a per-thread grant the user approves. Then same-uid
agents can no longer read those cookies from disk. This needs sudo setup.
Do it only if the user wants credentials protected from the agents
themselves, not just from accidents.

## 5. Rough implementation plan (Rust, this repo)

1. `ctx.rs`: `Space` (name, runtime dir, unit names). Every path and unit
   name goes through it. `hold-input` accepts `.../spaces/<name>` as well as
   `.../agent-display`.
2. `systemd.rs`: template units, slice with limits, `units_active ->
   Vec<Option<bool>>`.
3. `space.rs`: claim/release/list/status/reap, lease flock, owner-pid
   discovery (walk `PPid` to the first non-shell ancestor), pidfd liveness.
4. `tab.rs`: `getTargetInfo` lookup, exit codes 3/4/75, `tabs.json`
   registry, gc guard, `show`/`unshow` (wayvnc + `wayvncctl event-receive`
   reader).
5. `broker.rs` (phase 2): `agent-display broker <space>` runs as the
   browser's parent with fds 3/4, multiplexes `cdp.sock` clients (id
   remapping, session routing), keeps the target map from
   `Target.setDiscoverTargets`, and serves the Playwright ws port on
   request. Blocking I/O with `poll(2)`, matching the existing no-async
   style.
6. Tests: stub-level, as today, plus the opt-in live checks in
   `tests/e2e/` (nested user manager), extended per phase.

Size: phase 0 is about 300 lines. Phase 1 is about 1.2k. The phase 2 broker
is about 600.
