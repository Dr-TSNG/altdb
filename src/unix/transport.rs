use super::{ipc::Request, worker::Broker};
use crate::{
    config::MAX_STREAMS,
    crypto::{Identity, Tls},
    protocol::*,
};
use anyhow::{Result, bail, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Cursor, Read, Write},
    net::TcpStream,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

struct ServiceSocket {
    socket: UnixStream,
    owner: Option<(Broker, u64, u32)>,
}

impl Drop for ServiceSocket {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some((broker, transport, service)) = &self.owner {
            let _ = broker.call(Request::CloseService {
                transport: *transport,
                service: *service,
            });
        }
    }
}

enum Source {
    Socket(ServiceSocket),
    Bytes(Cursor<Vec<u8>>),
}

impl Source {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Socket(s) => s.socket.read(b),
            Self::Bytes(s) => s.read(b),
        }
    }

    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Self::Socket(s) => s.socket.write(b),
            Self::Bytes(_) => Err(io::Error::other("read-only service")),
        }
    }
}

struct Stream {
    remote: u32,
    source: Source,
    pending: VecDeque<Vec<u8>>,
    offset: usize,
    waiting: bool,
    opening: Option<Instant>,
    closing: Option<Instant>,
}

impl Stream {
    fn enqueue(&mut self, data: Vec<u8>) -> Result<()> {
        ensure!(self.pending.len() < 64, "stream pending packet limit");
        self.pending.push_back(data);
        Ok(())
    }

