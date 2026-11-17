use super::{
    ipc::{self, Message, Request},
    ksu,
    mdns::Mdns,
    socket::{Listener, Stream},
    transport,
};
use crate::{config::MAX_TRANSPORTS, pairing};
use anyhow::{Result, bail, ensure};
use std::{
    collections::HashMap,
    net::{Shutdown, UdpSocket},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

type Answer = (bool, String, Vec<OwnedFd>);
struct Shared {
    fd: OwnedFd,
    send: Mutex<()>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::SyncSender<Answer>>>,
    peers: Mutex<HashMap<u64, String>>,
}
#[derive(Clone)]
pub struct Broker(Arc<Shared>);
impl Broker {
    fn new(fd: OwnedFd) -> Self {
        Self(Arc::new(Shared {
            fd,
            send: Mutex::new(()),
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
        }))
    }

    pub fn call(&self, request: Request) -> Result<(String, Vec<OwnedFd>)> {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(1);
        let peer = if let Request::Authenticated {
            transport,
            fingerprint,
        } = &request
        {
            Some((*transport, fingerprint.clone()))
        } else {
            None
        };
        // Register before the supervisor reply so a concurrent revocation cannot
        // miss a just-authenticated connection while this thread is waking up.
        if let Some((id, fp)) = &peer {
            self.0.peers.lock().unwrap().insert(*id, fp.clone());
        }
        self.0.pending.lock().unwrap().insert(id, tx);
        let result = (|| -> Result<(String, Vec<OwnedFd>)> {
            {
                let _guard = self.0.send.lock().unwrap();
                ipc::send(
                    self.0.fd.as_raw_fd(),
                    &Message::Request { id, request },
                    &[],
                )?;
            }
            let (ok, message, fds) = rx.recv_timeout(Duration::from_secs(10))?;
            ensure!(ok, "{message}");
            Ok((message, fds))
        })();
        self.0.pending.lock().unwrap().remove(&id);
        if result.is_err()
            && let Some((id, _)) = peer
        {
            self.0.peers.lock().unwrap().remove(&id);
        }
        result
    }

    fn disconnected(&self, transport: u64) -> Result<()> {
        self.0.peers.lock().unwrap().remove(&transport);
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let _guard = self.0.send.lock().unwrap();
        // The worker receives replies itself, so cleanup must not wait for one.
        ipc::send(
            self.0.fd.as_raw_fd(),
            &Message::Request {
                id,
                request: Request::Disconnected { transport },
            },
            &[],
        )
    }
}

struct Running {
    socket: Stream,
    cancel: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
    started: Instant,
}

impl Running {
    fn stop(&self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

struct PairWindow {
    epoch: u64,
    code: Zeroizing<String>,
    listeners: Vec<Listener>,
    active: Option<Running>,
}

impl Drop for PairWindow {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.stop();
        }
    }
}

pub fn run() -> Result<()> {
    ksu::child_setup()?;
    let fd = unsafe { OwnedFd::from_raw_fd(3) };
    let (boot, mut fds) = ipc::receive::<Message>(fd.as_raw_fd())?;
    let Message::Boot {
        identity,
        mut hosts,
        endpoints,
        mdns,
        backend,
        test,
    } = boot
    else {
        bail!("worker bootstrap required")
    };
    ensure!(
        fds.len() == endpoints.len() + mdns.len(),
        "listener descriptor mismatch"
    );
    // Read Android properties before entering the network SELinux domain,
    // which deliberately has no access to the system property files.
    let banner = transport::device_banner();
    ksu::restrict_network(test)?;
    let mdns_sockets: Vec<UdpSocket> = fds.drain(endpoints.len()..).map(Into::into).collect();
    let listeners: Vec<Listener> = fds
        .into_iter()
        .map(|fd| Listener::new(fd, backend))
        .collect::<Result<_>>()?;
    let mut publisher = Mdns::new(
        mdns_sockets,
        mdns,
        endpoints.iter().filter_map(|e| e.tcp().cloned()).collect(),
        identity.guid.clone(),
    );
    let identity = Arc::new(identity);
    let broker = Broker::new(fd);
    let mut connections: HashMap<u64, Running> = HashMap::new();
    let mut next = 1u64;
    let mut pair: Option<PairWindow> = None;
    ipc::send(broker.0.fd.as_raw_fd(), &Message::Ready, &[])?;
    loop {
        let mut pfd = libc::pollfd {
            fd: broker.0.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, 10) };
        if n < 0 {
            break;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
        if pfd.revents & libc::POLLIN != 0 {
            let Ok((message, fds)) = ipc::receive::<Message>(broker.0.fd.as_raw_fd()) else {
                break;
            };
            match message {
                Message::Reply { id, ok, message } => {
                    if let Some(tx) = broker.0.pending.lock().unwrap().remove(&id) {
                        let _ = tx.try_send((ok, message, fds));
                    }
                }
                Message::Hosts { hosts: new } => {
                    hosts = new;
                    let peers = broker.0.peers.lock().unwrap();
                    for (id, running) in &connections {
                        if peers
                            .get(id)
                            .is_some_and(|fp| !hosts.iter().any(|h| &h.fingerprint == fp))
                        {
                            running.stop();
                        }
                    }
                }
                Message::PairStart {
                    epoch,
                    code,
                    endpoints,
                } => {
                    ensure!(fds.len() == endpoints.len(), "pairing listener mismatch");
                    publisher.pairing(endpoints.first().and_then(|e| e.tcp()).map(|e| e.port));
                    pair = Some(PairWindow {
                        epoch,
                        code: Zeroizing::new(code),
                        listeners: fds
                            .into_iter()
                            .map(|fd| Listener::new(fd, backend))
                            .collect::<Result<_>>()?,
                        active: None,
                    });
                }
                Message::PairStop => {
                    pair = None;
                    publisher.pairing(None);
                }
                Message::PairCommitted => {
                    if let Some(w) = &mut pair {
                        w.listeners.clear();
                        w.code.clear();
                    }
                    publisher.pairing(None);
                }
                Message::Shutdown => break,
                _ => bail!("unexpected supervisor message"),
            }
        }
        // Reap every exit path, including a failed/timed-out authentication
        // reply after the supervisor has already registered the connection.
        for (id, _) in connections.extract_if(|_, r| r.handle.is_finished()) {
            broker.disconnected(id)?;
        }
        {
            let peers = broker.0.peers.lock().unwrap();
            for (id, r) in &connections {
                if r.started.elapsed() > Duration::from_secs(10) && !peers.contains_key(id) {
                    r.stop();
                }
            }
        }
        for listener in &listeners {
            if connections.len() >= MAX_TRANSPORTS {
                break;
            }
            if let Ok(socket) = listener.accept() {
                let control = socket.try_clone()?;
                let cancel = Arc::new(AtomicBool::new(false));
                let signal = cancel.clone();
                let identity = identity.clone();
                let banner = banner.clone();
                let b = broker.clone();
                let id = next;
                next = next
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("transport IDs exhausted"))?;
                let allowed: Vec<_> = hosts.iter().map(|h| h.fingerprint.clone()).collect();
                let handle = thread::spawn(move || {
                    if let Err(error) =
                        transport::run(socket, &identity, &allowed, banner, b, id, signal)
                        && test
                    {
                        eprintln!("test transport: {error:#}");
                    }
                });
                connections.insert(
                    id,
                    Running {
                        socket: control,
                        cancel,
                        handle,
                        started: Instant::now(),
                    },
                );
            }
        }
        if let Some(window) = &mut pair {
            if let Some(active) = &window.active
                && active.started.elapsed() > Duration::from_secs(20)
            {
                active.stop();
            }
            if window
                .active
                .as_ref()
                .is_some_and(|r| r.handle.is_finished())
            {
                window.active = None;
            }
            if window.active.is_none() {
                for listener in &window.listeners {
                    if let Ok(socket) = listener.accept() {
                        let control = socket.try_clone()?;
                        let code = window.code.clone();
                        let epoch = window.epoch;
                        let identity = identity.clone();
                        let b = broker.clone();
                        let handle = thread::spawn(move || {
                            if pairing::pair_with_commit(socket, &identity, &code, |key| {
                                b.call(Request::PairResult {
                                    epoch,
                                    key: Some(key.into()),
                                })?;
                                Ok(())
                            })
                            .is_err()
                            {
                                let _ = b.call(Request::PairResult { epoch, key: None });
                            }
                        });
                        window.active = Some(Running {
                            socket: control,
                            cancel: Arc::new(AtomicBool::new(false)),
                            handle,
                            started: Instant::now(),
                        });
                        break;
                    }
                }
            }
        }
        publisher.tick();
    }
    drop(pair);
    for running in connections.values() {
        running.stop();
    }
    drop(publisher);
    broker.0.pending.lock().unwrap().clear();
    Ok(())
}
