#!/bin/sh
# Install-test the neewer-bridge Debian package on a machine running systemd.
#
#   sudo packaging/test-deb.sh <package.deb> [<newer-package.deb>]
#
# Installs the package and checks the whole life cycle against the real
# systemd and dpkg: the config created from the example, the service enabled
# and started, the journal format, a clean stop on SIGTERM, a configuration
# error that fails once instead of restarting for ever, a crash that is
# restarted, an upgrade (to the second package when given, else a reinstall)
# that keeps the admin's edited config and respects a disabled unit, then
# remove and purge. Run by the release workflow before anything is published.
#
# It purges the package at the end, so it refuses to run where neewer-bridge
# is already installed. It needs no Bluetooth adapter: without one the service
# waits for one, which is a state it checks for too.
set -eu

DEB=${1:?usage: test-deb.sh <package.deb> [<newer-package.deb>]}
UPGRADE=${2:-$1}
UNIT=neewer-bridge
CONF=/etc/neewer-bridge/config.toml
EXAMPLE=/etc/neewer-bridge/config.example.toml
export DEBIAN_FRONTEND=noninteractive

fail() {
    echo "FAIL: $*" >&2
    echo "--- systemctl status:" >&2
    systemctl status "$UNIT" --no-pager -l >&2 || true
    echo "--- journal (last 40 lines):" >&2
    journalctl -u "$UNIT" --no-pager -n 40 >&2 || true
    exit 1
}
ok() { echo "ok - $*"; }
prop() { systemctl show -p "$1" --value "$UNIT"; }
apt_install() { apt-get install -y -q -o Dpkg::Options::=--force-confold "$@" >/dev/null; }
# Wait up to $1 seconds for `systemctl is-active` to say $2.
wait_state() {
    i=0
    while [ "$(systemctl is-active "$UNIT" || true)" != "$2" ]; do
        i=$((i + 1))
        [ "$i" -le "$1" ] || return 1
        sleep 1
    done
}
# This run's journal: the bridge's own lines only (systemd's "Started …" lines
# carry UNIT=, not _SYSTEMD_UNIT=).
journal() { journalctl "_SYSTEMD_UNIT=$UNIT.service" --since "@$T0" --no-pager -o cat "$@"; }

[ "$(id -u)" = 0 ] || fail "run as root"
[ -d /run/systemd/system ] || fail "needs systemd as the init system"
for f in "$DEB" "$UPGRADE"; do [ -f "$f" ] || fail "no such package: $f"; done
if dpkg-query -W -f '${Status}' "$UNIT" 2>/dev/null | grep -q installed; then
    fail "neewer-bridge is already installed here; this test purges it at the end"
fi
DEB=$(readlink -f "$DEB")
UPGRADE=$(readlink -f "$UPGRADE")
T0=$(date +%s)

# --- install ------------------------------------------------------------------
apt_install "$DEB" || fail "installing $DEB"
dpkg-query -W -f '${Status}' "$UNIT" | grep -q "install ok installed" || fail "not installed"
[ -x /usr/bin/neewer-bridge ] || fail "no /usr/bin/neewer-bridge"
/usr/bin/neewer-bridge --version | grep -q '^neewer-bridge [0-9]' || fail "--version"
ok "installed $(dpkg-query -W -f '${Version}' "$UNIT")"

[ -f "$CONF" ] || fail "postinst did not create $CONF"
cmp -s "$CONF" "$EXAMPLE" || fail "$CONF is not a copy of the example"
[ "$(stat -c %a "$CONF")" = 644 ] || fail "$CONF mode is $(stat -c %a "$CONF")"
dpkg-query -L "$UNIT" | grep -qx "$CONF" && fail "$CONF must not be a packaged file"
grep -qx "$EXAMPLE" /var/lib/dpkg/info/neewer-bridge.conffiles || fail "the example is not a conffile"
ok "config created from the example, owned by the admin"

