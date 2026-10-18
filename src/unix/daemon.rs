use super::{
    ipc::{self, Message, Request, ServiceStart},
    ksu, mdns,
    network::{self, Endpoint},
    store::Store,
};
use crate::config::{self, Control};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    process::Child,
    time::{Duration, Instant},
};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

struct Bundle {
    endpoints: Vec<Endpoint>,
    tcp: Vec<TcpListener>,
    mdns_specs: Vec<Endpoint>,
    mdns: Vec<std::net::UdpSocket>,
}

struct Worker {
    child: Child,
    fd: OwnedFd,
}

struct Service {
    child: Child,
    transport: u64,
    binder: Option<(String, OwnedFd)>,
    stopped: bool,
    close_at: Option<Instant>,
}

impl Service {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if self.binder.take().is_some() {
            return;
        }
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGTERM);
            }
        }
    }
}

struct Window {
    epoch: u64,
    code: zeroize::Zeroizing<String>,
    expires: Instant,
    expires_at: u64,
    failures: u32,
    endpoints: Vec<Endpoint>,
}

struct Daemon {
    store: Store,
    test: bool,
    worker: Option<Worker>,
    bundle: Option<Bundle>,
    services: Vec<Service>,
    transports: HashMap<u64, String>,
    window: Option<Window>,
    root: bool,
    state: String,
    reason: String,
    logs: VecDeque<Value>,
    epoch: u64,
    quit: bool,
    retry_at: Instant,
    failures: u32,
}

impl Daemon {
    fn log(&mut self, message: &str) {
        self.logs
            .push_back(json!({"time": super::now(), "message": message}));
        while self.logs.len() > 256 {
            self.logs.pop_front();
        }
    }

    fn status(&self) -> Value {
        let endpoints = self
            .bundle
            .as_ref()
            .map(|bundle| {
                bundle
                    .endpoints
                    .iter()
                    .map(Endpoint::display)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let pairing = self.window.as_ref().map(|window| {
            json!({
                "code": &*window.code,
                "expires_at": window.expires_at,
                "failures": window.failures,
                "endpoints": window.endpoints.iter().map(Endpoint::display).collect::<Vec<_>>(),
            })
        });
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "state": self.state,
            "reason": self.reason,
            "root": self.root,
            "config": self.store.config,
            "hosts": self.store.hosts,
            "connections": self.transports.len(),
            "endpoints": endpoints,
            "pairing": pairing,
        })
    }

