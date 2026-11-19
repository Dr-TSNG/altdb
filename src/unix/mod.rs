pub mod daemon;
mod environment;
pub mod ipc;
pub mod ksu;
pub mod mdns;
pub mod network;
pub mod pty;
pub mod services;
pub mod socket;
pub mod store;
pub mod transport;
pub mod worker;

use anyhow::Result;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub fn duplicate(fd: RawFd) -> Result<OwnedFd> {
    let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn nonblocking(fd: &impl AsRawFd) -> Result<()> {
    let n = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(n, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(n, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
