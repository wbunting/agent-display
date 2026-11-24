# shellcheck shell=bash
# Live end-to-end harness for agent-display that cannot touch a live one.
#
# agent-display uses fixed unit names (agent-display.service, ...) in the
# user's systemd manager. To test the real binary with its real unit names
# next to a live display, run everything in a *nested* `systemd --user`:
#
#   - one outer unit, $E2E_PREFIX-mgr.service (Delegate=yes), runs a private
#     user manager with XDG_RUNTIME_DIR=$E2E_ROOT/run and HOME=$E2E_ROOT/home;
#   - its unit path is a copy of a few stock targets/slices only, so none of
#     the user's enabled units (~/.config/systemd/user, /etc/systemd/user,
#     sockets.target.wants) start in it;
#   - `ad` runs the binary with that runtime dir and HOME: its systemctl and
#     systemd-run reach only the nested manager ($E2E_ROOT/run/systemd/private;
#     there is no bus socket there to fall back to), its display, sockets,
#     registrations and profiles live under $E2E_ROOT (temp profiles under
#     /var/tmp as usual).
#
# Stopping the outer unit kills everything (one cgroup). Use only temp
# browsers (`browser start --temp`): the persistent profile's port 9425
# belongs to the live browser and `browser start` would refuse it anyway.
#
#   . tests/e2e/lib.sh; e2e_up; ad start; ...; e2e_down

E2E_PREFIX=${E2E_PREFIX:-ad-e2e-spaces}
E2E_ROOT=${E2E_ROOT:-/tmp/$E2E_PREFIX}
E2E_MGR=$E2E_PREFIX-mgr.service
AD_BIN=${AD_BIN:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/release/agent-display}

e2e_env() {
    env -i HOME="$E2E_ROOT/home" USER="$USER" LOGNAME="$USER" LANG=C.UTF-8 \
        PATH=/usr/local/bin:/usr/bin XDG_RUNTIME_DIR="$E2E_ROOT/run" "$@"
}

# agent-display against the nested manager.
ad() { e2e_env "$AD_BIN" "$@"; }
# systemctl --user of the nested manager.
nsys() { e2e_env systemctl --user "$@"; }
# swaymsg on the nested display.
nsway() {
    local sock
    sock=$(sed -n 's/^SWAYSOCK=//p' "$E2E_ROOT/run/agent-display/env")
    swaymsg -s "$sock" "$@"
}

e2e_up() {
    if systemctl --user is-active -q "$E2E_MGR"; then
        echo "$E2E_MGR is already running (another run?)" >&2
        return 1
    fi
    [ -x "$AD_BIN" ] || { echo "build first: cargo build --release" >&2; return 1; }
    e2e_rm_root
    mkdir -p -m 700 "$E2E_ROOT" "$E2E_ROOT/run" "$E2E_ROOT/home" "$E2E_ROOT/units"
    local u
    for u in app.slice background.slice session.slice basic.target default.target exit.target \
        shutdown.target sockets.target timers.target paths.target systemd-exit.service; do
        cp "/usr/lib/systemd/user/$u" "$E2E_ROOT/units/"
    done
    systemd-run --user --quiet --collect --unit="$E2E_MGR" -p Delegate=yes \
        -p "Description=agent-display e2e nested user manager ($E2E_ROOT)" \
        /usr/bin/env -i HOME="$E2E_ROOT/home" USER="$USER" LOGNAME="$USER" LANG=C.UTF-8 \
        PATH=/usr/local/bin:/usr/bin XDG_RUNTIME_DIR="$E2E_ROOT/run" SYSTEMD_UNIT_PATH="$E2E_ROOT/units" \
        /usr/lib/systemd/systemd --user
    local i
    for i in $(seq 100); do
        [ -S "$E2E_ROOT/run/systemd/private" ] \
            && [ "$(nsys show -p ActiveState --value default.target 2>/dev/null)" = active ] && break
        sleep 0.1
    done
    # Prove we talk to the nested manager, not the user's: its root slice
    # lives inside the outer unit's cgroup.
    local cg
    cg=$(nsys show -p ControlGroup --value -- -.slice 2>/dev/null)
    case "$cg" in
        *"/$E2E_MGR") ;;
        *) echo "nested manager not reachable or not nested: '$cg'" >&2; e2e_down; return 1 ;;
    esac
}

e2e_rm_root() {
    [ -e "$E2E_ROOT" ] || return 0
    # systemd leaves mode-0 "inaccessible" nodes in its runtime dir.
    find "$E2E_ROOT" -type d -exec chmod u+rwx {} + 2>/dev/null
    rm -rf "$E2E_ROOT"
}

e2e_down() {
    systemctl --user stop "$E2E_MGR" 2>/dev/null
    local i
    for i in $(seq 50); do
        systemctl --user is-active -q "$E2E_MGR" || break
        sleep 0.1
    done
    e2e_rm_root
}
