"""Write the src/cli_text.rs entries for commands the Rust port owns.

usage: python3 -I tools/gen_cli_text_rust.py [--check] src/cli_text.rs

Entries from the Python reference (tools/gen_cli_text.py) stay as they are.
Everything added or changed in Rust since (`tab` and all its subcommands,
`windows`, `browser start`, `view`) is described here with argparse, so
usage and help wrap and read exactly like the generated ones (80 columns).
Each entry is replaced in place, or inserted after its parent's last
existing child. --check: exit 1 if the file is not up to date.

Keep option order the same as in src/cli.rs `build()`; cli.rs tests check
every parser has an entry naming all its options.
"""
import argparse, json, os, sys

os.environ['COLUMNS'] = '80'
os.environ['NO_COLOR'] = '1'

PROG = 'agent-display'
PORT_HELP = 'CDP port from `browser list` (default: the browser that has TAB, or the persistent one)'
TAB_HELP = 'target id from `tab open`'
SHOW_URL_HELP = 'print URLs in full (default: query and fragment values, userinfo and token-like path segments are shown as …)'
TIMEOUT_STEP = 'seconds per CDP step (default 30)'
TIMEOUT_ELEMENT = 'seconds to wait for the element to be ready (default 10)'


def parser(prog, raw=False, **kw):
    fmt = argparse.RawDescriptionHelpFormatter if raw else argparse.HelpFormatter
    return argparse.ArgumentParser(prog=prog, formatter_class=fmt, **kw)


def tab_cmd(name, tab, timeout_help=TIMEOUT_STEP, timeout_metavar=None, raw=False, **kw):
    p = parser(f'{PROG} tab {name}', raw=raw, **kw)
    p.add_argument('--port', type=int, help=PORT_HELP)
    p.add_argument('--timeout', type=float, metavar=timeout_metavar, help=timeout_help)
    if tab:
        p.add_argument('tab', help=TAB_HELP)
    return p


def show_url(p):
    p.add_argument('--show-url', action='store_true', help=SHOW_URL_HELP)


def locator(p, text):
    p.add_argument('--css', metavar='SELECTOR',
                   help="CSS selector, run in every frame's document (not inside shadow roots: use --label or --role there)")
    p.add_argument('--label', metavar='TEXT',
                   help='field by its label, aria-label, placeholder or title (exact, then ignoring case and spaces, then containing)')
    p.add_argument('--role', help='ARIA role (textbox, button, link, checkbox, ...)')
    p.add_argument('--name', metavar='TEXT', help='with --role: accessible name, matched like --label')
    if text:
        p.add_argument('--text', metavar='TEXT', help='button, link or other clickable by its text, matched like --label')
    p.add_argument('--ref', help='element ref from `tab snapshot` (e.g. k3fq.12)')
    p.add_argument('--focused', action='store_true', help='the element that has focus')
    p.add_argument('--nth', type=int, metavar='N', help='pick the Nth visible match in page order (1 = first)')


entries = []

# ------------------------------------------------------------ tab

TAB_EXIT = """exit status (all tab commands):
  0   ok
  1   error (refusal, bad tab id, CDP error)
  2   usage error
  3   timed out (tab show/wait --timeout)
  4   tab gone: closed or crashed (checked twice, ~250 ms apart)
  5   browser down: no agent-display browser running (on that port)
  6   verification mismatch (tab type: the field's length differs)
  75  no definitive answer (browser stalled or restarting): try again"""

tab = parser(f'{PROG} tab', raw=True, epilog=TAB_EXIT)
sub = tab.add_subparsers(dest='tab_command', required=True)
for name, help_ in [
    ('open', 'new background tab; prints its id'),
    ('goto', 'navigate and wait for load'),
    ('eval', 'evaluate JS, print the JSON value'),
    ('shot', 'PNG screenshot of the tab'),
    ('info', 'url and title'),
    ('close', 'close the tab'),
    ('list', 'all tabs (ids, urls, titles)'),
    ('show', "let a human see and use this tab's window over VNC"),
    ('unshow', 'stop showing the tab'),
    ('wait', 'wait until the tab reaches a state (URL, text, ...)'),
    ('type', 'type into a field; --stdin for secrets'),
    ('click', 'click an element'),
    ('snapshot', 'accessibility outline with refs; no field values'),
]:
    sub.add_parser(name, help=help_)
entries.append(tab)

p = tab_cmd('open', False)
p.add_argument('url', nargs='?', help='URL to load; - reads it from stdin')
show_url(p)
entries.append(p)

