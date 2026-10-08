use crate::config::MIN_KSU;
use anyhow::{Result, ensure};
use serde::Serialize;
#[cfg(target_os = "android")]
use std::ffi::CString;
use std::{
    fs,
    io::Read,
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

const GET_INFO: libc::c_ulong = 0x80104b02;
const DISABLE_ESCAPE: libc::c_ulong = 0x4b15;
#[repr(C)]
#[derive(Default, Serialize)]
pub struct Info {
    pub version: u32,
    pub flags: u32,
    pub features: u32,
    pub uapi_version: u32,
}

fn validate_info(info: &Info) -> Result<()> {
    ensure!(
        info.version >= MIN_KSU,
        "requires KernelSU v3.2.5 / {MIN_KSU}+; detected kernel {}",
        info.version
    );
    // UAPI is a minimum requirement, not an exact ksud/kernel version match.
    // The disposable-process DISABLE_ESCAPE_TO_ROOT probe below remains
    // mandatory, including on newer UAPI versions.
    ensure!(
        info.uapi_version >= 2,
        "unsupported KernelSU UAPI {}; requires >= 2 (kernel {})",
        info.uapi_version,
        info.version
    );
    Ok(())
}

fn driver() -> Result<OwnedFd> {
    let mut fd: i32 = -1;
    unsafe {
        libc::syscall(libc::SYS_reboot, 0xdeadbeefu32, 0xcafebabeu32, 0, &mut fd);
    }
    ensure!(fd >= 0, "KernelSU driver unavailable");
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "cannot protect KernelSU descriptor"
    );
    Ok(owned)
}

pub fn info() -> Result<Info> {
    let fd = driver()?;
    let mut info = Info::default();
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), GET_INFO as _, &mut info) } == 0,
        "KernelSU UAPI query failed"
    );
    Ok(info)
}

pub fn prevent_escalation() -> Result<()> {
    let fd = driver()?;
    ensure!(
        unsafe {
            libc::ioctl(
                fd.as_raw_fd(),
                DISABLE_ESCAPE as _,
                std::ptr::null::<libc::c_void>(),
            )
        } == 0,
        "KSU_NO_NEW_PRIVS unavailable"
    );
    Ok(())
}

pub fn check() -> Result<Info> {
    ensure!(unsafe { libc::geteuid() } == 0, "requires root");
    ensure!(
        property("ro.build.version.sdk")?.parse::<u32>()? >= 30,
        "requires Android 11+"
    );
    ensure!(cfg!(target_arch = "aarch64"), "requires ARM64");
    let info = info()?;
    validate_info(&info)?;
    // Probe in a disposable process: this flag must never taint the supervisor.
    let status = Command::new(std::env::current_exe()?)
        .arg("probe-ksu")
        .status()?;
    ensure!(
        status.success(),
        "kernel does not implement KSU_NO_NEW_PRIVS"
    );
    Ok(info)
}

pub fn context(name: &str) -> Result<()> {
    fs::write("/proc/thread-self/attr/current", name)?;
    Ok(())
}
/// Invoked only in a disposable local diagnostic process. It execs su after
/// dropping to the normal shell service identity. Only the blocked diagnostic
/// applies KSU_NO_NEW_PRIVS; normal shell services follow KernelSU authorization.
pub fn probe_shell_su(allow: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    ensure!(unsafe { libc::geteuid() } == 0, "probe requires root");
    if !allow {
        prevent_escalation()?;
    }
    restrict_service(false, false)?;
    let error = Command::new("/system/bin/su")
        .args(["-c", "id -u"])
        .env_clear()
        .env("PATH", "/system/bin:/system/xbin")
        .exec();
    Err(error.into())
}

pub fn verify_su_isolation() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut outcomes = vec![];
    for mode in ["allow", "block"] {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["probe-shell-su", mode])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) < 0
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut output = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut data = vec![];
            output
                .by_ref()
                .take(1024)
                .read_to_end(&mut data)
                .map(|_| data)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                anyhow::bail!("su probe timed out; pre-authorize shell in KernelSU before testing")
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let bytes = reader
            .join()
            .map_err(|_| anyhow::anyhow!("probe reader failed"))??;
        outcomes.push(status.success() && bytes == b"0\n");
    }
    ensure!(
        outcomes[0],
        "baseline su did not obtain root; authorize shell in KernelSU and repeat"
    );
    ensure!(!outcomes[1], "KSU_NO_NEW_PRIVS failed to block su");
    Ok(())
}

