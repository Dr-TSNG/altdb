use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, SockAddr, SockFilter, Socket, Type};
use std::{
    ffi::CStr,
    fs,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    pub interface: String,
    pub index: u32,
    pub address: Ipv4Addr,
    pub port: u16,
}

impl Endpoint {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.address, self.port))
    }

    pub fn display(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }
}

fn numbered(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

pub fn eligible(name: &str, wireless: bool, physical: bool, ethernet: bool) -> bool {
    if [
        "rmnet", "ccmni", "pdp", "wwan", "tun", "tap", "wg", "ppp", "vti", "ipsec", "dummy", "veth",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        return false;
    }
    ethernet
        && (wireless
            || ["wlan", "swlan", "ap", "wifi"]
                .iter()
                .any(|p| numbered(name, p))
            || physical && (name.starts_with("eth") || name.starts_with("en")))
}

pub fn discover(test: bool) -> Result<Vec<Endpoint>> {
    if test {
        return Ok(vec![Endpoint {
            interface: "lo".into(),
            index: 1,
            address: Ipv4Addr::LOCALHOST,
            port: 0,
        }]);
    }
    let mut first = std::ptr::null_mut();
    ensure!(
        unsafe { libc::getifaddrs(&mut first) } == 0,
        "cannot enumerate interfaces"
    );
    struct Guard(*mut libc::ifaddrs);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) }
        }
    }
    let _guard = Guard(first);
    let mut p = first;
    let mut result = vec![];
    while !p.is_null() {
        let entry = unsafe { &*p };
        p = entry.ifa_next;
        if entry.ifa_addr.is_null()
            || entry.ifa_flags & libc::IFF_UP as u32 == 0
            || entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0
        {
            continue;
        }
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_str()?
            .to_owned();
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            continue;
        }
        let path = Path::new("/sys/class/net").join(&name);
        if !eligible(
            &name,
            path.join("wireless").exists() || path.join("phy80211").exists(),
            path.join("device").exists(),
            fs::read_to_string(path.join("type"))
                .unwrap_or_default()
                .trim()
                == "1",
        ) {
            continue;
        }
        let family = unsafe { (*entry.ifa_addr).sa_family } as i32;
        if family != libc::AF_INET {
            continue;
        }
        let a = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
        let address = Ipv4Addr::from(a.sin_addr.s_addr.to_ne_bytes());
        if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
            continue;
        }
        let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
        if index != 0 {
            result.push(Endpoint {
                interface: name,
                index,
                address,
                port: 0,
            });
        }
    }
    result.sort();
    result.dedup();
    ensure!(result.len() <= 32, "too many eligible interface addresses");
    Ok(result)
}

pub(super) fn restrict_ingress(socket: &Socket, index: u32) -> Result<()> {
    // Locally originated traffic to the device's own IPs arrives via lo.
    // Filter before TCP handles SYN so local scans cannot complete a handshake.
    const SKF_AD_IFINDEX: u32 = (-0x1000i32 + 8) as u32;
    socket.attach_filter(&[
        SockFilter::new(0x20, 0, 0, SKF_AD_IFINDEX), // ld ifidx
        SockFilter::new(0x15, 0, 1, index),          // jeq index, accept, drop
        SockFilter::new(0x06, 0, 0, u32::MAX),       // accept the full packet
        SockFilter::new(0x06, 0, 0, 0),              // drop
    ])?;
    #[cfg(target_os = "android")]
    const SO_LOCK_FILTER: libc::c_int = 44;
    #[cfg(target_os = "linux")]
    use libc::SO_LOCK_FILTER;
    let lock: libc::c_int = 1;
    ensure!(
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_LOCK_FILTER,
                (&lock as *const libc::c_int).cast(),
                std::mem::size_of_val(&lock) as _,
            )
        } == 0,
        "cannot lock ingress filter: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

