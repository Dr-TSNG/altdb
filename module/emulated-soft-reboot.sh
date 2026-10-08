#!/system/bin/sh
MODDIR=${0%/*}

# KernelSU calls this before applying modules_update. An unchanged module keeps
# its watchdog, daemon, listening port and existing sessions alive.
[ -f "$MODDIR/update" ] || exit 0

umask 077
STATE=/data/adb/altdb
[ ! -L "$STATE" ] || exit 1
[ -d "$STATE" ] || exit 0
for FILE in service.stop service.lock daemon.lock; do
  [ ! -L "$STATE/$FILE" ] || exit 1
done

# Also stop a watchdog waiting for boot or backing off after a crash. Leave the
# request in place until the next watchdog owns service.lock.
: >"$STATE/service.stop" || exit 1
"$MODDIR/bin/altdb" ctl stop >/dev/null 2>&1

# ctl stop acknowledges the request before shutdown finishes. Wait for both
# owners to exit so the new service cannot lose the race for either lock.
# BusyBox flock has no timeout option.
wait_lock() {
  LEFT=10
  until flock -n "$1"; do
    [ "$LEFT" -gt 0 ] || return 1
    sleep 1
    LEFT=$((LEFT - 1))
  done
}
exec 9>>"$STATE/service.lock" || exit 1
wait_lock 9 || exit 1
exec 8>>"$STATE/daemon.lock" || exit 1
wait_lock 8 || exit 1
