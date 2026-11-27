//! Stream sockets shared by TCP and the guest-to-host vsock transport.
use super::network;
use crate::config::Backend;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use socket2::{Domain, SockAddr, SockAddrStorage, Socket, TcpKeepalive, Type};
use std::{
    fs::File,
    io::{self, Read, Write},
    mem,
    net::{Shutdown, TcpStream},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Endpoint {
    Tcp(network::Endpoint),
    Vsock { cid: u32, port: u32 },
}

impl Endpoint {
    pub fn display(&self) -> String {
        match self {
            Self::Tcp(ep) => ep.display(),
            Self::Vsock { cid, port } => format!("vsock:{cid}:{port}"),
        }
    }

    pub fn tcp(&self) -> Option<&network::Endpoint> {
        match self {
            Self::Tcp(ep) => Some(ep),
            Self::Vsock { .. } => None,
        }
    }
}

// Linux UAPI sockaddr_vm; explicit layout also supports Android libc versions
// that do not expose the vsock address type. All integer fields are host endian.
#[repr(C)]
#[derive(Clone, Copy)]
struct VsockAddress {
    family: libc::sa_family_t,
    reserved: u16,
    port: u32,
    cid: u32,
    zero: [u8; 4],
}

fn address(cid: u32, port: u32) -> SockAddr {
    let mut storage = SockAddrStorage::zeroed();
    let vm = VsockAddress {
        family: libc::AF_VSOCK as _,
        reserved: 0,
        port,
        cid,
        zero: [0; 4],
    };
    unsafe {
        *storage.view_as::<VsockAddress>() = vm;
        SockAddr::new(storage, mem::size_of::<VsockAddress>() as _)
    }
}

fn vsock_address(addr: &SockAddr) -> io::Result<VsockAddress> {
    if addr.len() as usize != mem::size_of::<VsockAddress>()
        || i32::from(addr.family()) != libc::AF_VSOCK
    {
        return Err(io::Error::other("expected a vsock endpoint"));
    }
    Ok(unsafe { addr.as_ptr().cast::<VsockAddress>().read_unaligned() })
}

fn local_cid() -> Result<u32> {
    let device = File::open("/dev/vsock").context("Cannot open vsock device")?;
    let mut cid = 0u32;
    // IOCTL_VM_SOCKETS_GET_LOCAL_CID = _IO(7, 0xb9).
    ensure!(
        unsafe { libc::ioctl(device.as_raw_fd(), 0x7b9, &mut cid) } == 0,
        "Cannot read vsock CID: {}",
        io::Error::last_os_error()
    );
    ensure!(
        cid > 2 && cid != u32::MAX,
        "vsock backend requires a guest CID"
    );
    Ok(cid)
}

pub fn listeners(
    backend: Backend,
    addresses: &[network::Endpoint],
    port: u16,
    test: bool,
) -> Result<(Vec<Endpoint>, Vec<OwnedFd>)> {
    if backend == Backend::Tcp {
        let (endpoints, listeners) = network::listeners(addresses, port, test)?;
        return Ok((
            endpoints.into_iter().map(Endpoint::Tcp).collect(),
            listeners.into_iter().map(Into::into).collect(),
        ));
    }
    let cid = local_cid()?;
    let socket = Socket::new(Domain::from(libc::AF_VSOCK), Type::STREAM, None)
        .context("Cannot create vsock listener")?;
    socket.bind(&address(
        u32::MAX,
        if port == 0 { u32::MAX } else { port.into() },
    ))?;
    socket.listen(16)?;
    socket.set_nonblocking(true)?;
    let port = vsock_address(&socket.local_addr()?)?.port;
    Ok((vec![Endpoint::Vsock { cid, port }], vec![socket.into()]))
}

pub struct Listener {
    socket: Socket,
    backend: Backend,
}

impl Listener {
    pub fn new(fd: OwnedFd, backend: Backend) -> Result<Self> {
        let socket = Socket::from(fd);
        let addr = socket.local_addr()?;
        match backend {
            Backend::Tcp => ensure!(addr.as_socket_ipv4().is_some(), "expected TCP listener"),
            Backend::Vsock => {
                vsock_address(&addr)?;
            }
        }
        Ok(Self { socket, backend })
    }

    pub fn accept(&self) -> io::Result<Stream> {
        let (socket, peer) = self.socket.accept()?;
        if self.backend == Backend::Vsock && vsock_address(&peer)?.cid != 2 {
            // Only the VM host may connect.
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "vsock peer is not the host",
            ));
        }
        socket.set_nonblocking(false)?;
        Ok(Stream {
            socket,
            backend: self.backend,
        })
    }
}

pub struct Stream {
    socket: Socket,
    backend: Backend,
}

impl From<TcpStream> for Stream {
    fn from(socket: TcpStream) -> Self {
        Self {
            socket: socket.into(),
            backend: Backend::Tcp,
        }
    }
}

impl Stream {
    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn nodelay(&self) -> io::Result<()> {
        if self.backend == Backend::Tcp {
            self.socket.set_tcp_nodelay(true)?;
        }
        Ok(())
    }

    pub fn keepalive(&self) -> io::Result<()> {
        if self.backend == Backend::Tcp {
            self.socket.set_tcp_keepalive(
                &TcpKeepalive::new()
                    .with_time(Duration::from_secs(30))
                    .with_interval(Duration::from_secs(10))
                    .with_retries(3),
            )?;
            self.socket
                .set_tcp_user_timeout(Some(Duration::from_secs(60)))?;
        }
        Ok(())
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_write_timeout(timeout)
    }
    pub fn set_nonblocking(&self, value: bool) -> io::Result<()> {
        self.socket.set_nonblocking(value)
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.socket.shutdown(how)
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            socket: self.socket.try_clone()?,
            backend: self.backend,
        })
    }
}

impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}
impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (&self.socket).read(buffer)
    }
}
impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        (&self.socket).write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn vsock_address_and_ephemeral_port_keep_all_32_bits() {
        let value = vsock_address(&address(7, u32::MAX)).unwrap();
        assert_eq!((value.cid, value.port), (7, u32::MAX));
        let ep = Endpoint::Vsock {
            cid: 7,
            port: 0x80000000,
        };
        assert_eq!(
            serde_json::from_str::<Endpoint>(&serde_json::to_string(&ep).unwrap()).unwrap(),
            ep
        );
        assert_eq!(ep.display(), "vsock:7:2147483648");
        assert!(ep.tcp().is_none());
    }

    #[test]
    fn non_tcp_stream_skips_tcp_options_and_supports_cancellation() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut stream = Stream {
            socket: Socket::from(OwnedFd::from(a)),
            backend: Backend::Vsock,
        };
        stream.nodelay().unwrap();
        stream.keepalive().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let cancel = stream.try_clone().unwrap();
        stream.write_all(b"TLS bytes").unwrap();
        let mut data = [0; 9];
        b.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"TLS bytes");
        cancel.shutdown(Shutdown::Both).unwrap();
        assert_eq!(b.read(&mut data).unwrap(), 0);
    }

    #[test]
    fn listener_descriptor_must_match_selected_backend() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(Listener::new(listener.into(), Backend::Vsock).is_err());
    }
}
