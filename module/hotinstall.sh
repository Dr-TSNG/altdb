#!/system/bin/sh
MODDIR=${0%/*}
BUSYBOX=/data/adb/ksu/bin/busybox
export ASH_STANDALONE=1

# KernelSU has already replaced the module directory and loaded sepolicy.rule.
# Stop the old watchdog too, even though hot installs leave no update flag.
if ! "$BUSYBOX" sh "$MODDIR/emulated-soft-reboot.sh" --force; then
  echo "! altdb: could not stop the previous service; reboot to apply the update"
  exit 1
fi

# The installer waits for this script, not the long-running watchdog. Detach
# its stdio so the installer can close its output pipe without affecting it.
# nohup execs through PATH, so an unqualified sh would lose standalone mode.
"$BUSYBOX" nohup "$BUSYBOX" sh "$MODDIR/service.sh" </dev/null >/dev/null 2>&1 &

if [ "$(getprop sys.boot_completed)" = 1 ]; then
  if ! "$BUSYBOX" timeout 4 "$BUSYBOX" sh -c '
    until "$1" ctl status >/dev/null 2>&1; do sleep 0.2; done
  ' altdb "$MODDIR/bin/altdb"; then
    echo "! altdb: control socket did not become available; check /data/adb/altdb/service.log"
    exit 1
  fi
fi
