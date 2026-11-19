use super::{ipc, ksu};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
    // Keep the private mount tree alive without entering its namespace.
    pub namespace: Option<OwnedFd>,
}

#[derive(Deserialize, Serialize)]
struct Request {
    root: bool,
    test: bool,
}

pub fn open() -> Result<Pty> {
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    ensure!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        } == 0,
        "PTY unavailable: {}",
        io::Error::last_os_error()
    );
    let terminal = Pty {
        master: unsafe { OwnedFd::from_raw_fd(master) },
        slave: unsafe { OwnedFd::from_raw_fd(slave) },
        namespace: None,
    };
    for fd in [&terminal.master, &terminal.slave] {
        ensure!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "PTY descriptor protection failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(terminal)
}

pub fn private(root: bool, test: bool) -> Result<Pty> {
    let (socket, bootstrap) = ipc::pair()?;
    let timeout = libc::timeval {
        tv_sec: 5,
        tv_usec: 0,
    };
    ensure!(
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&timeout as *const libc::timeval).cast(),
                std::mem::size_of_val(&timeout) as libc::socklen_t,
            )
        } == 0,
        "PTY helper timeout setup failed"
    );
    let mut child = ipc::spawn("pty", bootstrap.as_raw_fd())?;
    drop(bootstrap);
    let result = (|| {
        ipc::send(socket.as_raw_fd(), &Request { root, test }, &[])?;
        let (reply, mut fds) = ipc::receive::<Result<(), String>>(socket.as_raw_fd())?;
        reply.map_err(anyhow::Error::msg)?;
        ensure!(
            fds.len() == 3,
            "PTY helper requires master, slave and namespace descriptors"
        );
        ensure!(child.wait()?.success(), "PTY helper failed");
        let namespace = fds.pop();
        let slave = fds.pop().unwrap();
        let master = fds.pop().unwrap();
        Ok(Pty {
            master,
            slave,
            namespace,
        })
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result.context("Private PTY allocation failed")
}

fn allocate(request: Request) -> Result<Pty> {
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWNS) } == 0,
        "mount namespace: {}",
        io::Error::last_os_error()
    );
    // Prevent outgoing mount propagation while retaining incoming system mounts.
    ensure!(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_SLAVE | libc::MS_REC,
                std::ptr::null(),
            )
        } == 0,
        "mount propagation: {}",
        io::Error::last_os_error()
    );
    ensure!(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/dev".as_ptr(),
                std::ptr::null(),
                libc::MS_PRIVATE | libc::MS_REC,
                std::ptr::null(),
            )
        } == 0,
        "private /dev: {}",
        io::Error::last_os_error()
    );
    ensure!(
        unsafe {
            libc::mount(
                c"devpts".as_ptr(),
                c"/dev/pts".as_ptr(),
                c"devpts".as_ptr(),
                libc::MS_NOSUID | libc::MS_NOEXEC,
                c"newinstance,mode=0600,ptmxmode=000".as_ptr().cast(),
            )
        } == 0,
        "private devpts: {}",
        io::Error::last_os_error()
    );
    // Open before dropping privileges; shell cannot open namespace handles.
    let namespace = File::open("/proc/self/ns/mnt")?.into();
    ksu::restrict_service(request.root, request.test)?;
    // /dev/ptmx resolves pts in this namespace and retains its platform SELinux label.
    let mut terminal = open()?;
    terminal.namespace = Some(namespace);
    Ok(terminal)
}

pub fn run() -> Result<()> {
    ksu::child_setup()?;
    let bootstrap = unsafe { OwnedFd::from_raw_fd(3) };
    let (request, fds) = ipc::receive::<Request>(bootstrap.as_raw_fd())?;
    ensure!(fds.is_empty(), "PTY request must not include descriptors");
    match allocate(request) {
        Ok(terminal) => ipc::send(
            bootstrap.as_raw_fd(),
            &Ok::<(), String>(()),
            &[
                terminal.master.as_raw_fd(),
                terminal.slave.as_raw_fd(),
                terminal.namespace.as_ref().unwrap().as_raw_fd(),
            ],
        ),
        Err(error) => {
            let _ = ipc::send(
                bootstrap.as_raw_fd(),
                &Err::<(), String>(error.to_string()),
                &[],
            );
            Err(error)
        }
    }
}