pub fn listeners(
    addresses: &[Endpoint],
    port: u16,
    test: bool,
) -> Result<(Vec<Endpoint>, Vec<TcpListener>)> {
    let mut result = vec![];
    let mut endpoints = vec![];
    let mut chosen = port;
    for original in addresses {
        let mut ep = original.clone();
        ep.port = chosen;
        let addr = ep.socket_addr();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        if !test {
            socket.bind_device(Some(ep.interface.as_bytes()))?;
            restrict_ingress(&socket, ep.index)?;
        }
        socket.bind(&SockAddr::from(addr))?;
        socket.listen(16)?;
        socket.set_nonblocking(true)?;
        let listener: TcpListener = socket.into();
        chosen = listener.local_addr()?.port();
        ep.port = chosen;
        endpoints.push(ep);
        result.push(listener);
    }
    Ok((endpoints, result))
}

pub fn unix_address(spec: &str) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let (abstract_name, path) = if let Some(s) = spec.strip_prefix("localabstract:") {
        (true, s)
    } else if let Some(s) = spec.strip_prefix("localfilesystem:") {
        (false, s)
    } else {
        anyhow::bail!("unsupported Unix socket")
    };
    ensure!(
        !path.is_empty() && !path.contains('\0'),
        "invalid Unix socket name"
    );
    if !abstract_name {
        ensure!(path.starts_with('/'), "Unix socket path must be absolute");
    }
    let start = usize::from(abstract_name);
    ensure!(
        path.len() + start < addr.sun_path.len(),
        "Unix socket name too long"
    );
    for (i, b) in path.bytes().enumerate() {
        addr.sun_path[i + start] = b as _;
    }
    let size = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        + start
        + path.len()
        + usize::from(!abstract_name);
    Ok((addr, size as _))
}

pub fn unix_socket(spec: &str, bind: bool) -> Result<OwnedFd> {
    let (addr, len) = unix_address(spec)?;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    ensure!(fd >= 0, "Unix socket creation failed");
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let rc = unsafe {
        if bind {
            libc::bind(fd, (&addr as *const libc::sockaddr_un).cast(), len)
        } else {
            libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len)
        }
    };
    ensure!(
        rc == 0,
        "Unix socket operation: {}",
        std::io::Error::last_os_error()
    );
    if bind {
        ensure!(unsafe { libc::listen(fd, 16) } == 0, "Unix listen failed");
    }
    Ok(owned)
}

pub fn reverse_listener(spec: &str) -> Result<(OwnedFd, String)> {
    if let Some(port) = spec.strip_prefix("tcp:") {
        let port = port.parse::<u16>()?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let actual = format!("tcp:{}", listener.local_addr()?.port());
        return Ok((listener.into(), actual));
    }
    Ok((unix_socket(spec, true)?, spec.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interface_policy() {
        assert!(eligible("wlan0", false, false, true));
        assert!(eligible("eth0", false, true, true));
        for n in ["tun0", "rmnet_data0", "wwan0", "veth0", "lo", "random0"] {
            assert!(!eligible(n, false, false, true));
        }
    }
    #[test]
    fn unix_names_are_bounded() {
        assert!(unix_address("localabstract:test").is_ok());
        assert!(unix_address("localfilesystem:/tmp/a").is_ok());
        for n in [
            "localfilesystem:relative",
            "localabstract:",
            "localabstract:a\0b",
        ] {
            assert!(unix_address(n).is_err());
        }
    }
    #[test]
    fn random_port_and_fixed_collision() {
        let addresses = discover(true).unwrap();
        let (ep, _fds) = listeners(&addresses, 0, true).unwrap();
        assert_ne!(ep[0].port, 0);
        assert!(listeners(&addresses, ep[0].port, true).is_err());
        assert!(_fds[0].local_addr().unwrap().is_ipv4());
    }

    #[test]
    fn endpoints_accept_only_ipv4() {
        let mut value = serde_json::json!({
            "interface": "wlan0",
            "index": 3,
            "address": "192.168.1.2",
            "port": 5555,
        });
        let ep: Endpoint = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(ep.display(), "192.168.1.2:5555");
        for address in ["::1", "fe80::1", "::ffff:192.168.1.2"] {
            value["address"] = address.into();
            assert!(serde_json::from_value::<Endpoint>(value.clone()).is_err());
        }
    }
}