p = tab_cmd('goto', True)
p.add_argument('url', help='URL; - reads it from stdin (keeps one-time links out of argv)')
g = p.add_mutually_exclusive_group()
g.add_argument('--wait-load', action='store_true', help='wait for the load event (default)')
g.add_argument('--wait-ms', type=int, metavar='N', help='wait N ms after navigating instead of the load event')
show_url(p)
entries.append(p)

p = tab_cmd('info', True)
show_url(p)
entries.append(p)

p = tab_cmd('list', False)
show_url(p)
entries.append(p)

p = parser(f'{PROG} tab show', raw=True, description="""\
Serve only this tab's window (its own output) over VNC on a Unix socket in
the private runtime dir, for a human to look in or finish a sign-in. Prints
{"shown": TAB, "viewer_command": "vncviewer SOCKET", ...}. The viewer stops
by itself when the human disconnects or after --timeout.

Without --wait-viewer/--until-url/--until-closed it returns at once.
With them it stays in the foreground, prints {"viewer": "connected"} and
{"viewer": "disconnected"} lines, and ends the view (printing
{"unshown": TAB, "reason": ...}) when the condition is met. With
--until-url/--until-closed it keeps watching the tab after the viewer
leaves (a sign-in can finish on the human's phone) until the condition,
--timeout or the tab going away.

A waiting show of a tab already shown by a show that returned at once
takes that viewer over and ends it. If another waiting show owns the
viewer, it is left up: the last line is then
{"done": TAB, "reason": ..., "still_shown": true}; end it with
`agent-display tab unshow TAB`.""", epilog="""\
exit status: 0 the condition was met (url-matched, closed; with --wait-viewer
alone: the viewer disconnected), 3 --timeout first, 4 tab gone, 5 browser
down, 75 try again, 1 other errors.""")
p.add_argument('--port', type=int, help=PORT_HELP)
p.add_argument('--timeout', type=float, metavar='SECONDS', help='end the view after this long (default 900)')
p.add_argument('--view-only', action='store_true', help='the viewer can look but not click or type')
p.add_argument('--until-url', metavar='REGEX',
               help="end the view once the tab's URL matches REGEX (e.g. the page after a sign-in); the URL is never printed")
p.add_argument('--until-closed', action='store_true', help='end the view (exit 0) when the tab closes')
p.add_argument('--wait-viewer', action='store_true',
               help='stay in the foreground and print viewer events until the viewer disconnects')
p.add_argument('tab', help=TAB_HELP)
entries.append(p)

p = parser(f'{PROG} tab unshow', description="Stop the tab's viewer (if any). Exits 0 whether or not one was shown; "
                                             '75 if the viewer could not be stopped (try again).')
p.add_argument('tab', help=TAB_HELP)
entries.append(p)

p = tab_cmd('wait', True, raw=True,
            timeout_help='seconds for the whole wait, stalls included (default 30; 0: no limit); returns within it + 1 s',
            description="""\
Wait until TAB reaches a state, driven by page events (no polling). All
conditions must hold (--any: one). RE is a JavaScript regex, `RE` or
`/RE/flags`. Prints one JSON line when done; --follow also prints a line per
state change, stall and dialog. Lines carry the URL (redacted like every tab
command's unless --show-url) and title, never the page's text or dialog
messages.""", epilog="""\
exit status:
  0   matched (with --gone: the tab or its browser went away)
  1   error: refused --port, not a page, invalid regex, the condition threw
  2   usage error (also: no condition, --gone with page conditions and no
      --any, a negative --timeout or --stable-ms, --tick-ms <= 0)
  3   timeout: the page answered and the condition never held
  4   the tab was closed or crashed (checked twice, ~250 ms apart)
  5   the browser went down (unregistered or its unit confirmed stopped)
  75  the deadline passed while the browser was stalled: try again

A stalled browser (no answer, a lookup without a definitive answer) is
reported in --follow as "stall"/"resumed" and retried; it never ends the
wait early. See docs/tab-wait.md.""")
c = p.add_argument_group('conditions')
c.add_argument('--url', metavar='RE', help='location.href matches')
c.add_argument('--url-not', metavar='RE', help='location.href does not match')
c.add_argument('--title', metavar='RE', help='document.title matches')
c.add_argument('--text', metavar='RE', help="the body's innerText matches")
c.add_argument('--text-not', metavar='RE', help="the body's innerText does not match")
c.add_argument('--selector', metavar='CSS', help='an element matches')
c.add_argument('--selector-not', metavar='CSS', help='no element matches')
c.add_argument('--js', metavar='EXPR', help='truthy; runs in an isolated world (DOM only, no page globals)')
c.add_argument('--gone', action='store_true', help='the tab is closed (with page conditions: needs --any)')
b = p.add_argument_group('behaviour')
b.add_argument('--any', action='store_true', help='any condition is enough (default: all)')
b.add_argument('--stable-ms', type=int, metavar='N', help='the match must hold N ms (redirect bounces; default 0)')
b.add_argument('--follow', action='store_true', help='print a JSON line per state change, stall and dialog')
b.add_argument('--tick-ms', type=int, metavar='N', help='re-check at least every N ms (default 1000)')
b.add_argument('--dialog', choices=['accept', 'dismiss'],
               help='answer alert/confirm/prompt (default dismiss); beforeunload is always accepted, so navigations go ahead')
