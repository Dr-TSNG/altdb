#!/system/bin/sh
MODDIR=${0%/*}

# KernelSU runs boot scripts in its standalone BusyBox shell. Keep the lock
# outside the module directory so an upgrade cannot create a second watchdog.
umask 077
STATE=/data/adb/altdb
[ ! -L "$STATE" ] || exit 1
mkdir -p "$STATE" || exit 1
chmod 0700 "$STATE" || exit 1
[ ! -L "$STATE/service.lock" ] || exit 1
exec 9>>"$STATE/service.lock" || exit 1
flock -n 9 || exit 0
rm -f "$STATE/service.stop" || exit 1

PID=
stopping() {
  [ -n "$PID" ] && kill -TERM "$PID" 2>/dev/null
  [ -n "$PID" ] && wait "$PID" 2>/dev/null
  exit 0
}
trap stopping TERM INT
trap '' HUP
check_stop() {
  if [ -f "$STATE/service.stop" ] || [ -f "$MODDIR/disable" ] || [ -f "$MODDIR/remove" ]; then
    stopping
  fi
}
while [ "$(getprop sys.boot_completed)" != 1 ]; do
  check_stop
  sleep 1
done
DELAY=1
while [ ! -f "$MODDIR/disable" ] && [ ! -f "$MODDIR/remove" ]; do
  check_stop
  START=$(date +%s)
  "$MODDIR/bin/altdb" daemon 9>&- >/dev/null 2>&1 &
  PID=$!
  while kill -0 "$PID" 2>/dev/null; do
    check_stop
    sleep 1
  done
  wait "$PID"
  RESULT=$?
  PID=
  # The daemon also locks independently to cover manual CLI starts.
  case "$RESULT" in
    0|73) exit 0 ;;
  esac
  NOW=$(date +%s)
  [ $((NOW - START)) -ge 60 ] && DELAY=1
  REMAINING=$DELAY
  while [ "$REMAINING" -gt 0 ]; do
    check_stop
    sleep 1
    REMAINING=$((REMAINING - 1))
  done
  DELAY=$((DELAY * 2))
  [ "$DELAY" -gt 60 ] && DELAY=60
done
