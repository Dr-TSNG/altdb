#!/system/bin/sh
MODDIR=${0%/*}

# KernelSU has already replaced the module directory and loaded sepolicy.rule.
# Stop the old watchdog too, even though hot installs leave no update flag.
if ! sh "$MODDIR/emulated-soft-reboot.sh" --force; then
  echo "! altdb: could not stop the previous service; reboot to apply the update"
  exit 1
fi

# The installer waits for this script, not the long-running watchdog. Detach
# its stdio so the installer can close its output pipe without affecting it.
nohup sh "$MODDIR/service.sh" </dev/null >/dev/null 2>&1 &
