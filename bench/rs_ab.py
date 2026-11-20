"""CLI-level A/B of `agent-display tab`: Python vs Rust, on the shared browser.

usage: python3 -I rs_ab.py ROUNDS TABS OUT.json
Arms (one process per step, as agents call it): py (installed Python), rs0
(Rust port before fixes: ss, 5 s), rs (Rust final). Arms alternate order each
round. Per round and arm:
  * TABS tabs: open, goto, eval(paint) -> shot -> check PNG, eval(paint) ->
    shot --full-page -> check, info, close
  * one full flow: open -> goto -> eval(paint) -> shot(check) -> close, timed
    end to end.
Tracks every tab id it opens; snapshots /json/list before and after.
"""
import json, os, random, statistics, struct, subprocess, sys, tempfile, time, urllib.request, zlib

ARMS = {'py': [os.path.expanduser('~/.local/bin/agent-display')],
        'rs0': ['/var/tmp/ad-rs-port'],
        'rs': ['/var/tmp/ad-rs-final']}
ROUNDS, TABS, OUT = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
URL = 'https://example.com/'
D = tempfile.mkdtemp(prefix='rsab.', dir='/tmp')
os.chmod(D, 0o700)
opened, closed = set(), set()


def run(arm, *a, timeout=120):
    t = time.perf_counter()
    try:
        p = subprocess.run([*ARMS[arm], 'tab', *a], capture_output=True, text=True, timeout=timeout)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        rc, out, err = 124, '', 'harness timeout'
    return round((time.perf_counter() - t) * 1000, 2), rc, out, err


def first_pixel(path):
    data = open(path, 'rb').read()
    pos, idat = 8, b''
    while pos < len(data):
        n = struct.unpack('>I', data[pos:pos + 4])[0]
        typ = data[pos + 4:pos + 8]
        if typ == b'IDAT':
            idat += data[pos + 8:pos + 8 + n]
        elif typ == b'IEND':
            break
        pos += 12 + n
    return tuple(zlib.decompressobj().decompress(idat, 8)[1:4])


def targets():
    with urllib.request.urlopen('http://127.0.0.1:9425/json/list', timeout=30) as r:
        return {t['id']: t['type'] for t in json.load(r)}


def paint(arm, rec, tab):
    rgb = tuple(random.randint(0, 255) for _ in range(3))
    js = ("for (const e of [document.documentElement, document.body]) "
          f"e.style.setProperty('background', 'rgb{rgb}', 'important'); 1")
    ms, rc, _, err = run(arm, 'eval', tab, js)
    rec['eval'].append(ms)
    if rc:
        rec['errors'].append(('eval', err.strip()[:200]))
        return None
    return rgb


def shot(arm, rec, key, tab, rgb, *extra):
    path = os.path.join(D, f'{arm}.png')
    if os.path.exists(path):
        os.unlink(path)
    ms, rc, out, err = run(arm, 'shot', tab, path, *extra)
    if rc:
        rec['errors'].append((key, err.strip()[:200]))
        rec['fresh'].append(False)
        return False
    rec[key].append(ms)
    got = first_pixel(out.strip())
    ok = all(abs(a - b) <= 3 for a, b in zip(got, rgb))
    rec['fresh'].append(ok)
    return ok


def open_tab(arm, rec):
    ms, rc, out, err = run(arm, 'open')
    if rc:
        rec['errors'].append(('open', err.strip()[:200]))
        return None, ms
    tab = json.loads(out)['tab']
    opened.add(tab)
    rec['open'].append(ms)
    return tab, ms


def close_tab(arm, rec, tab):
    ms, rc, _, err = run(arm, 'close', tab)
    if rc:
        rec['errors'].append(('close', err.strip()[:200]))
        # never leave our tab behind: retry with the final Rust build
        for _ in range(5):
            if run('rs', 'close', tab)[1] == 0:
                break
            time.sleep(2)
    else:
        rec['close'].append(ms)
    closed.add(tab)
    return ms


def one_round(arm):
    rec = {k: [] for k in ('open', 'goto', 'eval', 'shot', 'shot --full-page', 'info', 'close', 'flow', 'fresh', 'errors')}
    for _ in range(TABS):
        tab, _ = open_tab(arm, rec)
        if not tab:
            continue
        try:
            ms, rc, _, err = run(arm, 'goto', tab, URL)
            (rec['goto'].append(ms) if not rc else rec['errors'].append(('goto', err.strip()[:200])))
            rgb = paint(arm, rec, tab)
            if rgb:
                shot(arm, rec, 'shot', tab, rgb)
            rgb = paint(arm, rec, tab)
            if rgb:
                shot(arm, rec, 'shot --full-page', tab, rgb, '--full-page')
            ms, rc, _, err = run(arm, 'info', tab)
            (rec['info'].append(ms) if not rc else rec['errors'].append(('info', err.strip()[:200])))
        finally:
            close_tab(arm, rec, tab)
    # full flow, end to end
    t0 = time.perf_counter()
    tab, _ = open_tab(arm, rec)
    ok = tab is not None
    if tab:
        try:
            ok &= run(arm, 'goto', tab, URL)[1] == 0
            rgb = paint(arm, rec, tab)
            ok &= bool(rgb) and shot(arm, rec, 'shot', tab, rgb)
        finally:
            close_tab(arm, rec, tab)
    if ok:
        rec['flow'].append(round((time.perf_counter() - t0) * 1000, 2))
    return rec


before = targets()
res = {'rounds': ROUNDS, 'tabs': TABS, 'arms': {a: [] for a in ARMS}, 'started': time.time()}
order = list(ARMS)
for r in range(ROUNDS):
    rot = order[r % 3:] + order[:r % 3]
    for arm in rot:
        rec = one_round(arm)
        res['arms'][arm].append(rec)
        med = lambda k: statistics.median(rec[k]) if rec[k] else None
        print(f"round {r} {arm}: open {med('open')} shot {med('shot')} flow {med('flow')} "
              f"fresh {sum(rec['fresh'])}/{len(rec['fresh'])} errors {len(rec['errors'])}", flush=True)
    json.dump(res, open(OUT, 'w'))
after = targets()
mine_left = sorted(t for t in opened if t in after)
res['tabs_check'] = {
    'opened': len(opened), 'closed': len(closed), 'mine_left_open': mine_left,
    'pages_before': sorted(k for k, v in before.items() if v == 'page'),
    'pages_after': sorted(k for k, v in after.items() if v == 'page'),
    'foreign_gone': sorted(k for k in before if k not in after),
    'foreign_new': sorted(k for k in after if k not in before and k not in opened),
}
json.dump(res, open(OUT, 'w'))
print(json.dumps(res['tabs_check'], indent=1))