# Without --config, the packaged binary reads the service's config — even
# from a directory holding a config.toml of its own.
WORK=$(mktemp -d)
printf '[ble]\nflush_hz = 1\n' > "$WORK/config.toml"
(cd "$WORK" && /usr/bin/neewer-bridge lights 2>&1) | grep -q "config=$CONF" \
    || fail "the packaged binary did not resolve to $CONF"
rm -rf "$WORK"
ok "the packaged binary uses $CONF"

systemctl is-enabled --quiet "$UNIT" || fail "unit not enabled"
wait_state 30 active || fail "service did not become active"
[ "$(prop Type)" = notify ] || fail "Type is $(prop Type)"
# The status line follows READY by a moment.
i=0
while :; do
    STATUS=$(prop StatusText)
    case "$STATUS" in
        *"waiting for a Bluetooth adapter"* | *"no lights configured"*) break ;;
    esac
    i=$((i + 1))
    [ "$i" -le 10 ] || fail "unexpected status line: '$STATUS'"
    sleep 1
done
ok "enabled, active, status: $STATUS"

# --- the journal ----------------------------------------------------------------
# Everything arrives (the version banner)...
journal | grep -q "neewer-bridge v.* starting" || fail "no startup banner in the journal"
# ...with no colour escapes and no second timestamp (journald stamps lines)...
ESC=$(printf '\033')
journal | grep -q "$ESC" && fail "colour escapes in the journal"
journal | grep -Eq '^[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}' && fail "lines carry their own timestamp"
# ...and at its real priority: the warning (no adapter, or no lights) is
# there at -p warning, the info banner is not.
journal -p warning | grep -Eq "no usable Bluetooth adapter yet|no \[\[lights\]\] configured" \
    || fail "no warning at priority warning"
journal -p warning | grep -q "starting" && fail "info lines logged as warnings"
ok "journal: plain text, journald timestamps, real priorities"

# --- stop: SIGTERM, promptly and cleanly -----------------------------------------
START=$(date +%s)
systemctl stop "$UNIT"
[ $(($(date +%s) - START)) -le 5 ] || fail "stop took $(($(date +%s) - START)) s"
[ "$(prop ExecMainStatus)" = 0 ] || fail "exit status after stop: $(prop ExecMainStatus)"
journal | grep -q "SIGTERM received" || fail "SIGTERM not handled"
ok "systemctl stop: SIGTERM handled, exit 0"

# --- a configuration error fails once, without a restart loop ----------------------
SAVED=$(mktemp) # outside /etc, so a failed run leaves nothing there
cp "$CONF" "$SAVED"
echo 'this is = not [valid toml' >> "$CONF"
systemctl start "$UNIT" 2>/dev/null && fail "start succeeded with a broken config"
sleep 8 # longer than RestartSec
systemctl is-failed --quiet "$UNIT" || fail "not failed after a config error ($(systemctl is-active "$UNIT"))"
[ "$(prop ExecMainStatus)" = 78 ] || fail "exit status $(prop ExecMainStatus), expected 78"
[ "$(prop NRestarts)" = 0 ] || fail "restarted $(prop NRestarts) times on a config error"
# The parser's pointer at the bad line is part of the error, so it must be
# there at the error's priority too, not just the error's first line.
journal -p err | grep -q "not \[valid toml" || fail "the error's detail lines are not at priority err"
cat "$SAVED" > "$CONF" && rm -f "$SAVED"
systemctl reset-failed "$UNIT"
ok "a broken config: exit 78, unit failed, no restart loop"

# --- a crash is restarted --------------------------------------------------------
systemctl start "$UNIT"
wait_state 30 active || fail "did not start again"
PID=$(prop MainPID)
kill -KILL "$PID"
i=0
while [ "$(prop NRestarts)" = 0 ] || [ "$(systemctl is-active "$UNIT" || true)" != active ]; do
    i=$((i + 1))
    [ "$i" -le 30 ] || fail "not restarted after being killed"
    sleep 1
done
[ "$(prop MainPID)" != "$PID" ] || fail "same PID after a restart"
ok "killed: restarted by systemd ($(prop NRestarts) restart)"

