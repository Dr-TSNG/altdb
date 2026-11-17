# altdb

A KernelSU module that provides ADB over local-network TCP or guest-to-host vsock.

Connect with the standard `adb` client and manage pairing, ports and permissions through the module WebUI.
System adbd and its existing authorizations are left untouched.

## Features

- Six-digit pairing codes and TLS-encrypted connections. Paired computers can reconnect without pairing again.
- Shell access, file transfers, app installation and removal, logs, reboot, and forward/reverse port forwarding.
- Wi-Fi, hotspots and Ethernet over IPv4. Cellular and VPN interfaces are excluded.
- Both backends pause while system USB or wireless debugging is enabled, then resume after debugging is disabled and system adbd has stopped. vsock operates independently of Wi-Fi, Ethernet and IP addresses.
- Starts during `post-fs-data`, with a detached watchdog that keeps the boot sequence moving.
- Session preservation during framework-only soft restarts, while the daemon and transport remain alive.
- A WebUI for status, connection commands, paired computers and diagnostics, available in English and Simplified Chinese.

## Requirements

- Android 11 or later on an ARM64 device.
- KernelSU v3.2.5 (32525) or later.
- A computer with `adb` installed. TCP requires a reachable local network; vsock requires a virtual machine with an AF_VSOCK transport.

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

The module's **Action** button toggles altdb on or off and saves the setting, just like the WebUI switch. Turning it off disconnects current sessions. Both backends yield to system debugging. The CLI equivalent is `altdb ctl toggle`.

Use the WebUI to configure:

- **Backend:** `tcp` by default, or `vsock`. Existing configurations select TCP. Switching backend disconnects sessions and closes pairing.

- **Connection port (TCP):** Random by default, or a fixed port from `1024–65535`. An occupied fixed port is reported as an error. vsock always uses port `5555`.
- **Enable mDNS (TCP):** On by default. Advertises connection and pairing services for automatic discovery. When disabled, use the commands in the WebUI to pair and connect manually. Changing this setting restarts TCP sessions. The port and mDNS controls are hidden for vsock; their saved values apply when switching back to TCP.
- **Allow adb root:** Off by default. When enabled, `adb root` selects root permissions for new services across all connections; `adb unroot` selects shell permissions. TLS connections stay open, and running services keep their existing permissions. A service restart restores shell mode.
- **Paired computers:** Revoking a computer disconnects it immediately. It must pair again to reconnect.
- **Language:** Follows the system by default. A manual language choice is saved on the device.

Connections use shell permissions by default. Shell access to `su` follows KernelSU's authorization for shell. Changing the adb root setting in the WebUI disconnects existing sessions. Module upgrades preserve settings and pairing records; uninstalling removes the module's own data. CLI messages are in English.

On KernelSU versions with hot install support, installing or updating altdb starts the new version without rebooting. An update restarts the service and disconnects existing sessions; paired computers can reconnect without pairing again. Older KernelSU versions apply the module after a reboot.

After `adb root`, new shells append `/data/adb/ksu/bin` to `PATH`. Interactive shells load `/data/adb/ksu/.ksurc` when it exists, matching KernelSU; non-interactive commands such as `adb shell command` and `adb exec-out command` do not load it. `adb unroot` restores the normal environment for new shells.

## vsock

Select **vsock** in the WebUI or set `"backend":"vsock"` in the configuration. Like TCP, this backend pauses when system USB or wireless debugging is enabled, and resumes only after debugging is disabled and system adbd has stopped. Pausing closes listeners, pairing and sessions. The same checks for unavailable settings and framework restart grace apply to both backends. Wi-Fi changes and IP addresses do not affect vsock listeners or connections.

Connection and pairing listeners use AF_VSOCK and accept the VM host (CID 2). Connections always use port `5555`, regardless of `port_mode` and `fixed_port`. Pairing uses a separate temporary port, which may exceed 65535. Status reports endpoints as `vsock:CID:PORT`. Both listeners retain TLS and the existing host-key authorization flow.

For a Linux VM host, expose each endpoint on a host loopback TCP port using a vsock bridge, for example:

```sh
socat TCP4-LISTEN:15555,bind=127.0.0.1,reuseaddr,fork VSOCK-CONNECT:GUEST_CID:5555
# In another terminal, bridge the temporary pairing endpoint:
socat TCP4-LISTEN:15556,bind=127.0.0.1,reuseaddr,fork VSOCK-CONNECT:GUEST_CID:PAIRING_PORT
adb pair 127.0.0.1:15556
adb connect 127.0.0.1:15555
```

Use the ports reported by the WebUI or `altdb ctl status`; start pairing before opening its bridge. TLS remains end to end through the bridge. On other hosts, use the virtual machine monitor's vsock forwarding facility.

## Scope

altdb provides TCP and vsock ADB. USB gadget transport, JDWP/IDE debugging, incremental installation, remount, verity and recovery sideload are not supported. Some system commands depend on device permissions.

If a connection fails, check the pause reason in the WebUI, check the selected backend's debugging switch and connection path, then try the manual connection command shown on the page.
