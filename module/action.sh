#!/system/bin/sh
MODDIR=${0%/*}
exec "$MODDIR/bin/altdb" ctl toggle