pub fn restrict_network(test: bool) -> Result<()> {
    if test {
        ensure!(
            !cfg!(target_os = "android"),
            "test mode is unavailable on Android"
        );
        return Ok(());
    }
    prevent_escalation()?;
    identity(2000, &[3003])?;
    context("u:r:altdb_net:s0")?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0,
        "no_new_privs failed"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0,
        "dump protection failed"
    );
    Ok(())
}

pub fn restrict_service(root: bool, test: bool) -> Result<()> {
    if test {
        ensure!(
            !cfg!(target_os = "android"),
            "test mode is unavailable on Android"
        );
        return Ok(());
    }
    if root {
        context("u:r:ksu:s0")?;
        return Ok(());
    }
    // AOSP adbd supplemental groups, API 30 baseline.
    identity(
        2000,
        &[
            1004, 1007, 1011, 1015, 1028, 1078, 3001, 3002, 3003, 3006, 3009, 3011,
        ],
    )?;
    context("u:r:shell:s0")?;
    Ok(())
}

fn identity(uid: u32, groups: &[u32]) -> Result<()> {
    // Drop the capability bounding set before giving up CAP_SETPCAP.
    for cap in 0..64 {
        let present = unsafe { libc::prctl(libc::PR_CAPBSET_READ, cap, 0, 0, 0) };
        if present < 0 {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
                "capability query failed"
            );
            break;
        }
        ensure!(
            unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) } == 0,
            "capability bounding drop failed"
        );
    }
    ensure!(
        unsafe { libc::setgroups(groups.len(), groups.as_ptr()) } == 0,
        "setgroups failed"
    );
    ensure!(
        unsafe { libc::setresgid(uid, uid, uid) } == 0
            && unsafe { libc::setresuid(uid, uid, uid) } == 0,
        "identity drop failed"
    );
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Caps {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x20080522,
        pid: 0,
    };
    let data = [Caps {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    ensure!(
        unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } == 0,
        "capability drop failed"
    );
    ensure!(
        unsafe { libc::getuid() } == uid && unsafe { libc::geteuid() } == uid,
        "identity verification failed"
    );
    Ok(())
}

#[cfg(target_os = "android")]
unsafe extern "C" {
    fn __system_property_get(name: *const libc::c_char, value: *mut libc::c_char) -> libc::c_int;
    fn __system_property_area_serial() -> u32;
    fn __system_property_wait(
        info: *const libc::c_void,
        old: u32,
        new: *mut u32,
        timeout: *const libc::timespec,
    ) -> bool;
}

pub fn watch_properties() -> Arc<AtomicBool> {
    let changed = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "android")]
    {
        let flag = changed.clone();
        std::thread::spawn(move || {
            let mut serial = unsafe { __system_property_area_serial() };
            loop {
                let timeout = libc::timespec {
                    tv_sec: 1,
                    tv_nsec: 0,
                };
                let mut next = serial;
                if unsafe { __system_property_wait(std::ptr::null(), serial, &mut next, &timeout) }
                {
                    serial = next;
                    flag.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        });
    }
    changed
}

pub fn property(name: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        let name = CString::new(name)?;
        let mut buf = [0 as libc::c_char; 92];
        let n = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
        ensure!(n >= 0, "property read failed");
        return Ok(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_str()?
            .to_owned());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = name;
        anyhow::bail!("Android properties unavailable")
    }
}

pub fn native_properties() -> Result<Option<String>> {
    native_property_status(
        &property("init.svc.adbd")?,
        &property("persist.adb.tls_server.enable")?,
        &property("sys.usb.state")?,
    )
}

fn native_property_status(adbd: &str, wireless: &str, usb: &str) -> Result<Option<String>> {
    if wireless == "1" {
        return Ok(Some("Native wireless debugging is enabled".into()));
    }
    if usb.split(',').any(|s| s == "adb") {
        return Ok(Some("Native USB debugging is enabled".into()));
    }
    match adbd {
        "running" | "restarting" | "stopping" => {
            Ok(Some("Native adbd is running or stopping".into()))
        }
        // init may not publish a state until this disabled service first runs.
        // The caller ALSO requires a successful settings + /proc check; an
        // empty property alone is never sufficient to resume the daemon.
        "stopped" | "" => Ok(None),
        _ => anyhow::bail!("Cannot determine native adbd state"),
    }
}

