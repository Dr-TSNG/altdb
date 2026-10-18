# altdb

A KernelSU module that provides wireless ADB over your local network.

Connect with the standard `adb` client and manage pairing, ports and permissions through the module WebUI.
System adbd and its existing authorizations are left untouched.

## Features

- Six-digit pairing codes and TLS-encrypted connections. Paired computers can reconnect without pairing again.
- Shell access, file transfers, app installation and removal, logs, reboot, and forward/reverse port forwarding.
- Wi-Fi, hotspots and Ethernet over IPv4. Cellular and VPN interfaces are excluded.
- Automatic pause when system USB or wireless debugging is enabled, followed by recovery once debugging is disabled and system adbd has stopped.
- Session preservation during framework-only soft restarts, while the daemon and network remain alive.
- A WebUI for status, connection commands, paired computers and diagnostics, available in English and Simplified Chinese.

## Requirements

- Android 11 or later on an ARM64 device.
- KernelSU v3.2.5 (32525) or later.
- A computer with `adb` installed, on a network that can reach the phone.

## Getting started

1. Install the module ZIP in KernelSU Manager. With hot install support it starts immediately; otherwise restart the phone as prompted.
2. Turn off system USB debugging and wireless debugging.
3. Open the module WebUI, wait for the service to start, then select **Start pairing**.
4. Run the commands shown in the WebUI on your computer. Enter the six-digit code when prompted:

   ```sh
   adb pair PHONE_IP:PAIRING_PORT
   adb connect PHONE_IP:CONNECTION_PORT
   ```

Pairing and connection ports are different. The pairing window lasts five minutes and closes after a successful pairing.
On later connections, use `adb connect` directly. Paired computers may also reconnect automatically when network discovery is available.

## Settings and permissions

The module's **Action** button toggles altdb on or off and saves the setting, just like the WebUI switch. Turning it off disconnects current sessions; system debugging still takes priority when it is on. The CLI equivalent is `altdb ctl toggle`.

Use the WebUI to configure:

- **Connection port:** Random by default, or a fixed port from `1024–65535`. An occupied fixed port is reported as an error.
- **Enable mDNS:** On by default. Advertises connection and pairing services for automatic discovery. When disabled, use the commands in the WebUI to pair and connect manually. Changing this setting restarts the service and disconnects existing sessions.
- **Allow adb root:** Off by default. When enabled, `adb root` selects root permissions for new services across all connections; `adb unroot` selects shell permissions. TLS connections stay open, and running services keep their existing permissions. A service restart restores shell mode.
- **Paired computers:** Revoking a computer disconnects it immediately. It must pair again to reconnect.
- **Language:** Follows the system by default. A manual language choice is saved on the device.

Connections use shell permissions by default. Shell access to `su` follows KernelSU's authorization for shell. Changing the adb root setting in the WebUI disconnects existing sessions. Module upgrades preserve settings and pairing records; uninstalling removes the module's own data. CLI messages are in English.

On KernelSU versions with hot install support, installing or updating altdb starts the new version without rebooting. An update restarts the service and disconnects existing sessions; paired computers can reconnect without pairing again. Older KernelSU versions apply the module after a reboot.

After `adb root`, new shells append `/data/adb/ksu/bin` to `PATH`. Interactive shells load `/data/adb/ksu/.ksurc` when it exists, matching KernelSU; non-interactive commands such as `adb shell command` and `adb exec-out command` do not load it. `adb unroot` restores the normal environment for new shells.

## Scope

altdb provides local-network ADB only. USB transport, JDWP/IDE debugging, incremental installation, remount, verity and recovery sideload are not supported. Some system commands depend on device permissions.

If a connection fails, check the pause reason in the WebUI, confirm that system debugging is off and the network is reachable, then try the manual connection command shown on the page.
