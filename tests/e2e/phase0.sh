#!/usr/bin/env bash
# Live end-to-end checks for tab lookup, exit codes, the output gc guard and
# `tab show`, in a nested user manager that cannot reach a live
# agent-display (see lib.sh). Opt-in; needs sway, chromium, wayvnc, python3,
# jq. Prints a transcript and PASS/FAIL per check; evidence (frames, logs)
# goes to $E2E_OUT.
#
#   cargo build --release && tests/e2e/phase0.sh [CHURN_SECONDS]
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
CHURN=${1:-60}
E2E_OUT=${E2E_OUT:-/tmp/$E2E_PREFIX-evidence}
mkdir -p -m 700 "$E2E_OUT"
OLD_BIN=${OLD_BIN:-}   # optional: an older agent-display to compare the gc against

fails=0
check() { # check NAME CONDITION...
    local name=$1; shift
    if "$@"; then echo "PASS  $name"; else echo "FAIL  $name"; fails=$((fails + 1)); fi
}
run() { # run CMD...: echo it, its output and exit status; status in $rc
    echo "\$ $*"
    "$@"
    rc=$?
    echo "[exit $rc]"
}
live_units() {
    systemctl --user show -p Id -p MainPID -p ActiveEnterTimestamp \
        agent-display.service agent-display-browser.service agent-display-vnc.service 2>/dev/null
}
# TCP sockets in LISTEN held by any process of a nested unit.
unit_tcp_listeners() {
    local cg p n=0
    cg=$(nsys show -p ControlGroup --value "$1")
    [ -n "$cg" ] || { echo 0; return; }
    for p in $(cat "/sys/fs/cgroup$cg/cgroup.procs" 2>/dev/null); do
        n=$((n + $(ss -Hltnp | grep -c "pid=$p,")))
    done
    echo "$n"
}
not() { ! "$@"; }
has_output() { nsway -t get_outputs -r | jq -e --arg o "$1" 'any(.[]; .name == $o)' >/dev/null; }
active_viewers() { nsys list-units --plain --no-legend --state=active,activating,deactivating 'agent-display-vnc*' | grep -c .; }
free_port() { python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

BPID=
cleanup() {
    [ -n "$BPID" ] && kill -CONT "$BPID" 2>/dev/null
    e2e_down
}
trap cleanup EXIT

echo "== live agent-display units before (must be unchanged after)"
live_units | tee "$E2E_OUT/live-before.txt"

echo "== nested manager"
e2e_up || exit 1
echo "nested root slice: $(nsys show -p ControlGroup --value -- -.slice)"
run ad start
BJ=$(ad browser start --temp); echo "$BJ"
P=$(jq -r .port <<<"$BJ"); PROFILE=$(jq -r .profile <<<"$BJ")
BUNIT=$(ad browser list | jq -r '.[0].unit')
check "temp browser on an auto port, not 9425" test "$P" != 9425 -a -n "$P"

# Two local sites (127.0.0.1 and localhost are different sites: navigating
# between them swaps the main frame, the case that hid tabs from /json/list).
mkdir -p "$E2E_ROOT/www"
printf '<title>site</title><h1>site</h1>' > "$E2E_ROOT/www/index.html"
HP=$(free_port)
e2e_env systemd-run --user --quiet --collect --unit=e2e-www -- \
    /usr/bin/python3 -I -m http.server --bind 127.0.0.1 --directory "$E2E_ROOT/www" "$HP" >/dev/null
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$HP/" >/dev/null && break; sleep 0.1; done

echo
echo "== 1. lookup under concurrent tab churn (${CHURN}s, 4 churners, victim navigating cross-site)"
VICTIM=$(ad tab open --port "$P" "http://127.0.0.1:$HP/" | jq -r .tab)
echo "victim $VICTIM"
churn() {
    local end=$((SECONDS + CHURN)) n=0 t
    while ((SECONDS < end)); do
        t=$(ad tab open --port "$P" 2>/dev/null | jq -r .tab) || continue
        ad tab close "$t" >/dev/null 2>&1 && n=$((n + 1))
    done
    echo "$n" > "$E2E_OUT/churn.$1"
}
navigate() {
    local end=$((SECONDS + CHURN)) n=0 url
    while ((SECONDS < end)); do
        if ((n % 2)); then url="http://localhost:$HP/?n=$n"; else url="http://127.0.0.1:$HP/?n=$n"; fi
        ad tab goto "$VICTIM" "$url" >/dev/null 2>&1
        n=$((n + 1))
    done
    echo "$n" > "$E2E_OUT/navigations"
}
poll() {
    local end=$((SECONDS + CHURN)) code
    : > "$E2E_OUT/info-codes"; : > "$E2E_OUT/list-hits"
    while ((SECONDS < end)); do
        ad tab info "$VICTIM" >/dev/null 2>>"$E2E_OUT/info-stderr"
        echo $? >> "$E2E_OUT/info-codes"
        if ad tab list --port "$P" 2>/dev/null | jq -e --arg t "$VICTIM" 'any(.tabs[]; .tab == $t)' >/dev/null; then
            echo 1 >> "$E2E_OUT/list-hits"
        else
            echo 0 >> "$E2E_OUT/list-hits"
        fi
    done
}
for i in 1 2 3 4; do churn "$i" & done
navigate & poll & wait
opens=$(awk '{s+=$1} END {print s}' "$E2E_OUT"/churn.*)
polls=$(wc -l < "$E2E_OUT/info-codes")
echo "churn open+close cycles: $opens; victim navigations: $(cat "$E2E_OUT/navigations"); tab info polls: $polls"
echo "tab info exit codes: $(sort "$E2E_OUT/info-codes" | uniq -c | tr '\n' ' ')"
echo "tab list polls missing the live victim: $(grep -c '^0' "$E2E_OUT/list-hits") of $(wc -l < "$E2E_OUT/list-hits")"
[ -s "$E2E_OUT/info-stderr" ] && { echo "tab info stderr (unique):"; sort "$E2E_OUT/info-stderr" | uniq -c | head; }
check "no tab info said gone/down/unavailable for a live tab" \
    test "$(grep -cvE '^(0|1)$' "$E2E_OUT/info-codes")" -eq 0
check "tab info found the live tab in every poll" test "$(grep -cv '^0$' "$E2E_OUT/info-codes")" -eq 0
check "churners cycled tabs" test "${opens:-0}" -gt 0
ad tab goto "$VICTIM" "http://127.0.0.1:$HP/" >/dev/null

echo
echo "== 2. stalled browser (SIGSTOP of its main process)"
BPID=$(nsys show -p MainPID --value "$BUNIT.service")
kill -STOP "$BPID"; (sleep 3; kill -CONT "$BPID") &
s=$SECONDS; run ad tab info "$VICTIM"; wait
check "3 s stall ridden out (exit 0 after $((SECONDS - s)) s)" test "$rc" -eq 0
kill -STOP "$BPID"
s=$SECONDS; run ad tab info "$VICTIM" 2> "$E2E_OUT/stall.err"; cat "$E2E_OUT/stall.err"
el=$((SECONDS - s))
check "long stall: exit 75 after the ~15 s budget (${el} s)" test "$rc" -eq 75 -a "$el" -ge 12 -a "$el" -le 25
check "long stall: never says closed" not grep -qi closed "$E2E_OUT/stall.err"
s=$SECONDS; run ad tab info --port "$P" "$VICTIM" 2> "$E2E_OUT/stall-port.err"; cat "$E2E_OUT/stall-port.err"
check "long stall with --port: exit 75, not a refusal ($((SECONDS - s)) s)" bash -c "test $rc -eq 75 && ! grep -q refusing '$E2E_OUT/stall-port.err'"
s=$SECONDS; run ad tab open --port "$P" 2> "$E2E_OUT/stall-open.err"; cat "$E2E_OUT/stall-open.err"
check "tab open on a stalled browser: exit 75 ($((SECONDS - s)) s)" test "$rc" -eq 75
kill -CONT "$BPID"; BPID=
run ad tab info "$VICTIM"
check "after SIGCONT: found again" test "$rc" -eq 0

echo
echo "== 3. gc keeps the output of a live tab whose window is away"
con() { nsway -t get_tree -r | jq --arg o "$1" '[.nodes[] | select(.name == $o) | .. | objects | select(.pid? and .type == "con") | .id][0]'; }
away() { # open a tab, move its window off its output; prints "tab output con"
    local j t o c
    j=$(ad tab open --port "$P" "http://127.0.0.1:$HP/?gc")
    t=$(jq -r .tab <<<"$j"); o=$(jq -r .output <<<"$j"); c=$(con "$o")
    nsway "[con_id=$c] move container to output HEADLESS-1" >/dev/null
    echo "$t $o $c"
}
if [ -n "$OLD_BIN" ]; then
    echo "-- before: $OLD_BIN"
    read -r OT1 OO1 _ < <(away)
    echo "live tab $OT1: window moved off $OO1; outputs $(nsway -t get_outputs -r | jq -c '[.[].name]')"
    OT2=$(e2e_env "$OLD_BIN" tab open --port "$P" | jq -r .tab)
    echo "after another agent's tab open with the old binary: $(nsway -t get_outputs -r | jq -c '[.[].name]')"
    if has_output "$OO1"; then echo "old binary kept $OO1"; else echo "old binary UNPLUGGED $OO1 although tab $OT1 is alive"; fi
    ad tab close "$OT1" >/dev/null; ad tab close "$OT2" >/dev/null
    echo "-- after: this binary"
fi
read -r GT GO GC < <(away)
echo "live tab $GT: window (con $GC) moved off $GO; outputs $(nsway -t get_outputs -r | jq -c '[.[].name]')"
NT=$(ad tab open --port "$P" | jq -r .tab)
echo "after another agent's tab open (gc ran): $(nsway -t get_outputs -r | jq -c '[.[].name]')"
check "output $GO of the live tab kept" has_output "$GO"
check "its marker kept" test "$(cat "$E2E_ROOT/run/agent-display/tab-outputs/$GO" 2>/dev/null)" = "$GT"
nsway "[con_id=$GC] move container to output $GO" >/dev/null
run ad tab shot "$GT" "$E2E_OUT/gc-tab.png"
check "the tab still works on its output" test "$rc" -eq 0
ad tab close "$NT" >/dev/null
run ad tab close "$GT"
check "closing it removes the output" not has_output "$GO"

echo
echo "== 4. tab show: one tab to a human over VNC on a Unix socket"
PAGE='<title>handoff</title><body style="margin:0;background:#2a6;font:40px sans-serif"><input id=i style="font:40px sans-serif;width:900px;margin:8px" placeholder="type here"><p>agent A tab: human handoff test</p><script>let n=0;addEventListener("mousedown",()=>{n++});i.addEventListener("input",()=>{document.title="clicks="+n+" val="+i.value})</script>'
printf '%s' "$PAGE" > "$E2E_ROOT/www/handoff.html"
printf '<title>home</title><h1>signed in</h1>' > "$E2E_ROOT/www/home.html"
A=$(ad tab open --port "$P" "http://127.0.0.1:$HP/handoff.html" | jq -r .tab)
B=$(ad tab open --port "$P" 'data:text/html,<title>other agent</title><body style="background:%23d00">secret of agent B' | jq -r .tab)
AO=$(grep -lx "$A" "$E2E_ROOT"/run/agent-display/tab-outputs/* | xargs -n1 basename)
echo "tab A $A on $AO; tab B (red, another agent) $B"
ad tab show "$A" --wait-viewer --timeout 120 > "$E2E_OUT/show.jsonl" 2> "$E2E_OUT/show.err" &
SP=$!
for _ in $(seq 50); do [ -s "$E2E_OUT/show.jsonl" ] && break; sleep 0.1; done
SOCK=$(head -1 "$E2E_OUT/show.jsonl" | jq -r .socket)
echo "socket: $SOCK"; ls -l "$SOCK"
check "viewer listens on a Unix socket in the private runtime dir" test -S "$SOCK"
VUNIT=agent-display-vnc-$(tr 'A-Z' 'a-z' <<<"$AO").service
check "no TCP listener in $VUNIT" test "$(unit_tcp_listeners "$VUNIT")" -eq 0
run timeout 30 python3 -I "$HERE/rfb.py" "unix:$SOCK" "$E2E_OUT/show-frame.png" --click 400 146 --type 'hello' --hold 1
wait "$SP"; src=$?
echo "tab show output (exit $src):"; cat "$E2E_OUT/show.jsonl"; cat "$E2E_OUT/show.err"
check "show exited 0 on disconnect" test "$src" -eq 0
check "viewer connected event" grep -q '"viewer": "connected", "clients": 1' "$E2E_OUT/show.jsonl"
check "viewer disconnected event" grep -q '"viewer": "disconnected", "clients": 0' "$E2E_OUT/show.jsonl"
check "unshown: disconnected" grep -q '"reason": "disconnected"' "$E2E_OUT/show.jsonl"
run ad tab eval "$A" 'document.querySelector("#i").value + " | " + document.title'
check "the human's click and typing reached the page" test "$(ad tab eval "$A" 'document.querySelector("#i").value')" = '"hello"' 
python3 -I - "$E2E_OUT/show-frame-after.png" <<'PY' > "$E2E_OUT/frame-colors.json"
import json, struct, sys, zlib
d = open(sys.argv[1], 'rb').read()
w, h = struct.unpack('>II', d[16:24])
i, idat = 8, b''
while i < len(d):
    n, t = struct.unpack('>I4s', d[i:i + 8])
    if t == b'IDAT':
        idat += d[i + 8:i + 8 + n]
    i += 12 + n
raw = zlib.decompress(idat)
red = green = 0
for y in range(0, h, 4):
    row = raw[y * (w * 3 + 1) + 1:(y + 1) * (w * 3 + 1)]
    for x in range(0, w * 3, 12):
        r, g, b = row[x], row[x + 1], row[x + 2]
        red += r > 180 and g < 60 and b < 60
        green += abs(r - 0x22) < 20 and abs(g - 0xaa) < 20 and abs(b - 0x66) < 20
print(json.dumps({'width': w, 'height': h, 'green_samples': green, 'red_samples': red}))
PY
cat "$E2E_OUT/frame-colors.json"
check "frame is tab A's page (green)" test "$(jq .green_samples "$E2E_OUT/frame-colors.json")" -gt 10000
check "frame has nothing of tab B (red)" test "$(jq .red_samples "$E2E_OUT/frame-colors.json")" -eq 0
sleep 1
check "wayvnc exited, unit gone" test "$(active_viewers)" -eq 0
check "no socket left" test ! -e "$SOCK"
check "nothing listening on it" not grep -qF "$SOCK" <(ss -Hxl)

echo "-- show --until-url: ends the view once the page reaches the URL"
ad tab show "$A" --until-url '/home\.html' --timeout 60 > "$E2E_OUT/show2.jsonl" &
SP=$!
for _ in $(seq 50); do [ -s "$E2E_OUT/show2.jsonl" ] && break; sleep 0.1; done
SOCK=$(head -1 "$E2E_OUT/show2.jsonl" | jq -r .socket)
timeout 30 python3 -I "$HERE/rfb.py" "unix:$SOCK" "$E2E_OUT/show2-frame.png" --hold 6 > "$E2E_OUT/rfb2.out" 2>&1 &
RP=$!
sleep 2
# The page "signs in": redirect with a code in the URL.
ad tab eval "$A" 'location.href = "/home.html?code=SECRET123"' >/dev/null
wait "$SP"; src=$?
wait "$RP"
cat "$E2E_OUT/show2.jsonl"; echo "[exit $src]"
check "until-url: exit 0, reason url-matched" bash -c "test $src -eq 0 && grep -q '\"reason\": \"url-matched\"' '$E2E_OUT/show2.jsonl'"
check "until-url: viewer stopped (client cut off)" test "$(active_viewers)" -eq 0
check "until-url: the URL (with its code) was never printed" not grep -q SECRET123 "$E2E_OUT/show2.jsonl"

echo "-- show --until-url: the human leaves before the redirect (phone approval), it lands later"
ad tab goto "$A" "http://127.0.0.1:$HP/handoff.html" >/dev/null
ad tab show "$A" --until-url '/home\.html' --timeout 60 > "$E2E_OUT/show4.jsonl" &
SP=$!
for _ in $(seq 50); do [ -s "$E2E_OUT/show4.jsonl" ] && break; sleep 0.1; done
SOCK=$(head -1 "$E2E_OUT/show4.jsonl" | jq -r .socket)
timeout 30 python3 -I "$HERE/rfb.py" "unix:$SOCK" "$E2E_OUT/show4-frame.png" --hold 0.5 > /dev/null 2>&1
sleep 2
check "until-url: still waiting after the viewer left" kill -0 "$SP"
ad tab eval "$A" 'location.href = "/home.html?code=SECRET456"' >/dev/null
wait "$SP"; src=$?
cat "$E2E_OUT/show4.jsonl"; echo "[exit $src]"
check "until-url met after the viewer left: exit 0, url-matched" bash -c "test $src -eq 0 && grep -q '\"viewer\": \"stopped\"' '$E2E_OUT/show4.jsonl' && grep -q '\"reason\": \"url-matched\"' '$E2E_OUT/show4.jsonl'"
check "until-url: the URL (with its code) was never printed" not grep -q SECRET456 "$E2E_OUT/show4.jsonl"

echo "-- show --until-url: the human leaves and the URL never matches: exit 3"
ad tab goto "$A" "http://127.0.0.1:$HP/handoff.html" >/dev/null
s=$SECONDS
ad tab show "$A" --until-url '/home\.html' --timeout 5 > "$E2E_OUT/show5.jsonl" &
SP=$!
for _ in $(seq 50); do [ -s "$E2E_OUT/show5.jsonl" ] && break; sleep 0.1; done
SOCK=$(head -1 "$E2E_OUT/show5.jsonl" | jq -r .socket)
timeout 30 python3 -I "$HERE/rfb.py" "unix:$SOCK" "$E2E_OUT/show5-frame.png" --hold 0.5 > /dev/null 2>&1
wait "$SP"; src=$?
cat "$E2E_OUT/show5.jsonl"; echo "[exit $src after $((SECONDS - s)) s]"
check "until-url not met, viewer gone: exit 3, reason timeout (not disconnected/0)" bash -c "test $src -eq 3 && grep -q '\"reason\": \"timeout\"' '$E2E_OUT/show5.jsonl'"
check "no viewer left" test "$(active_viewers)" -eq 0

echo "-- show --timeout while a human is still connected: exit 3, viewer stopped"
s=$SECONDS
ad tab show "$A" --wait-viewer --timeout 4 > "$E2E_OUT/show3.jsonl" &
SP=$!
for _ in $(seq 50); do [ -s "$E2E_OUT/show3.jsonl" ] && break; sleep 0.1; done
SOCK=$(head -1 "$E2E_OUT/show3.jsonl" | jq -r .socket)
timeout 30 python3 -I "$HERE/rfb.py" "unix:$SOCK" "$E2E_OUT/show3-frame.png" --hold 15 > "$E2E_OUT/rfb3.out" 2>&1 &
RP=$!
wait "$SP"; src=$?; el=$((SECONDS - s))
wait "$RP"; rfb3=$?
cat "$E2E_OUT/show3.jsonl"; echo "[exit $src after ${el} s; viewer client exit $rfb3: $(tail -1 "$E2E_OUT/rfb3.out")]"
check "timeout: exit 3, reason timeout" bash -c "test $src -eq 3 && grep -q '\"reason\": \"timeout\"' '$E2E_OUT/show3.jsonl'"
check "timeout: within the --timeout (${el} s)" test "$el" -ge 3 -a "$el" -le 6
check "timeout: viewer stopped, the human's connection cut" bash -c "test $(active_viewers) -eq 0 && test $rfb3 -ne 0"

echo "-- reuse: a running interactive viewer can't be re-shown view-only"
run ad tab show "$A"
run ad tab show "$A" --view-only
check "different mode while shown: exit 1" test "$rc" -eq 1
check "the running viewer is left alone" test "$(active_viewers)" -eq 1
run ad tab unshow "$A"

echo "-- show (returns at once) + unshow"
run ad tab show "$A" --view-only
UNIT=$(nsys list-units --plain --no-legend 'agent-display-vnc*' | awk '{print $1}')
check "non-blocking show leaves the viewer running ($UNIT)" test -n "$UNIT"
run ad tab unshow "$A"
check "unshow stopped it" test "$(active_viewers)" -eq 0
run ad tab unshow "$A"

echo
echo "== 5. view --exit-on-disconnect (whole display, loopback TCP)"
VP=$(free_port)
run ad view --exit-on-disconnect --port "$VP"
run timeout 30 python3 -I "$HERE/rfb.py" "tcp:127.0.0.1:$VP" "$E2E_OUT/view-frame.png" --hold 0.5
sleep 1
check "view stopped by itself after the viewer left" test "$(nsys is-active agent-display-vnc.service)" != active
check "its port is free again" test -z "$(ss -Hltn "sport = :$VP")"

echo
echo "== 6. gone: a real tab close, then lookups say 4"
run ad tab close "$A"
s=$SECONDS; run ad tab info "$A"
check "closed tab: tab info exits 4" test "$rc" -eq 4
run ad tab close "$A"
check "closed tab: tab close exits 4" test "$rc" -eq 4

echo
echo "== 7. browser down: browser stop, then lookups say 5"
run ad browser stop "$BUNIT"
run ad tab info "$B"
check "tab of a stopped browser: exit 5" test "$rc" -eq 5
run ad tab open --port "$P"
check "tab open on the stopped browser's port: exit 5" test "$rc" -eq 5

echo
echo "== teardown"
run ad stop
e2e_down
trap - EXIT
left_units=$(systemctl --user list-units --all --plain --no-legend "$E2E_PREFIX*" | grep -c .)
left_procs=$(pgrep -fc "$E2E_ROOT/")
echo "leftovers: units $left_units, processes $left_procs, root $(test -e "$E2E_ROOT" && echo present || echo gone), temp profile $(test -e "$PROFILE" && echo present || echo gone)"
check "no units, processes or files left" test "$left_units" -eq 0 -a "$left_procs" -eq 0 -a ! -e "$E2E_ROOT" -a ! -e "$PROFILE"
echo "== live agent-display units after"
live_units | tee "$E2E_OUT/live-after.txt"
check "live agent-display units untouched (same pids, same start times)" cmp -s "$E2E_OUT/live-before.txt" "$E2E_OUT/live-after.txt"
echo
echo "$fails check(s) failed; evidence in $E2E_OUT"
exit $((fails > 0))