    fn flush_pending(&mut self) -> io::Result<bool> {
        let mut work = false;
        while let Some(data) = self.pending.front() {
            match self.source.write(&data[self.offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    work = true;
                    if self.offset == data.len() {
                        self.pending.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(work)
    }
}

struct Reverse {
    listener: OwnedFd,
    target: String,
}

pub fn device_banner() -> Vec<u8> {
    let mut banner = String::from("device::");
    for name in ["ro.product.name", "ro.product.model", "ro.product.device"] {
        banner.push_str(name);
        banner.push('=');
        banner.push_str(&super::ksu::property(name).unwrap_or_default());
        banner.push(';');
    }
    banner.push_str(&format!("features={FEATURES};\0"));
    banner.into_bytes()
}

pub fn run(
    mut socket: TcpStream,
    identity: &Identity,
    allowed: &[String],
    banner: Vec<u8>,
    broker: Broker,
    transport: u64,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let hello = Packet::read(&mut socket, 4096, false)?;
    ensure!(
        hello.command == CNXN && hello.arg0 >= STLS_VERSION && hello.arg1 >= 4096,
        "TLS-only ADB requires CNXN"
    );
    let max = (hello.arg1 as usize).min(MAX_PAYLOAD);
    Packet::new(STLS, STLS_VERSION, 0, vec![]).write(&mut socket, false)?;
    let stls = Packet::read(&mut socket, 4096, false)?;
    ensure!(
        stls.command == STLS && stls.arg0 == STLS_VERSION && stls.arg1 == 0 && stls.data.is_empty(),
        "TLS upgrade required"
    );
    let mut tls = Tls::accept(socket, identity, allowed, false)?;
    let fp = tls.peer();
    broker.call(Request::Authenticated {
        transport,
        fingerprint: fp,
    })?;
    let result = (|| -> Result<()> {
        Packet::new(CNXN, VERSION, max as u32, banner).write(&mut tls, false)?;
        tls.flush()?;
        tls.nonblocking()?;
        let mut state = Connection {
            tls,
            broker: broker.clone(),
            transport,
            max,
            streams: BTreeMap::new(),
            reverse: BTreeMap::new(),
            next: 1,
            input: vec![],
        };
        while !cancel.load(Ordering::Acquire) {
            let mut work = false;
            let mut buf = [0; CHUNK];
            for _ in 0..16 {
                match state.tls.read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        state.input.extend_from_slice(&buf[..n]);
                        ensure!(
                            state.input.len() <= MAX_PAYLOAD + 24 + CHUNK,
                            "input queue limit"
                        );
                        work = true;
                        state.parse()?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            work |= state.services()?;
            work |= state.reverse_accept()?;
            match state.tls.flush() {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if !work {
                state.wait_for_io()?;
            }
        }
        Ok(())
    })();
    let _ = broker.call(Request::Disconnected { transport });
    result
}

struct Connection {
    tls: Tls,
    broker: Broker,
    transport: u64,
    max: usize,
    streams: BTreeMap<u32, Stream>,
    reverse: BTreeMap<String, Reverse>,
    next: u32,
    input: Vec<u8>,
}

impl Connection {
    fn wait_for_io(&self) -> io::Result<()> {
        let mut fds = vec![libc::pollfd {
            fd: self.tls.socket().as_raw_fd(),
            events: libc::POLLIN
                | if self.tls.queued() > 0 {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        }];
        for stream in self.streams.values() {
            if let Source::Socket(source) = &stream.source {
                let mut events = 0;
                if !stream.pending.is_empty() {
                    events |= libc::POLLOUT;
                }
                if stream.opening.is_none()
                    && stream.closing.is_none()
                    && !stream.waiting
                    && self.tls.queued() < 4 * 1024 * 1024
                {
                    events |= libc::POLLIN;
                }
                // Do not poll an ACK-blocked stream, including for HUP: its
                // last output must be acknowledged before CLSE is delivered.
                if events != 0 {
                    fds.push(libc::pollfd {
                        fd: source.socket.as_raw_fd(),
                        events,
                        revents: 0,
                    });
                }
            }
        }
        if self.streams.len() < MAX_STREAMS {
            for reverse in self.reverse.values() {
                fds.push(libc::pollfd {
                    fd: reverse.listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        // Network and service readiness wake immediately. A bounded timeout
        // handles pending stream-open / close deadlines even with no traffic.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }

    fn packet(&mut self, command: u32, a: u32, b: u32, data: impl Into<Vec<u8>>) -> Result<()> {
        Packet::new(command, a, b, data).write(&mut self.tls, false)?;
        Ok(())
    }

    fn allocate(&mut self) -> Result<u32> {
        ensure!(self.streams.len() < MAX_STREAMS, "stream limit reached");
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("stream IDs exhausted"))?;
        Ok(id)
    }

    fn parse(&mut self) -> Result<()> {
        loop {
            if self.input.len() < 24 {
                return Ok(());
            }
            let len = u32::from_le_bytes(self.input[12..16].try_into().unwrap()) as usize;
            ensure!(len <= self.max, "ADB packet exceeds negotiated limit");
            if self.input.len() < 24 + len {
                return Ok(());
            }
            let packet = Packet::read(&mut &self.input[..24 + len], self.max, false)?;
            self.input.drain(..24 + len);
            self.handle(packet)?;
        }
    }

    fn handle(&mut self, p: Packet) -> Result<()> {
        match p.command {
            OPEN => {
                ensure!(p.arg0 != 0 && p.arg1 == 0, "invalid OPEN IDs");
                ensure!(
                    !self.streams.values().any(|s| s.remote == p.arg0),
                    "duplicate remote stream ID"
                );
                if self.streams.len() >= MAX_STREAMS {
                    return self.packet(CLSE, 0, p.arg0, vec![]);
                }
                let result = self.open(&p.data);
                match result {
                    Ok(source) => {
                        let id = self.allocate()?;
                        self.streams.insert(
                            id,
                            Stream {
                                remote: p.arg0,
                                source,
                                pending: VecDeque::new(),
                                offset: 0,
                                waiting: false,
                                opening: None,
                                closing: None,
                            },
                        );
                        self.packet(OKAY, id, p.arg0, vec![])?;
                    }
                    Err(_) => self.packet(CLSE, 0, p.arg0, vec![])?,
                }
            }
            OKAY => {
                if let Some(s) = self.streams.get_mut(&p.arg1) {
                    ensure!(p.data.is_empty() && p.arg0 != 0, "invalid acknowledgement");
                    if s.opening.take().is_some() {
                        ensure!(s.remote == 0, "unexpected open acknowledgement");
                        s.remote = p.arg0;
                    } else {
                        ensure!(
                            s.remote == p.arg0 && s.waiting,
                            "unexpected acknowledgement"
                        );
                        s.waiting = false;
                    }
                }
            }
            WRTE => {
                let queued: usize = self
                    .streams
                    .values()
                    // Include consumed prefixes until their packet is freed,
                    // bounding retained buffers as well as unwritten bytes.
                    .flat_map(|s| s.pending.iter())
                    .map(Vec::len)
                    .sum();
                ensure!(
                    queued + p.data.len() <= 8 * 1024 * 1024,
                    "stream input queue limit"
                );
                if let Some(s) = self.streams.get_mut(&p.arg1) {
                    ensure!(
                        s.remote == p.arg0 && s.opening.is_none() && s.closing.is_none(),
                        "invalid stream write ordering"
                    );
                    if p.data.is_empty() {
                        self.packet(OKAY, p.arg1, p.arg0, vec![])?;
                    } else {
                        // Stock adb can deliver another WRTE while an earlier
                        // packet is only partly drained. Keep packet ownership
                        // and the partial-write offset instead of rejecting or
                        // overwriting the earlier data.
                        s.enqueue(p.data)?;
                    }
                } else {
                    self.packet(CLSE, p.arg1, p.arg0, vec![])?;
                }
            }
            CLSE => {
                if let Some(s) = self.streams.get_mut(&p.arg1) {
                    ensure!(
                        p.arg0 == s.remote || (s.opening.is_some() && p.arg0 == 0),
                        "invalid close ID"
                    );
                    s.closing = Some(Instant::now());
                }
            }
            _ => bail!("unsupported ADB command after authentication"),
        }
        Ok(())
    }

    fn open(&mut self, data: &[u8]) -> Result<Source> {
        ensure!(
            !data.is_empty() && data.len() <= 4096 && data.last() == Some(&0),
            "invalid service name"
        );
        let name = std::str::from_utf8(&data[..data.len() - 1])?;
        ensure!(!name.contains('\0'), "invalid service name");
        if name == "root:" || name == "unroot:" {
            let message = match self.broker.call(Request::Root {
                transport: self.transport,
                enable: name == "root:",
            }) {
                Ok((message, _)) => message,
                Err(e) => format!("altdb: {e}\n"),
            };
            return Ok(Source::Bytes(Cursor::new(message.into_bytes())));
        }
        if let Some(command) = name.strip_prefix("reverse:") {
            let body = match self.reverse_command(command) {
                Ok(body) => body,
                Err(_) => {
                    let m = "reverse operation rejected";
                    format!("FAIL{:04x}{m}", m.len()).into_bytes()
                }
            };
            return Ok(Source::Bytes(Cursor::new(body)));
        }
        let (id, mut fds) = self.broker.call(Request::Spawn {
            transport: self.transport,
            name: name.into(),
        })?;
        ensure!(fds.len() == 1, "service descriptor unavailable");
        let socket: UnixStream = fds.remove(0).into();
        socket.set_nonblocking(true)?;
        Ok(Source::Socket(ServiceSocket {
            socket,
            owner: Some((self.broker.clone(), self.transport, id.parse()?)),
        }))
    }

    fn services(&mut self) -> Result<bool> {
        let mut events = vec![];
        let mut closed = vec![];
        let mut work = false;
        for (&id, s) in &mut self.streams {
            if s.opening
                .is_some_and(|t| t.elapsed() > Duration::from_secs(10))
            {
                closed.push(id);
                continue;
            }
            let had_pending = !s.pending.is_empty();
            match s.flush_pending() {
                Ok(progress) => work |= progress,
                Err(_) => {
                    closed.push(id);
                    continue;
                }
            }
            if had_pending && s.pending.is_empty() && s.closing.is_none() {
                events.push(Packet::new(OKAY, id, s.remote, vec![]));
            }
            // exec-in can send CLSE immediately after its last WRTE. Drain that
            // write to the service before delivering EOF or terminating it.
            if let Some(t) = s.closing {
                if s.pending.is_empty() || t.elapsed() > Duration::from_secs(1) {
                    closed.push(id);
                }
                continue;
            }
            if s.opening.is_none() && !s.waiting && self.tls.queued() < 4 * 1024 * 1024 {
                let mut data = vec![0; self.max.min(CHUNK)];
                match s.source.read(&mut data) {
                    Ok(0) => closed.push(id),
                    Ok(n) => {
                        data.truncate(n);
                        events.push(Packet::new(WRTE, id, s.remote, data));
                        s.waiting = true;
                        work = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => closed.push(id),
                }
            }
        }
        for packet in events {
            packet.write(&mut self.tls, false)?;
        }
        for id in closed {
            if let Some(s) = self.streams.remove(&id) {
                self.packet(CLSE, id, s.remote, vec![])?;
                work = true;
            }
        }
        Ok(work)
    }

    fn reverse_command(&mut self, command: &str) -> Result<Vec<u8>> {
        if command == "list-forward" {
            let mut list = String::new();
            for (local, r) in &self.reverse {
                list.push_str(&format!("altdb {local} {}\n", r.target));
            }
            return Ok(format!("{:04x}{list}", list.len()).into_bytes());
        }
        if command == "killforward-all" {
            let keys: Vec<_> = self.reverse.keys().cloned().collect();
            for k in keys {
                self.remove_reverse(&k)?;
            }
            return Ok(b"OKAY".to_vec());
        }
        if let Some(spec) = command.strip_prefix("killforward:") {
            self.remove_reverse(spec)?;
            return Ok(b"OKAY".to_vec());
        }
        let command = command
            .strip_prefix("forward:")
            .ok_or_else(|| anyhow::anyhow!("unsupported reverse command"))?;
        let (no_rebind, command) = if let Some(c) = command.strip_prefix("norebind:") {
            (true, c)
        } else {
            (false, command)
        };
        let (local, remote) = command
            .split_once(';')
            .ok_or_else(|| anyhow::anyhow!("invalid reverse specification"))?;
        validate_endpoint(local, true)?;
        validate_endpoint(remote, false)?;
        if self.reverse.contains_key(local) {
            ensure!(!no_rebind, "reverse listener already exists");
            self.remove_reverse(local)?;
        }
        ensure!(self.reverse.len() < 16, "reverse listener limit reached");
        let (actual, mut fds) = self.broker.call(Request::BindReverse {
            transport: self.transport,
            spec: local.into(),
        })?;
        ensure!(fds.len() == 1, "reverse listener missing");
        let fd = fds.remove(0);
        super::nonblocking(&fd)?;
        self.reverse.insert(
            actual.clone(),
            Reverse {
                listener: fd,
                target: remote.into(),
            },
        );
        if let Some(port) = actual.strip_prefix("tcp:") {
            Ok(format!("OKAY{:04x}{port}", port.len()).into_bytes())
        } else {
            Ok(b"OKAY".to_vec())
        }
    }

    fn remove_reverse(&mut self, spec: &str) -> Result<()> {
        ensure!(
            self.reverse.remove(spec).is_some(),
            "reverse listener not found"
        );
        self.broker.call(Request::CloseReverse {
            transport: self.transport,
            spec: spec.into(),
        })?;
        Ok(())
    }

    fn reverse_accept(&mut self) -> Result<bool> {
        if self.streams.len() >= MAX_STREAMS {
            return Ok(false);
        }
        let mut sockets = vec![];
        for reverse in self.reverse.values() {
            let fd = unsafe {
                libc::accept4(
                    reverse.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if fd >= 0 {
                sockets.push((unsafe { OwnedFd::from_raw_fd(fd) }, reverse.target.clone()));
            }
        }
        let work = !sockets.is_empty();
        for (fd, target) in sockets {
            if self.streams.len() >= MAX_STREAMS {
                break;
            }
            // UnixStream's read/write/shutdown operate on any connected stream FD.
            let source = Source::Socket(ServiceSocket {
                socket: UnixStream::from(fd),
                owner: None,
            });
            let id = self.allocate()?;
            self.streams.insert(
                id,
                Stream {
                    remote: 0,
                    source,
                    pending: VecDeque::new(),
                    offset: 0,
                    waiting: false,
                    opening: Some(Instant::now()),
                    closing: None,
                },
            );
            self.packet(OPEN, id, 0, format!("{target}\0").into_bytes())?;
        }
        Ok(work)
    }
}

pub fn validate_endpoint(spec: &str, zero: bool) -> Result<()> {
    ensure!(
        spec.len() <= 1024 && !spec.contains(['\0', ';', '\n', '\r']),
        "invalid socket endpoint"
    );
    if let Some(port) = spec.strip_prefix("tcp:") {
        let port = port.parse::<u16>()?;
        ensure!(zero || port != 0, "zero destination port");
        return Ok(());
    }
    super::network::unix_address(spec)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(source: Source) -> Stream {
        Stream {
            remote: 1,
            source,
            pending: VecDeque::new(),
            offset: 0,
            waiting: false,
            opening: None,
            closing: None,
        }
    }

    #[test]
    fn subsequent_write_preserves_partially_drained_data_before_close() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        socket2::SockRef::from(&writer)
            .set_send_buffer_size(1024)
            .unwrap();
        writer.set_nonblocking(true).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut s = stream(Source::Socket(ServiceSocket {
            socket: writer,
            owner: None,
        }));
        let first = vec![0x13; 64 * 1024];
        let second = vec![0x27; 777];
        let expected = [first.clone(), second.clone()].concat();
        s.enqueue(first).unwrap();
        assert!(s.flush_pending().unwrap());
        assert!(s.offset > 0 && !s.pending.is_empty());
        s.enqueue(second).unwrap();
        s.closing = Some(Instant::now());
        let reader_thread = std::thread::spawn(move || {
            let mut received = vec![0; expected.len()];
            reader.read_exact(&mut received).unwrap();
            assert_eq!(received, expected);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !s.pending.is_empty() {
            assert!(Instant::now() < deadline);
            if !s.flush_pending().unwrap() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(s.offset, 0);
        reader_thread.join().unwrap();
    }

    #[test]
    fn tiny_pending_packets_cannot_grow_metadata_without_bound() {
        let mut s = stream(Source::Bytes(Cursor::new(vec![])));
        for _ in 0..64 {
            s.enqueue(vec![1]).unwrap();
        }
        assert!(s.enqueue(vec![2]).is_err());
        assert_eq!(s.pending.len(), 64);
    }
}
