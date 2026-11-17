#!/system/bin/sh
MODDIR=${0%/*}
BUSYBOX=/data/adb/ksu/bin/busybox
export ASH_STANDALONE=1
# post-fs-data is blocking: detach the watchdog and all inherited stdio.
"$BUSYBOX" nohup "$BUSYBOX" sh "$MODDIR/watchdog.sh" </dev/null >/dev/null 2>&1 &