    fn send(&self, message: &Message, fds: &[i32]) -> Result<()> {
        let w = self
            .worker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("network worker unavailable"))?;
        ipc::send(w.fd.as_raw_fd(), message, fds)
    }

    fn stop_worker(&mut self) {
        self.window = None;
        if let Some(mut w) = self.worker.take() {
            let _ = ipc::send(w.fd.as_raw_fd(), &Message::Shutdown, &[]);
            for _ in 0..20 {
                if w.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if w.child.try_wait().ok().flatten().is_none() {
                let _ = w.child.kill();
            }
            let _ = w.child.wait();
        }
        for s in &mut self.services {
            s.stop();
        }
        for mut s in self.services.drain(..) {
            for _ in 0..10 {
                if s.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if s.child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(-(s.child.id() as i32), libc::SIGKILL);
                }
            }
            let _ = s.child.wait();
        }
        self.transports.clear();
    }

    fn stop(&mut self, state: &str, reason: &str) {
        if state == "error" {
            self.failures = (self.failures + 1).min(6);
            self.retry_at = Instant::now() + Duration::from_secs(1u64 << self.failures);
        }
        if self.worker.is_some() {
            self.stop_worker();
        }
        self.bundle = None;
        self.root = false;
        if self.state != state || self.reason != reason {
            self.log(reason);
        }
        self.state = state.into();
        self.reason = reason.into();
    }

    fn start_worker(&mut self) -> Result<()> {
        let bundle = self
            .bundle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no listening interfaces"))?;
        let (a, b) = ipc::pair()?;
        let child = ipc::spawn("worker", b.as_raw_fd())?;
        drop(b);
        let fds: Vec<_> = bundle
            .tcp
            .iter()
            .map(AsRawFd::as_raw_fd)
            .chain(bundle.mdns.iter().map(AsRawFd::as_raw_fd))
            .collect();
        let boot = Message::Boot {
            identity: self.store.identity.clone(),
            hosts: self.store.hosts.clone(),
            endpoints: bundle.endpoints.clone(),
            mdns: bundle.mdns_specs.clone(),
            test: self.test,
        };
        ipc::send(a.as_raw_fd(), &boot, &fds)?;
        self.worker = Some(Worker { child, fd: a });
        self.state = "starting".into();
        self.reason = "Checking network worker permissions".into();
        Ok(())
    }

    fn start(&mut self, addresses: &[Endpoint]) -> Result<()> {
        let (endpoints, tcp) = network::listeners(addresses, self.store.config.port(), self.test)?;
        let (mdns_specs, mdns) = if self.store.config.mdns_enabled {
            mdns::sockets(&endpoints, self.test)?
        } else {
            (vec![], vec![])
        };
        self.bundle = Some(Bundle {
            endpoints,
            tcp,
            mdns_specs,
            mdns,
        });
        self.start_worker()
    }

    fn pair_stop(&mut self) {
        self.window = None;
        let _ = self.send(&Message::PairStop, &[]);
    }

    fn control(&mut self, request: Control) -> Result<Value> {
        match request {
            Control::Status => {}
            Control::Logs => return Ok(json!(self.logs)),
            Control::Toggle => {
                let mut config = self.store.config.clone();
                config.enabled = !config.enabled;
                return self.control(Control::Configure { config });
            }
            Control::PairStart => {
                ensure!(self.state == "running", "Service is not running");
                ensure!(
                    self.store.hosts.len() < config::MAX_HOSTS,
                    "Paired host limit reached; revoke a host first"
                );
                self.pair_stop();
                let bundle = self.bundle.as_ref().unwrap();
                let (endpoints, tcp) = network::listeners(&bundle.endpoints, 0, self.test)?;
                let code = crate::crypto::pairing_code()?;
                self.epoch += 1;
                self.send(
                    &Message::PairStart {
                        epoch: self.epoch,
                        code: code.clone(),
                        endpoints: endpoints.clone(),
                    },
                    &tcp.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>(),
                )?;
                self.window = Some(Window {
                    epoch: self.epoch,
                    code: zeroize::Zeroizing::new(code),
                    expires: Instant::now() + Duration::from_secs(config::PAIR_SECONDS),
                    expires_at: super::now() + config::PAIR_SECONDS,
                    failures: 0,
                    endpoints,
                });
                self.log("Pairing window opened");
            }
            Control::PairStop => {
                self.pair_stop();
                self.log("Pairing window closed");
            }
            Control::Configure { config } => {
                let previous = self.store.config.clone();
                self.store.configure(config)?;
                if previous != self.store.config {
                    let endpoint_changed = previous.port_mode != self.store.config.port_mode
                        || previous.fixed_port != self.store.config.fixed_port
                        || previous.mdns_enabled != self.store.config.mdns_enabled
                        || previous.enabled != self.store.config.enabled;
                    if endpoint_changed {
                        self.stop(
                            "disabled",
                            "Configuration updated; checking startup conditions",
                        );
                    } else {
                        self.stop_worker();
                        self.root = false;
                        if self.bundle.is_some() {
                            self.start_worker()?;
                        }
                    }
                    self.log("Configuration saved");
                }
            }
            Control::Revoke { fingerprint } => {
                self.store.revoke(&fingerprint)?;
                let ids: Vec<_> = self
                    .transports
                    .iter()
                    .filter(|(_, fp)| *fp == &fingerprint)
                    .map(|(id, _)| *id)
                    .collect();
                for id in ids {
                    self.disconnect(id);
                }
                if self.worker.is_some() {
                    self.send(
                        &Message::Hosts {
                            hosts: self.store.hosts.clone(),
                        },
                        &[],
                    )?;
                }
                self.log("Host authorization revoked");
            }
            Control::Stop => {
                self.quit = true;
                self.stop("disabled", "Service stopped");
            }
        }
        Ok(self.status())
    }

    fn disconnect(&mut self, transport: u64) {
        self.transports.remove(&transport);
        for service in &mut self.services {
            if service.transport == transport {
                service.stop();
            }
        }
    }

    fn spawn_service(
        &mut self,
        transport: u64,
        name: String,
        bind: bool,
    ) -> Result<(String, Vec<OwnedFd>)> {
        ensure!(
            self.transports.contains_key(&transport),
            "host is no longer authorized"
        );
        ensure!(
            self.services
                .iter()
                .filter(|s| s.transport == transport)
                .count()
                < config::MAX_STREAMS + 16,
            "service process limit reached"
        );
        ensure!(
            name.len() <= 4096 && !name.contains('\0'),
            "invalid service request"
        );
        if bind {
            super::transport::validate_endpoint(&name, true)?;
        } else if name != "sync:"
            && !name.starts_with("tcp:")
            && !name.starts_with("localabstract:")
            && !name.starts_with("localfilesystem:")
        {
            super::services::ShellArgs::parse(&name)?;
        }
        let (a, b) = ipc::pair()?;
        let (data, child_data) = if bind {
            ipc::pair()?
        } else {
            let (x, y) = UnixStream::pair()?;
            (x.into(), y.into())
        };
        let child = ipc::spawn("service", b.as_raw_fd())?;
        drop(b);
        ipc::send(
            a.as_raw_fd(),
            &ServiceStart {
                name: name.clone(),
                root: self.root,
                test: self.test,
                bind,
            },
            &[child_data.as_raw_fd()],
        )?;
        drop(a);
        drop(child_data);
        let mut service = Service {
            child,
            transport,
            binder: None,
            stopped: false,
            close_at: None,
        };
        if bind {
            let mut pfd = libc::pollfd {
                fd: data.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 3000) } <= 0 {
                service.stop();
                self.services.push(service);
                bail!("reverse bind timeout")
            }
            match ipc::receive::<String>(data.as_raw_fd()) {
                Ok((actual, fds)) => {
                    service.binder = Some((actual.clone(), data));
                    self.services.push(service);
                    Ok((actual, fds))
                }
                Err(e) => {
                    service.stop();
                    self.services.push(service);
                    Err(e)
                }
            }
        } else {
            let id = service.child.id().to_string();
            self.services.push(service);
            Ok((id, vec![data]))
        }
    }

    fn request(&mut self, r: Request) -> Result<(String, Vec<OwnedFd>)> {
        match r {
            Request::Authenticated {
                transport,
                fingerprint,
            } => {
                ensure!(
                    self.transports.len() < config::MAX_TRANSPORTS
                        && !self.transports.contains_key(&transport),
                    "connection limit reached"
                );
                self.store.connected(&fingerprint)?;
                self.transports.insert(transport, fingerprint);
                self.log("Paired host connected");
            }
            Request::Disconnected { transport } => self.disconnect(transport),
            Request::Spawn { transport, name } => {
                return self.spawn_service(transport, name, false);
            }
            Request::CloseService { transport, service } => {
                for s in &mut self.services {
                    if s.transport == transport && s.child.id() == service && s.binder.is_none() {
                        s.close_at = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
            }
            Request::BindReverse { transport, spec } => {
                return self.spawn_service(transport, spec, true);
            }
            Request::CloseReverse { transport, spec } => {
                for service in &mut self.services {
                    if service.transport == transport
                        && service.binder.as_ref().is_some_and(|(s, _)| s == &spec)
                    {
                        service.stop();
                    }
                }
            }
            Request::Root { transport, enable } => {
                ensure!(
                    self.transports.contains_key(&transport),
                    "host is no longer authorized"
                );
                ensure!(
                    !enable || self.store.config.allow_adb_root,
                    "adb root is disabled in altdb"
                );
                if self.root == enable {
                    return Ok((
                        if enable {
                            "new services already use root permissions\n"
                        } else {
                            "new services already use shell permissions\n"
                        }
                        .into(),
                        vec![],
                    ));
                }
                self.root = enable;
                // The stock adb client waits for a disconnect if this says "restarting".
                return Ok((
                    format!(
                        "new services will use {} permissions\n",
                        if enable { "root" } else { "shell" }
                    ),
                    vec![],
                ));
            }
            Request::PairResult { epoch, key } => {
                let w = self
                    .window
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("pairing window closed"))?;
                ensure!(
                    w.epoch == epoch
                        && Instant::now() < w.expires
                        && w.failures < config::MAX_PAIR_FAILURES,
                    "pairing window expired"
                );
                if let Some(key) = key {
                    self.store.pair(key)?;
                    self.window = None;
                    self.send(&Message::PairCommitted, &[])?;
                    self.send(
                        &Message::Hosts {
                            hosts: self.store.hosts.clone(),
                        },
                        &[],
                    )?;
                    self.log("Host paired successfully");
                } else {
                    w.failures += 1;
                    if w.failures >= config::MAX_PAIR_FAILURES {
                        self.pair_stop();
                    }
                    self.log("Pairing authentication failed");
                }
            }
        }
        Ok((String::new(), vec![]))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

pub fn run(path: &Path, test: bool) -> Result<()> {
    ensure!(
        !test || !cfg!(target_os = "android"),
        "test mode is unavailable on Android"
    );
    if !test {
        ksu::check()?;
    }
    STOP.store(false, Ordering::Relaxed);
    unsafe {
        libc::umask(0o077);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, stop_signal as *const () as usize);
        libc::signal(libc::SIGINT, stop_signal as *const () as usize);
    }
    let store = Store::open(path)?;
    let socket_path = path.join("control.sock");
    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }
    let control = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    control.set_nonblocking(true)?;
    let mut d = Daemon {
        store,
        test,
        worker: None,
        bundle: None,
        services: vec![],
        transports: HashMap::new(),
        window: None,
        root: false,
        state: "starting".into(),
        reason: String::new(),
        logs: VecDeque::new(),
        epoch: 0,
        quit: false,
        retry_at: Instant::now(),
        failures: 0,
    };
    let mut property_check = Instant::now() - Duration::from_secs(1);
    let mut network_check = Instant::now() - Duration::from_secs(10);
    // The Android settings command can take hundreds of milliseconds. Keep
    // it off the supervisor's authentication / service-start / control path.
    let native_monitor = if test {
        None
    } else {
        Some(ksu::NativeMonitor::start()?)
    };
    let properties_changed = ksu::watch_properties();
    while !d.quit && !STOP.load(Ordering::Relaxed) {
        if let Ok((mut socket, _)) = control.accept() {
            let result = (|| -> Result<Value> {
                ensure!(
                    test || ipc::peer_uid(socket.as_raw_fd())? == 0,
                    "root required"
                );
                socket.set_read_timeout(Some(Duration::from_millis(500)))?;
                socket.set_write_timeout(Some(Duration::from_millis(500)))?;
                let mut len = [0; 4];
                socket.read_exact(&mut len)?;
                let len = u32::from_le_bytes(len) as usize;
                ensure!(len <= 16384, "control request too large");
                let mut body = vec![0; len];
                socket.read_exact(&mut body)?;
                d.control(serde_json::from_slice(&body)?)
            })();
            let response = match result {
                Ok(value) => json!({"ok": true, "data": value}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            };
            let body = serde_json::to_vec(&response)?;
            let _ = socket.write_all(&(body.len() as u32).to_le_bytes());
            let _ = socket.write_all(&body);
        }
        if d.quit {
            break;
        }
        if d.window
            .as_ref()
            .is_some_and(|w| Instant::now() >= w.expires)
        {
            d.pair_stop();
            d.log("Pairing window expired");
        }
        if let Some(worker) = &d.worker {
            let mut pfd = libc::pollfd {
                fd: worker.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut pfd, 1, 0);
            }
            if pfd.revents & libc::POLLIN != 0 {
                match ipc::receive::<Message>(worker.fd.as_raw_fd()) {
                    Ok((Message::Ready, fds)) if fds.is_empty() => {
                        d.state = "running".into();
                        d.reason.clear();
                        d.log("Service started");
                    }
                    Ok((Message::Request { id, request }, fds)) if fds.is_empty() => {
                        let result = d.request(request);
                        let (ok, message, fds) = match result {
                            Ok((m, f)) => (true, m, f),
                            Err(e) => (false, e.to_string(), vec![]),
                        };
                        let _ = d.send(
                            &Message::Reply { id, ok, message },
                            &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>(),
                        );
                    }
                    _ => d.stop("error", "Network worker communication failed"),
                }
            } else if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                d.stop("error", "Network worker exited; check SELinux and KernelSU");
            }
        }
        d.services.retain_mut(|s| {
            if s.close_at.is_some_and(|t| Instant::now() >= t) {
                s.stop();
            }
            s.child.try_wait().ok().flatten().is_none()
        });
        if properties_changed.swap(false, Ordering::AcqRel)
            || property_check.elapsed() >= Duration::from_millis(250)
        {
            property_check = Instant::now();
            if !d.store.config.enabled {
                d.stop("disabled", "Disabled in settings");
            } else {
                let (reason, waiting_settings) = if test {
                    (None, false)
                } else {
                    match ksu::native_properties() {
                        Ok(Some(reason)) => (Some(reason), false),
                        Ok(None) => native_monitor
                            .as_ref()
                            .unwrap()
                            .status(d.worker.is_some() && d.state == "running"),
                        Err(error) => (
                            Some(format!(
                                "Cannot read system debugging state; service paused: {error}"
                            )),
                            false,
                        ),
                    }
                };
                if let Some(reason) = reason {
                    d.stop("paused_native", &reason);
                } else if waiting_settings {
                    if d.reason != ksu::SETTINGS_UNAVAILABLE {
                        d.reason = ksu::SETTINGS_UNAVAILABLE.into();
                        d.log(ksu::SETTINGS_UNAVAILABLE);
                    }
                    // Keep the worker, sessions, port and interface-bound sockets.
                    // Do not rotate on transient framework network snapshots.
                } else {
                    if d.reason == ksu::SETTINGS_UNAVAILABLE {
                        d.reason.clear();
                        d.log("Debugging settings are available again");
                    }
                    if network_check.elapsed() >= Duration::from_secs(2)
                        && Instant::now() >= d.retry_at
                    {
                        network_check = Instant::now();
                        match network::discover(test) {
                            Ok(addresses) if addresses.is_empty() => {
                                d.stop("waiting_network", "Waiting for Wi-Fi, hotspot or Ethernet")
                            }
                            Ok(addresses) => {
                                let changed = d.bundle.as_ref().is_none_or(|b| {
                                    let mut old = b.endpoints.clone();
                                    for ep in &mut old {
                                        ep.port = 0;
                                    }
                                    old != addresses
                                });
                                if changed {
                                    d.stop("starting", "Network changed; starting service");
                                    if d.start(&addresses).is_err() {
                                        let reason = "Cannot bind port or publish mDNS; check port availability and interface permissions";
                                        d.stop("error", reason);
                                    }
                                }
                            }
                            Err(_) => d.stop("error", "Cannot read network interfaces"),
                        }
                    }
                }
            }
        }
        let mut ready = [
            libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: d.worker.as_ref().map_or(-1, |w| w.fd.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // IPC and CLI requests wake immediately; the timeout bounds property,
        // configuration and service-lifetime checks while otherwise idle.
        unsafe {
            libc::poll(ready.as_mut_ptr(), ready.len() as _, 50);
        }
    }
    d.stop("disabled", "Service stopped");
    drop(control);
    let _ = fs::remove_file(socket_path);
    Ok(())
}

pub fn control(path: &Path, request: &Control) -> Result<Value> {
    let mut socket = UnixStream::connect(path.join("control.sock"))?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let body = serde_json::to_vec(request)?;
    ensure!(body.len() <= 16384, "control request too large");
    socket.write_all(&(body.len() as u32).to_le_bytes())?;
    socket.write_all(&body)?;
    let mut h = [0; 4];
    socket.read_exact(&mut h)?;
    let len = u32::from_le_bytes(h) as usize;
    ensure!(len <= 1024 * 1024, "control response too large");
    let mut data = vec![0; len];
    socket.read_exact(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}
