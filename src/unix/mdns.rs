use super::network::{Endpoint, restrict_ingress};
use anyhow::{Result, ensure};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    collections::BTreeSet,
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

pub const CONNECT: &str = "_adb-tls-connect._tcp.local";
pub const PAIR: &str = "_adb-tls-pairing._tcp.local";

pub fn sockets(endpoints: &[Endpoint], test: bool) -> Result<(Vec<Endpoint>, Vec<UdpSocket>)> {
    if test {
        return Ok((vec![], vec![]));
    }
    let mut seen = BTreeSet::new();
    let mut specs = vec![];
    let mut sockets = vec![];
    for ep in endpoints {
        if !seen.insert(ep.index) {
            continue;
        }
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 5353));
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.bind_device(Some(ep.interface.as_bytes()))?;
        restrict_ingress(&socket, ep.index)?;
        socket.bind(&SockAddr::from(addr))?;
        socket.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 251), &ep.address)?;
        socket.set_multicast_if_v4(&ep.address)?;
        socket.set_multicast_ttl_v4(255)?;
        socket.set_multicast_loop_v4(false)?;
        socket.set_nonblocking(true)?;
        specs.push(ep.clone());
        sockets.push(socket.into());
    }
    Ok((specs, sockets))
}

fn name(out: &mut Vec<u8>, n: &str) {
    for label in n.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn record(out: &mut Vec<u8>, n: &str, kind: u16, ttl: u32, data: &[u8]) {
    name(out, n);
    out.extend(kind.to_be_bytes());
    out.extend((if kind == 12 { 1u16 } else { 0x8001 }).to_be_bytes());
    out.extend(ttl.to_be_bytes());
    out.extend((data.len() as u16).to_be_bytes());
    out.extend(data);
}

pub fn response(guid: &str, endpoints: &[Endpoint], pair: Option<u16>, ttl: u32) -> Vec<u8> {
    let mut out = vec![0; 12];
    out[2] = 0x84;
    let hostname = format!("{guid}.local");
    let mut count = 0u16;
    if let Some(ep) = endpoints.first() {
        for (service, port) in [(CONNECT, Some(ep.port)), (PAIR, pair)] {
            if let Some(port) = port {
                let instance = format!("{guid}.{service}");
                let mut target = vec![];
                name(&mut target, &instance);
                record(&mut out, service, 12, ttl, &target);
                let mut srv = vec![0, 0, 0, 0];
                srv.extend(port.to_be_bytes());
                name(&mut srv, &hostname);
                record(&mut out, &instance, 33, ttl, &srv);
                record(&mut out, &instance, 16, ttl, &[0]);
                count += 3;
            }
        }
    }
    for ep in endpoints {
        record(&mut out, &hostname, 1, ttl, &ep.address.octets());
        count += 1;
    }
    out[6..8].copy_from_slice(&count.to_be_bytes());
    out
}

pub fn query_name(data: &[u8], offset: &mut usize) -> Result<String> {
    let mut p = *offset;
    let mut next = None;
    let mut labels = vec![];
    let mut steps = 0;
    let mut total = 0;
    loop {
        steps += 1;
        ensure!(steps <= 64 && p < data.len(), "invalid DNS name");
        let len = data[p] as usize;
        p += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 == 0xc0 {
            ensure!(p < data.len(), "truncated DNS pointer");
            next.get_or_insert(p + 1);
            p = ((len & 0x3f) << 8) | data[p] as usize;
            continue;
        }
        ensure!(len <= 63 && p + len <= data.len(), "invalid DNS label");
        total += len + 1;
        ensure!(total <= 255, "DNS name too long");
        labels.push(std::str::from_utf8(&data[p..p + len])?.to_ascii_lowercase());
        p += len;
    }
    *offset = next.unwrap_or(p);
    Ok(labels.join("."))
}

fn relevant(data: &[u8], guid: &str) -> bool {
    if data.len() < 12 || data[2] & 0x80 != 0 {
        return false;
    }
    let count = u16::from_be_bytes([data[4], data[5]]) as usize;
    if count > 64 {
        return false;
    }
    let mut p = 12;
    for _ in 0..count {
        let Ok(q) = query_name(data, &mut p) else {
            return false;
        };
        if p + 4 > data.len() {
            return false;
        }
        p += 4;
        if q == CONNECT
            || q == PAIR
            || q == format!("{guid}.local")
            || q == format!("{guid}.{CONNECT}")
            || q == format!("{guid}.{PAIR}")
        {
            return true;
        }
    }
    false
}

pub struct Mdns {
    pub sockets: Vec<UdpSocket>,
    pub specs: Vec<Endpoint>,
    pub endpoints: Vec<Endpoint>,
    pub guid: String,
    pub pair: Option<u16>,
    last: Instant,
    last_reply: Instant,
}

impl Mdns {
    pub fn new(
        sockets: Vec<UdpSocket>,
        specs: Vec<Endpoint>,
        endpoints: Vec<Endpoint>,
        guid: String,
    ) -> Self {
        Self {
            sockets,
            specs,
            endpoints,
            guid,
            pair: None,
            last: Instant::now() - Duration::from_secs(120),
            last_reply: Instant::now() - Duration::from_secs(1),
        }
    }

    fn packet(&self, spec: &Endpoint, ttl: u32) -> Vec<u8> {
        let eps: Vec<_> = self
            .endpoints
            .iter()
            .filter(|e| e.index == spec.index)
            .cloned()
            .collect();
        response(&self.guid, &eps, self.pair, ttl)
    }

    pub fn announce(&mut self, ttl: u32) {
        for (socket, spec) in self.sockets.iter().zip(&self.specs) {
            let target = SocketAddr::from(([224, 0, 0, 251], 5353));
            let _ = socket.send_to(&self.packet(spec, ttl), target);
        }
        self.last = Instant::now();
    }

    pub fn pairing(&mut self, port: Option<u16>) {
        self.announce(0);
        self.pair = port;
        self.announce(120);
    }

    pub fn tick(&mut self) {
        if self.last.elapsed() >= Duration::from_secs(60) {
            self.announce(120);
        }
        let mut buffer = [0; 9000];
        let mut reply = false;
        for socket in &self.sockets {
            for _ in 0..16 {
                match socket.recv_from(&mut buffer) {
                    Ok((n, _)) => {
                        reply |= relevant(&buffer[..n], &self.guid);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        if reply && self.last_reply.elapsed() >= Duration::from_millis(250) {
            self.announce(120);
            self.last_reply = Instant::now();
        }
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        self.announce(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_pointer_loop() {
        let mut p = 0;
        assert!(query_name(&[0xc0, 0], &mut p).is_err());
    }
    #[test]
    fn advertisement_has_expected_records() {
        let ep = Endpoint {
            interface: "wlan0".into(),
            index: 3,
            address: Ipv4Addr::new(192, 168, 1, 2),
            port: 5555,
        };
        let b = response("altdb-test", &[ep], Some(32000), 120);
        assert_eq!(u16::from_be_bytes([b[6], b[7]]), 7);
        assert!(
            b.windows(CONNECT.split('.').next().unwrap().len())
                .any(|s| s == b"_adb-tls-connect")
        );
        let mut p = 12;
        let mut kinds = vec![];
        for _ in 0..7 {
            query_name(&b, &mut p).unwrap();
            let kind = u16::from_be_bytes([b[p], b[p + 1]]);
            let size = u16::from_be_bytes([b[p + 8], b[p + 9]]) as usize;
            kinds.push(kind);
            p += 10;
            if kind == 1 {
                assert_eq!(&b[p..p + size], &[192, 168, 1, 2]);
            }
            p += size;
        }
        assert_eq!(kinds, [12, 33, 16, 12, 33, 16, 1]);
        assert_eq!(p, b.len());
    }
}
