#!/usr/bin/env node
// Live isolation check: drive an agent-display Chromium over CDP (tabs, a new
// window, popups, navigations, activation/focus requests) while watching the
// user's Hyprland session read-only. Fails if Hyprland's active workspace or
// window changes, or if any window of the browser's processes appears there.
//
//   node isolation_check.mjs <cdp-port> <browser-unit>
//
// Reads Hyprland state only (hyprctl -j queries, event socket); never
// dispatches. Requires Node 22+ (global WebSocket).
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import net from 'node:net';

const [port, unit] = process.argv.slice(2);
if (!port || !unit) {
  console.error('usage: node isolation_check.mjs <cdp-port> <browser-unit>');
  process.exit(2);
}

const hypr = (what) =>
  JSON.parse(execFileSync('hyprctl', ['-j', what], { encoding: 'utf8' }));
const snapshot = () => {
  const ws = hypr('activeworkspace');
  const win = hypr('activewindow');
  return {
    workspace: ws.id,
    window: win.address ?? null,
    monitors: hypr('monitors').map((m) => `${m.name}:${m.activeWorkspace.id}`),
    clients: hypr('clients').map((c) => ({ address: c.address, pid: c.pid })),
  };
};

const unitPids = () => {
  const cg = execFileSync(
    'systemctl', ['--user', 'show', '-P', 'ControlGroup', unit],
    { encoding: 'utf8' },
  ).trim();
  const procs = readFileSync(`/sys/fs/cgroup${cg}/cgroup.procs`, 'utf8');
  return new Set(procs.split('\n').filter(Boolean).map(Number));
};

// Hyprland event stream, read-only.
const sig = process.env.HYPRLAND_INSTANCE_SIGNATURE ||
  hypr('instances')[0].instance;
const events = [];
const evsock = net.connect(
  `${process.env.XDG_RUNTIME_DIR}/hypr/${sig}/.socket2.sock`);
let buf = '';
evsock.on('data', (d) => {
  buf += d;
  const lines = buf.split('\n');
  buf = lines.pop();
  for (const l of lines) events.push({ t: Date.now(), l });
});

let seq = 0;
const pending = new Map();
const version = await (await fetch(`http://127.0.0.1:${port}/json/version`))
  .json();
const ws = new WebSocket(version.webSocketDebuggerUrl);
await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
  }
};
const created = new Set();
const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
  const id = ++seq;
  pending.set(id, { resolve, reject });
  ws.send(JSON.stringify({ id, method, params, sessionId }));
}).then((r) => {
  if (method === 'Target.createTarget') created.add(r.targetId);
  return r;
});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const attach = async (targetId) =>
  (await send('Target.attachToTarget', { targetId, flatten: true })).sessionId;

const before = snapshot();
const samples = [];
const sampler = setInterval(() => {
  const s = snapshot();
  samples.push({ workspace: s.workspace, window: s.window });
}, 100);
const log = [];
const step = async (name, fn) => { await fn(); log.push(name); await sleep(700); };

const page = 'data:text/html,<title>agent-display isolation</title>' +
  '<h1>agent display</h1><textarea id=t autofocus></textarea>';
let first;
await step('new tab (createTarget)', async () => {
  first = (await send('Target.createTarget', { url: page })).targetId;
});
await step('3 more tabs', async () => {
  for (const u of ['https://example.com/', 'about:blank', page])
    await send('Target.createTarget', { url: u });
});
let win;
await step('new window (newWindow: true)', async () => {
  win = (await send('Target.createTarget',
    { url: 'https://example.org/', newWindow: true })).targetId;
});
const s1 = await attach(first);
await send('Page.enable', {}, s1);
await step('navigate tab', () =>
  send('Page.navigate', { url: 'https://example.com/?nav=1' }, s1));
await step('navigate back to test page', () =>
  send('Page.navigate', { url: page }, s1));
await step('Target.activateTarget + Page.bringToFront', async () => {
  await send('Target.activateTarget', { targetId: first });
  await send('Page.bringToFront', {}, s1);
});
await step('window.open popup (user gesture)', () =>
  send('Runtime.evaluate', {
    expression: "window.open('https://example.net/', 'oauth', " +
      "'popup,width=500,height=600'); 1",
    userGesture: true,
  }, s1));
await step('window.open second popup + focus()', () =>
  send('Runtime.evaluate', {
    expression: "const w = window.open('about:blank', 'p2', " +
      "'popup,width=400,height=400'); w.focus(); window.focus(); 1",
    userGesture: true,
  }, s1));
await step('activate the separate window', async () => {
  await send('Target.activateTarget', { targetId: win });
});
await step('activate first tab again', () =>
  send('Target.activateTarget', { targetId: first }));
await sleep(1500);
clearInterval(sampler);
const after = snapshot();
const targets = (await send('Target.getTargets')).targetInfos
  .filter((t) => t.type === 'page');
// The browser is shared: close only what this check opened (its targets
// and the popups they opened), never other agents' tabs.
for (let grew = true; grew;) {
  grew = false;
  for (const t of targets) {
    if (!created.has(t.targetId) && created.has(t.openerId)) {
      created.add(t.targetId);
      grew = true;
    }
  }
}
for (const t of targets) {
  if (created.has(t.targetId))
    await send('Target.closeTarget', { targetId: t.targetId }).catch(() => {});
}
ws.close();
evsock.end();

const pids = unitPids();
const leaked = after.clients.filter((c) => pids.has(c.pid));
const changed = samples.filter(
  (s) => s.workspace !== before.workspace || s.window !== before.window);
const browserEvents = events.filter((e) =>
  /^(openwindow|activewindow|urgent|focusedmon|workspace)/.test(e.l));
const result = {
  before: { workspace: before.workspace, window: before.window,
    monitors: before.monitors, clients: before.clients.length },
  after: { workspace: after.workspace, window: after.window,
    monitors: after.monitors, clients: after.clients.length },
  samples: samples.length,
  samplesDiffering: changed.length,
  browserPageTargets: targets.length,
  closedOwnTargets: targets.filter((t) => created.has(t.targetId)).length,
  browserWindowsInHyprland: leaked.length,
  hyprlandFocusEvents: browserEvents.map((e) => e.l),
  steps: log,
};
console.log(JSON.stringify(result, null, 2));
const ok = before.workspace === after.workspace &&
  before.window === after.window &&
  before.monitors.join() === after.monitors.join() &&
  changed.length === 0 && leaked.length === 0 &&
  after.clients.length === before.clients.length;
console.log(ok ? 'PASS: Hyprland untouched' : 'FAIL');
process.exit(ok ? 0 : 1);