show_url(b)
entries.append(p)

p = tab_cmd('type', True, timeout_help=TIMEOUT_ELEMENT,
            description='Type into one field. Secrets: pipe them with --stdin; the value is never printed, '
                        'and password and one-time-code fields refuse TEXT on the command line. Exit 6: the '
                        "field's length afterwards doesn't match what was typed (then --submit does not press Enter).")
p.add_argument('text', nargs='?', help='text to type (not for secrets: argv is visible to every process)')
locator(p, False)
p.add_argument('--stdin', action='store_true', help='read the text from stdin (one trailing newline is dropped)')
p.add_argument('--keys', action='store_true',
               help='send key presses instead of inserting text (split one-time-code boxes, key handlers)')
p.add_argument('--append', action='store_true', help='keep what the field already holds')
p.add_argument('--submit', action='store_true', help='press Enter afterwards (not after a length mismatch)')
p.add_argument('--keep-newline', action='store_true', help='with --stdin: keep a trailing newline')
entries.append(p)

p = tab_cmd('click', True, timeout_help=TIMEOUT_ELEMENT,
            description='Click one element once it is visible, still, enabled and not covered.')
locator(p, True)
p.add_argument('--count', type=int, default=1, metavar='N', help='1, 2 (double click) or 3')
entries.append(p)

p = tab_cmd('snapshot', True,
            description='Accessibility outline of the tab and its iframes, with refs for --ref. Fields show '
                        '[filled] or [empty], never their values.')
p.add_argument('--all', action='store_true', help='also print plain text nodes')
show_url(p)
entries.append(p)

# ------------------------------------------------------------ others

p = parser(f'{PROG} windows')
p.add_argument('--show-url', action='store_true',
               help='print window titles in full (default: URL-like titles, e.g. a browser tab whose page has no '
                    '<title>, are redacted like URLs)')
entries.append(p)

p = parser(f'{PROG} browser start')
p.add_argument('--temp', action='store_true', help='throwaway profile under /var/tmp, deleted on stop')
p.add_argument('--port', type=int, help='CDP port (default 9425; temp: auto)')
p.add_argument('--audio', action='store_true', help='do not mute')
p.add_argument('url', nargs='*', help='opened over CDP once the browser is up; - reads them from stdin, one per line')
entries.append(p)

p = parser(f'{PROG} view')
p.add_argument('--port', type=int)
p.add_argument('--view-only', action='store_true')
p.add_argument('--exit-on-disconnect', action='store_true', help='stop the viewer when the last client disconnects')
entries.append(p)


def line(p):
    # ensure_ascii=False: Rust string literals take UTF-8 (…), not \\uXXXX.
    usage, help_ = (json.dumps(t, ensure_ascii=False) for t in (p.format_usage(), p.format_help()))
    return f'    ({json.dumps(p.prog)}, {usage}, {help_}),'


def update(text):
    lines = text.split('\n')
    for p in entries:
        new = line(p)
        key = f'    ({json.dumps(p.prog)}, '
        idx = next((i for i, l in enumerate(lines) if l.startswith(key)), None)
        if idx is not None:
            lines[idx] = new
            continue
        parent = p.prog.rsplit(' ', 1)[0]
        # After the parent's last existing child (or the parent itself).
        last = max(i for i, l in enumerate(lines) if l.startswith(f'    ({json.dumps(parent)[:-1]}'))
        lines.insert(last + 1, new)
    return '\n'.join(lines)


args = sys.argv[1:]
check = '--check' in args
args = [a for a in args if a != '--check']
if len(args) != 1:
    sys.exit(__doc__.split('\n\n')[1])
path = args[0]
old = open(path).read()
new = update(old)
if check:
    if new != old:
        print(f'{path} is out of date: run python3 -I tools/gen_cli_text_rust.py {path}', file=sys.stderr)
        sys.exit(1)
elif new != old:
    open(path, 'w').write(new)
