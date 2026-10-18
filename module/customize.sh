#!/system/bin/sh
ui_print "- altdb: Android 11+ / ARM64 / KernelSU 3.2.5+"
[ "$KSU" = true ] || abort "KernelSU is required"
[ "$(getprop ro.product.cpu.abi)" = arm64-v8a ] || abort "ARM64 is required"
[ "$(getprop ro.build.version.sdk)" -ge 30 ] || abort "Android 11+ is required"
case "$KSU_VER_CODE" in
  ''|*[!0-9]*) abort "Cannot determine KernelSU Manager version" ;;
esac
[ "$KSU_VER_CODE" -ge 32525 ] || abort "Update KernelSU Manager to v3.2.5 / 32525 or later"
set_perm "$MODPATH/bin/altdb" 0 0 0755
"$MODPATH/bin/altdb" doctor || abort "KernelSU kernel/UAPI/privilege restriction check failed"
set_perm "$MODPATH/service.sh" 0 0 0755
set_perm "$MODPATH/emulated-soft-reboot.sh" 0 0 0755
set_perm "$MODPATH/hotinstall.sh" 0 0 0755
set_perm "$MODPATH/uninstall.sh" 0 0 0755
if [ "$KSU_HAS_HOT_INSTALL" = true ]; then
  export MODULE_HOT_INSTALL_REQUEST=true
  export MODULE_HOT_RUN_SCRIPT=hotinstall.sh
  ui_print "- Requesting hot installation; altdb will restart after installation"
fi
ui_print "- Defaults: random port; adb root disabled"
ui_print "- Open the module WebUI to pair a computer"
# KernelSU assigns the WebUI directory's permissions and context.