pub const SETTINGS_UNAVAILABLE: &str =
    "Debugging settings are temporarily unavailable; keeping connections";
const SETTINGS_GRACE: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
enum NativeState {
    Clear,
    Paused(String),
    Grace,
}

fn native_state(
    last_clear: &mut Option<Instant>,
    processes: Result<Option<String>>,
    settings: Result<Option<String>>,
    now: Instant,
) -> NativeState {
    let settings_failed = matches!(processes, Ok(None)) && settings.is_err();
    match processes.and_then(|reason| {
        if reason.is_some() {
            Ok(reason)
        } else {
            settings
        }
    }) {
        Ok(None) => {
            *last_clear = Some(now);
            NativeState::Clear
        }
        Ok(Some(reason)) => {
            *last_clear = None;
            NativeState::Paused(reason)
        }
        Err(_)
            if settings_failed
                && last_clear
                    .is_some_and(|checked| now.duration_since(checked) < SETTINGS_GRACE) =>
        {
            // Only settings failures may reuse a recent successful check.
            // Do not refresh its timestamp until settings can be read again.
            NativeState::Grace
        }
        Err(error) => {
            *last_clear = None;
            NativeState::Paused(format!(
                "Cannot read native debugging settings or processes; service paused: {error}"
            ))
        }
    }
}

struct NativeSnapshot {
    checked_at: Instant,
    state: NativeState,
}

pub struct NativeMonitor {
    snapshot: Arc<Mutex<Option<NativeSnapshot>>>,
    _lifetime: mpsc::Sender<()>,
}

impl NativeMonitor {
    pub fn start() -> Result<Self> {
        let mut last_clear = None;
        Self::spawn(move || {
            let processes = native_processes();
            let settings = if matches!(processes, Ok(None)) {
                read_native_settings()
            } else {
                // Do not wait on Android Binder when /proc already requires a pause.
                Ok(None)
            };
            native_state(&mut last_clear, processes, settings, Instant::now())
        })
    }