# --- upgrade: the admin's config survives, a disabled unit stays disabled ---------
echo "# edited by the admin" >> "$CONF"
SUM=$(md5sum "$CONF")
PID=$(prop MainPID)
apt_install --reinstall "$UPGRADE" || fail "upgrading to $UPGRADE"
[ "$(md5sum "$CONF")" = "$SUM" ] || fail "the upgrade changed $CONF"
wait_state 30 active || fail "not active after the upgrade"
[ "$(prop MainPID)" != "$PID" ] || fail "not restarted onto the new binary"
ok "upgraded to $(dpkg-query -W -f '${Version}' "$UNIT"): config kept, service restarted"

systemctl disable --now "$UNIT" 2>/dev/null
apt_install --reinstall "$UPGRADE" || fail "reinstalling"
systemctl is-enabled --quiet "$UNIT" && fail "the upgrade re-enabled a disabled unit"
systemctl is-active --quiet "$UNIT" && fail "the upgrade started a disabled unit"
systemctl enable "$UNIT" 2>/dev/null
ok "a disabled unit stays disabled and stopped through an upgrade"

# --- remove keeps the config, purge takes everything -------------------------------
systemctl start "$UNIT"
wait_state 30 active || fail "did not start before the removal"
apt-get remove -y -q "$UNIT" >/dev/null || fail "remove"
systemctl is-active --quiet "$UNIT" && fail "still running after remove"
[ -e /usr/bin/neewer-bridge ] && fail "binary still there after remove"
[ -f "$CONF" ] || fail "remove deleted the config"
ok "removed: stopped, binary gone, config kept"

# Installing again over a removed (not purged) package: the unit was masked by
# the removal and must come back enabled and running, on the kept config.
SUM=$(md5sum "$CONF")
apt_install "$UPGRADE" || fail "reinstalling after remove"
[ "$(md5sum "$CONF")" = "$SUM" ] || fail "reinstalling changed the kept config"
systemctl is-enabled --quiet "$UNIT" || fail "not enabled after reinstalling"
wait_state 30 active || fail "not active after reinstalling"
ok "reinstalled after remove: enabled, active, config kept"

apt-get purge -y -q "$UNIT" >/dev/null || fail "purge"
[ -e /etc/neewer-bridge ] && fail "/etc/neewer-bridge left after purge"
[ -e /var/log/neewer-bridge ] && fail "/var/log/neewer-bridge left after purge"
[ -n "$(find /etc/systemd/system -name "$UNIT.service*" 2>/dev/null)" ] && fail "unit links left after purge"
dpkg-query -W -f '${Status}' "$UNIT" 2>/dev/null | grep -q installed && fail "still known to dpkg"
ok "purged: nothing left"

# A config put in place BEFORE the first install (moving an existing setup
# over) is adopted as it is, never replaced by the example. It also turns on
# the file log, with the relative path the example suggests: it must land in
# the log directory, writable through the unit's sandbox.
mkdir -p /etc/neewer-bridge
printf '# moved over from another machine\n[ble]\nflush_hz = 12\n[logging]\nfile = "neewer-bridge.log"\n' > "$CONF"
SUM=$(md5sum "$CONF")
apt_install "$DEB" || fail "installing over a pre-placed config"
[ "$(md5sum "$CONF")" = "$SUM" ] || fail "the install replaced a pre-placed config"
wait_state 30 active || fail "not active on a pre-placed config"
i=0
until grep -q "starting" /var/log/neewer-bridge/neewer-bridge.log 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -le 10 ] || fail "no file log in /var/log/neewer-bridge"
    sleep 1
done
apt-get purge -y -q "$UNIT" >/dev/null || fail "final purge"
[ -e /etc/neewer-bridge ] && fail "/etc/neewer-bridge left after the final purge"
[ -e /var/log/neewer-bridge ] && fail "/var/log/neewer-bridge left after the final purge"
ok "a pre-placed config is adopted, not replaced; its file log is in /var/log/neewer-bridge"

echo "ALL PASSED"
