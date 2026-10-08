use super::{environment, network::Endpoint};
use crate::{config::Host, crypto::Identity};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(test)]
use std::os::fd::AsRawFd;
use std::{
    io, mem,
    os::fd::{FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};

pub const MAX_MESSAGE: usize = 65536;
pub const MAX_FDS: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Ready,
    Boot {
        identity: Identity,
        hosts: Vec<Host>,
        endpoints: Vec<Endpoint>,
        mdns: Vec<Endpoint>,
        test: bool,
    },
    Request {
        id: u64,
        request: Request,
    },
    Reply {
        id: u64,
        ok: bool,
        message: String,
    },
    Hosts {
        hosts: Vec<Host>,
    },
    PairStart {
        epoch: u64,
        code: String,
        endpoints: Vec<Endpoint>,
    },
    PairStop,
    PairCommitted,
    Shutdown,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Authenticated { transport: u64, fingerprint: String },
    Disconnected { transport: u64 },
    Spawn { transport: u64, name: String },
    CloseService { transport: u64, service: u32 },
    Root { transport: u64, enable: bool },
    PairResult { epoch: u64, key: Option<String> },
    BindReverse { transport: u64, spec: String },
    CloseReverse { transport: u64, spec: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStart {
    pub name: String,
    pub root: bool,
    pub test: bool,
    pub bind: bool,
}

pub fn pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    ensure!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        } == 0,
        "IPC socketpair: {}",
        io::Error::last_os_error()
    );
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub fn send<T: Serialize>(socket: RawFd, message: &T, fds: &[RawFd]) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    ensure!(
        body.len() <= MAX_MESSAGE && fds.len() <= MAX_FDS,
        "IPC message too large"
    );
    let mut iov = libc::iovec {
        iov_base: body.as_ptr().cast_mut().cast(),
        iov_len: body.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    // usize storage provides cmsghdr alignment on every supported target.
    let mut ancillary = [0usize; 128];
    if !fds.is_empty() {
        let len = unsafe { libc::CMSG_SPACE(mem::size_of_val(fds) as u32) } as usize;
        ensure!(
            len <= mem::size_of_val(&ancillary),
            "too many IPC descriptors"
        );
        msg.msg_control = ancillary.as_mut_ptr().cast();
        msg.msg_controllen = len;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(mem::size_of_val(fds) as u32) as usize;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast(), fds.len());
        }
    }
    let n = unsafe { libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(n as usize == body.len(), "short IPC send");
    Ok(())
}

pub fn receive<T: DeserializeOwned>(socket: RawFd) -> Result<(T, Vec<OwnedFd>)> {
    let mut body = vec![0u8; MAX_MESSAGE];
    let mut ancillary = [0usize; 128];
    let mut iov = libc::iovec {
        iov_base: body.as_mut_ptr().cast(),
        iov_len: body.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ancillary.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of_val(&ancillary);
    let n = unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut fds = vec![];
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*c).cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize);
                for i in 0..bytes / mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                        libc::CMSG_DATA(c).cast::<RawFd>().add(i),
                    )));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    // OwnedFd drops all received capabilities on malformed/truncated messages.
    ensure!(
        n > 0 && msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0,
        "closed or truncated IPC message"
    );
    ensure!(fds.len() <= MAX_FDS, "too many IPC descriptors");
    Ok((serde_json::from_slice(&body[..n as usize])?, fds))
}

pub fn peer_uid(fd: RawFd) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut len = mem::size_of_val(&cred) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } == 0,
        "cannot read peer credentials"
    );
    ensure!(
        len as usize == mem::size_of_val(&cred),
        "invalid peer credentials"
    );
    Ok(cred.uid)
}

pub fn spawn(mode: &str, fd: RawFd) -> Result<Child> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg(mode)
        .env_clear()
        .env("PATH", "/system/bin:/system/xbin:/bin:/usr/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if mode == "service" {
        environment::inherit_android_runtime(&mut cmd);
    }
    #[cfg(not(target_os = "android"))]
    cmd.stderr(Stdio::inherit());
    // Only async-signal-safe syscalls in the fork-to-exec boundary.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(fd, 3) < 0
                || libc::fcntl(3, libc::F_SETFD, 0) < 0
                || libc::setpgid(0, 0) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(cmd.spawn()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fd_passing_and_cloexec() {
        let (a, b) = pair().unwrap();
        let (c, _d) = pair().unwrap();
        send(a.as_raw_fd(), &vec![42], &[c.as_raw_fd()]).unwrap();
        let (v, f) = receive::<Vec<u32>>(b.as_raw_fd()).unwrap();
        assert_eq!(v, vec![42]);
        assert_eq!(f.len(), 1);
        assert_ne!(
            unsafe { libc::fcntl(f[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}
