"""Phase breakdown per tab command: Python (in-process, wrapped leaf ops, as
bench.py) vs Rust trace builds (rs0: port before fixes, rs: final).

usage: python3 -I phases.py N OUT.json
Each iteration (arm order rotates): open, goto, eval, shot, shot --full-page,
info, close on the shared browser (9425). Also measures startup baselines.
"""
import contextlib, functools, importlib.machinery, importlib.util, io, json, os, statistics, subprocess, sys, tempfile, time

N, OUT = int(sys.argv[1]), sys.argv[2]
PY = os.path.realpath(os.path.expanduser('~/.local/bin/agent-display'))
RS = {'rs0': '/var/tmp/ad-rs0-trace', 'rs': '/var/tmp/ad-rs-trace'}
URL = 'https://example.com/'
D = tempfile.mkdtemp(prefix='phases.', dir='/tmp')
STEPS = ['open', 'goto', 'eval', 'shot', 'shot --full-page', 'info', 'close']

l = importlib.machinery.SourceFileLoader('ad', PY)
spec = importlib.util.spec_from_loader('ad', l)
ad = importlib.util.module_from_spec(spec)
l.exec_module(ad)
cur = {'ph': None}


def rec(label, dt):
    cur['ph'].setdefault(label, []).append(dt * 1000)


def wrap(obj, name, label_fn):
    f = getattr(obj, name)

    @functools.wraps(f)
    def g(*a, **k):
        t = time.perf_counter()
        try:
            return f(*a, **k)
        finally:
            rec(label_fn(*a, **k), time.perf_counter() - t)
    setattr(obj, name, g)


def run_label(argv, *a, **k):
    tool = argv[0]
    return 'exec ss' if tool == 'ss' else f'exec {tool} {argv[2] if tool == "systemctl" else ""}'.strip()


wrap(ad, 'run', run_label)
wrap(ad, 'cdp_version', lambda *a, **k: 'HTTP /json/version')
wrap(ad, 'cdp_targets', lambda *a, **k: 'HTTP /json/list')
orig_call = ad.Cdp.call


def call(self, method, params=None, session=None, timeout=None):
    t = time.perf_counter()
    try:
        return orig_call(self, method, params, session, timeout)
    finally:
        rec(f'CDP {method}', time.perf_counter() - t)


ad.Cdp.call = call
orig_wait = ad.Cdp.wait_event


def wait_event(self, method, session=None, timeout=None):
    t = time.perf_counter()
    try:
        return orig_wait(self, method, session, timeout)
    finally:
        rec(f'wait {method}', time.perf_counter() - t)


ad.Cdp.wait_event = wait_event
orig_init = ad.Cdp.__init__


def init(self, port, timeout=10):
    n0 = sum(cur['ph'].get('HTTP /json/version', []))
    t = time.perf_counter()
    try:
        return orig_init(self, port, timeout)
    finally:
        nested = sum(cur['ph'].get('HTTP /json/version', [])) - n0
        rec('WS connect + handshake', time.perf_counter() - t - nested / 1000)


ad.Cdp.__init__ = init
orig_sleep = time.sleep


def sleep(s):
    t = time.perf_counter()
    orig_sleep(s)
    rec('sleep', time.perf_counter() - t)


ad.time.sleep = sleep


def py_cmd(*a):
    cur['ph'] = {}
    out = io.StringIO()
    t = time.perf_counter()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
        rc = ad.main(['tab', *a])
    total = (time.perf_counter() - t) * 1000
    assert rc == 0, a
    return out.getvalue(), {'total_ms': total, 'phases': cur['ph']}


def rs_cmd(arm, *a):
    t = time.perf_counter()
    p = subprocess.run([RS[arm], 'tab', *a], capture_output=True, text=True)
    wall = (time.perf_counter() - t) * 1000
    assert p.returncode == 0, (arm, a, p.stderr)
    tr = json.loads(next(l for l in p.stderr.splitlines() if l.startswith('TRACE '))[6:])
    tr['wall_ms'] = wall
    return p.stdout, tr


def cmd(arm, *a):
    return py_cmd(*a) if arm == 'py' else rs_cmd(arm, *a)


res = {a: {s: [] for s in STEPS} for a in ('py', 'rs0', 'rs')}
arms = ['py', 'rs0', 'rs']
for i in range(N):
    for arm in arms[i % 3:] + arms[:i % 3]:
        out, tr = cmd(arm, 'open')
        res[arm]['open'].append(tr)
        tab = json.loads(out)['tab']
        try:
            res[arm]['goto'].append(cmd(arm, 'goto', tab, URL)[1])
            res[arm]['eval'].append(cmd(arm, 'eval', tab, 'document.title')[1])
            res[arm]['shot'].append(cmd(arm, 'shot', tab, f'{D}/{arm}{i}.png')[1])
            res[arm]['shot --full-page'].append(cmd(arm, 'shot', tab, f'{D}/{arm}{i}f.png', '--full-page')[1])
            res[arm]['info'].append(cmd(arm, 'info', tab)[1])
        finally:
            res[arm]['close'].append(cmd(arm, 'close', tab)[1])
    print('iteration', i, flush=True)


def t_run(argv, n=30):
    r = []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run(argv, capture_output=True)
        r.append((time.perf_counter() - t) * 1000)
    return {'p50': statistics.median(r), 'p90': sorted(r)[int(0.9 * (n - 1))]}


load = (f'import importlib.machinery as m,importlib.util as u;l=m.SourceFileLoader("ad","{PY}");'
        's=u.spec_from_loader("ad",l);a=u.module_from_spec(s);l.exec_module(a)')
res['startup'] = {
    'python3 -c pass': t_run([sys.executable, '-c', 'pass']),
    'python3 + load agent-display': t_run([sys.executable, '-c', load]),
    'py: tab info <invalid id> (parse, fail before I/O)': t_run([PY, 'tab', 'info', 'bad!']),
    'rs: tab info <invalid id> (parse, fail before I/O)': t_run(['/var/tmp/ad-rs-final', 'tab', 'info', 'bad!']),
    'py: tab -h': t_run([PY, 'tab', '-h']),
    'rs: tab -h': t_run(['/var/tmp/ad-rs-final', 'tab', '-h']),
    '/usr/bin/true': t_run(['/usr/bin/true']),
    'systemctl --user is-active (1 call)': t_run(['systemctl', '--user', 'is-active', 'agent-display-browser.service']),
    'systemctl --user show 4 props (1 call)': t_run(['systemctl', '--user', 'show', '-p', 'Id', '-p', 'ActiveState', '-p', 'ControlGroup', '-p', 'MainPID', 'agent-display-browser.service']),
    'ss -Hltnp (1 call)': t_run(['ss', '-Hltnp', 'sport = :9425']),
}
json.dump(res, open(OUT, 'w'))
print(json.dumps(res['startup'], indent=1))
