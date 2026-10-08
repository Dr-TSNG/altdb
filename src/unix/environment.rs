use std::{env, process::Command};

// Keep the device's init/APEX runtime paths across daemon -> service -> shell.
// In particular, BOOTCLASSPATH is device/version-specific and cannot be rebuilt
// from a fixed list of framework jars. Do not inherit loader or shell hooks,
// CLASSPATH, or KernelSU's ASH_STANDALONE into client commands.
pub fn inherit_android_runtime(command: &mut Command) {
    for key in [
        "ANDROID_ROOT",
        "ANDROID_DATA",
        "ANDROID_ASSETS",
        "ANDROID_STORAGE",
        "ANDROID_RUNTIME_ROOT",
        "ANDROID_ART_ROOT",
        "ANDROID_I18N_ROOT",
        "ANDROID_TZDATA_ROOT",
        "EXTERNAL_STORAGE",
        "ASEC_MOUNTPOINT",
        "BOOTCLASSPATH",
        "DEX2OATBOOTCLASSPATH",
        "SYSTEMSERVERCLASSPATH",
        "STANDALONE_SYSTEMSERVER_JARS",
    ] {
        if let Some(value) = env::var_os(key).filter(|value| !value.is_empty()) {
            command.env(key, value);
        }
    }
}

pub const ANDROID_PATH: &str = "/product/bin:/apex/com.android.runtime/bin:/apex/com.android.art/bin:/system_ext/bin:/system/bin:/system/xbin:/odm/bin:/vendor/bin:/vendor/xbin";