    fn spawn(mut check: impl FnMut() -> NativeState + Send + 'static) -> Result<Self> {
        let snapshot = Arc::new(Mutex::new(None));
        let state = snapshot.clone();
        let (lifetime, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("native-debug-check".into())
            .spawn(move || {
                loop {
                    let result = check();
                    *state.lock().unwrap() = Some(NativeSnapshot {
                        checked_at: Instant::now(),
                        state: result,
                    });
                    // Disconnect on drop stops the monitor without blocking the daemon.
                    if matches!(
                        receiver.recv_timeout(Duration::from_secs(3)),
                        Err(mpsc::RecvTimeoutError::Disconnected)
                    ) {
                        break;
                    }
                }
            })?;
        Ok(Self {
            snapshot,
            _lifetime: lifetime,
        })
    }

    pub fn status(&self, running: bool) -> (Option<String>, bool) {
        match &*self.snapshot.lock().unwrap() {
            None => (
                Some("Checking native debugging settings and processes".into()),
                false,
            ),
            Some(snapshot) if snapshot.checked_at.elapsed() > Duration::from_secs(6) => (
                Some("Native debugging check is stale; service paused".into()),
                false,
            ),
            Some(snapshot) => match &snapshot.state {
                NativeState::Clear => (None, false),
                NativeState::Paused(reason) => (Some(reason.clone()), false),
                NativeState::Grace if running => (None, true),
                NativeState::Grace => (
                    Some("Checking native debugging settings and processes".into()),
                    false,
                ),
            },
        }
    }
}

fn native_processes() -> Result<Option<String>> {
    // Detect an adbd launched outside init as well. Vanishing /proc entries are
    // normal; unreadable process state is conservatively treated as unknown.
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        match fs::read_to_string(entry.path().join("comm")) {
            Ok(name) if name.trim() == "adbd" => {
                return Ok(Some("A running native adbd process was detected".into()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn read_native_settings() -> Result<Option<String>> {
    let child = Command::new("/system/bin/settings")
        .args(["list", "global"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let output = settings_output(child, Duration::from_secs(2))?;
    native_settings_status(std::str::from_utf8(&output)?)
}

fn settings_output(mut child: Child, timeout: Duration) -> Result<Vec<u8>> {
    let mut stdout = child.stdout.take().unwrap();
    let result = (|| -> Result<Vec<u8>> {
        super::nonblocking(&stdout)?;
        let mut out = vec![];
        let mut eof = false;
        let deadline = Instant::now() + timeout;
        let mut buffer = [0; 8192];
        loop {
            ensure!(
                Instant::now() < deadline,
                "Timed out reading debugging settings"
            );
            if !eof {
                match stdout.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        ensure!(
                            out.len() + n <= 1024 * 1024,
                            "Cannot read system debugging settings"
                        );
                        out.extend_from_slice(&buffer[..n]);
                        continue;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "Cannot read system debugging settings");
                if eof {
                    return Ok(out);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() {
        // settings can wrap cmd; kill the private group as well as the wrapper.
        // A descendant holding stdout must not strand the monitor during reboot.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
    }
    result
}

fn native_settings_status(text: &str) -> Result<Option<String>> {
    ensure!(
        text.contains('='),
        "System debugging settings are unavailable"
    );
    for line in text.lines() {
        if let Some(("adb_enabled" | "adb_wifi_enabled", value)) = line.split_once('=') {
            match value {
                "1" => {
                    return Ok(Some(
                        "Native debugging is enabled in developer options".into(),
                    ));
                }
                "0" => {}
                _ => anyhow::bail!("Unrecognized native debugging setting"),
            }
        }
    }
    // Unset adb settings have Android's disabled default, provided the global
    // settings read itself succeeded. Empty/failed output is never accepted.
    Ok(None)
}

pub fn child_setup() -> Result<()> {
    ensure!(
        unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "missing bootstrap descriptor"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } == 0,
        "parent death signal failed"
    );
    ensure!(unsafe { libc::getppid() } > 1, "supervisor exited");
    let zero: libc::rlimit = unsafe { mem::zeroed() };
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn settings_reader_bounds_hung_binder_calls_and_inherited_stdout() {
        let command = |script| {
            Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap()
        };
        assert_eq!(
            settings_output(command("printf 'adb_enabled=0\\n'"), Duration::from_secs(2)).unwrap(),
            b"adb_enabled=0\n"
        );
        for script in ["sleep 30", "sleep 30 & exit 0"] {
            let started = Instant::now();
            assert!(settings_output(command(script), Duration::from_millis(100)).is_err());
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    fn unavailable() -> Result<Option<String>> {
        anyhow::bail!("settings provider unavailable")
    }

    #[test]
    fn settings_grace_requires_a_recent_success_and_does_not_extend_it() {
        let mut last_clear = None;
        let now = Instant::now();
        assert!(matches!(
            native_state(&mut last_clear, Ok(None), unavailable(), now),
            NativeState::Paused(_)
        ));
        assert_eq!(
            native_state(&mut last_clear, Ok(None), Ok(None), now),
            NativeState::Clear
        );
        assert_eq!(
            native_state(
                &mut last_clear,
                Ok(None),
                unavailable(),
                now + Duration::from_secs(90)
            ),
            NativeState::Grace
        );
        assert_eq!(last_clear, Some(now));
        assert!(matches!(
            native_state(
                &mut last_clear,
                Ok(None),
                unavailable(),
                now + SETTINGS_GRACE
            ),
            NativeState::Paused(_)
        ));
        assert!(last_clear.is_none());
        assert!(matches!(
            native_state(
                &mut last_clear,
                Ok(None),
                unavailable(),
                now + SETTINGS_GRACE
            ),
            NativeState::Paused(_)
        ));
        let recovered = now + SETTINGS_GRACE;
        assert_eq!(
            native_state(&mut last_clear, Ok(None), Ok(None), recovered),
            NativeState::Clear
        );
        assert_eq!(last_clear, Some(recovered));
    }

    #[test]
    fn native_debugging_and_proc_errors_bypass_settings_grace() {
        let now = Instant::now();
        for (processes, settings) in [
            (Ok(Some("adbd running".into())), unavailable()),
            (Err(anyhow::anyhow!("proc unavailable")), unavailable()),
            (Ok(None), Ok(Some("debugging enabled".into()))),
        ] {
            let mut last_clear = Some(now);
            assert!(matches!(
                native_state(&mut last_clear, processes, settings, now),
                NativeState::Paused(_)
            ));
            assert!(last_clear.is_none());
        }
    }

    #[test]
    fn settings_grace_allows_preservation_but_not_startup_or_stale_checks() {
        let (lifetime, _receiver) = mpsc::channel();
        let monitor = NativeMonitor {
            snapshot: Arc::new(Mutex::new(Some(NativeSnapshot {
                checked_at: Instant::now(),
                state: NativeState::Grace,
            }))),
            _lifetime: lifetime,
        };
        assert_eq!(monitor.status(true), (None, true));
        assert!(monitor.status(false).0.is_some());
        monitor
            .snapshot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .checked_at -= Duration::from_secs(7);
        assert!(monitor.status(true).0.is_some());
        assert!(!monitor.status(true).1);
    }

    #[test]
    fn native_properties_handle_never_started_and_active_adbd() {
        assert!(native_property_status("", "", "").unwrap().is_none());
        assert!(
            native_property_status("stopped", "0", "mtp")
                .unwrap()
                .is_none()
        );
        for state in ["running", "restarting", "stopping"] {
            assert!(native_property_status(state, "0", "mtp").unwrap().is_some());
        }
        assert!(native_property_status("", "1", "").unwrap().is_some());
        assert!(
            native_property_status("", "0", "mtp,adb")
                .unwrap()
                .is_some()
        );
        assert!(native_property_status("invalid-state", "0", "mtp").is_err());
    }

    #[test]
    fn native_settings_distinguish_unset_from_unreadable() {
        assert!(native_settings_status("boot_count=1\n").unwrap().is_none());
        assert!(
            native_settings_status("adb_enabled=0\nadb_wifi_enabled=0\n")
                .unwrap()
                .is_none()
        );
        assert!(native_settings_status("adb_enabled=1\n").unwrap().is_some());
        assert!(
            native_settings_status("adb_wifi_enabled=1\n")
                .unwrap()
                .is_some()
        );
        assert!(native_settings_status("").is_err());
        assert!(native_settings_status("Permission denied").is_err());
        assert!(native_settings_status("adb_enabled=unknown\n").is_err());
    }

    #[test]
    fn native_monitor_stays_responsive_and_pauses_until_checked_or_when_stale() {
        let (release, blocked) = mpsc::channel();
        let (entered, checking) = mpsc::channel();
        let monitor = NativeMonitor::spawn(move || {
            entered.send(()).unwrap();
            blocked.recv().unwrap()
        })
        .unwrap();
        checking.recv_timeout(Duration::from_secs(2)).unwrap();
        // The check is still blocked: querying the supervisor's cached state
        // must return immediately and must not allow startup yet.
        assert!(monitor.status(false).0.is_some());
        release.send(NativeState::Clear).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while monitor.snapshot.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(monitor.status(false).0.is_none());
        monitor
            .snapshot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .checked_at = Instant::now() - Duration::from_secs(7);
        assert!(monitor.status(true).0.unwrap().contains("stale"));
    }

    #[test]
    fn native_monitor_keeps_failed_checks_paused() {
        let monitor =
            NativeMonitor::spawn(|| NativeState::Paused("simulated read failure".into())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while monitor.snapshot.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            monitor
                .status(true)
                .0
                .unwrap()
                .contains("simulated read failure")
        );
    }

    #[test]
    fn accepts_minimum_and_newer_uapis_with_metadata_flags() {
        for uapi_version in [2, 3, 4, 5, u32::MAX] {
            // Includes UAPI 4's BUNDLED bit and future metadata bits. Flags
            // and max feature ID do not determine privilege restrictions.
            let info = Info {
                version: MIN_KSU,
                flags: u32::MAX,
                features: u32::MAX,
                uapi_version,
            };
            validate_info(&info).unwrap();
        }
    }

    #[test]
    fn rejects_old_kernels_and_reports_too_old_uapis() {
        for uapi_version in [2, 3, 4, 5, u32::MAX] {
            let info = Info {
                version: MIN_KSU - 1,
                uapi_version,
                ..Info::default()
            };
            assert!(validate_info(&info).is_err());
        }
        for uapi_version in [0, 1] {
            let info = Info {
                version: MIN_KSU + 100,
                uapi_version,
                ..Info::default()
            };
            let error = validate_info(&info).unwrap_err().to_string();
            assert!(error.contains(&format!("UAPI {uapi_version};")));
            assert!(error.contains("requires >= 2"));
        }
    }
}
