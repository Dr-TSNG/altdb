pub mod sync;
use super::{
    ipc::{self, ServiceStart},
    ksu, network,
};
use crate::protocol::{self, CHUNK};
use anyhow::{Result, bail, ensure};
use std::{
    fs::File,
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
    thread,
};

static CHILD: AtomicI32 = AtomicI32::new(0);
extern "C" fn terminate(_: i32) {
    let pid = CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    unsafe {
        libc::_exit(0);
    }
}

pub fn run() -> Result<()> {
    ksu::child_setup()?;
    let bootstrap = unsafe { OwnedFd::from_raw_fd(3) };
    let (start, mut fds) = ipc::receive::<ServiceStart>(bootstrap.as_raw_fd())?;
    ensure!(fds.len() == 1, "service requires one data descriptor");
    let mut stream: UnixStream = fds.remove(0).into();
    drop(bootstrap);
    ksu::restrict_service(start.root, start.allow_su, start.test)?;
    if start.bind {
        let (fd, spec) = network::reverse_listener(&start.name)?;
        ipc::send(stream.as_raw_fd(), &spec, &[fd.as_raw_fd()])?;
        // Keep filesystem socket ownership until the supervisor ends the binder.
        let mut b = [0];
        let _ = stream.read(&mut b);
        if let Some(path) = start.name.strip_prefix("localfilesystem:") {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }
    if start.name == "sync:" {
        return sync::serve(&mut stream);
    }
    if let Some(spec) = start.name.strip_prefix("tcp:") {
        let port = spec.parse::<u16>()?;
        let socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))?;
        return relay(stream, socket);
    }
    if start.name.starts_with("localabstract:") || start.name.starts_with("localfilesystem:") {
        let socket: UnixStream = network::unix_socket(&start.name, false)?.into();
        return relay(stream, socket);
    }
    let args = ShellArgs::parse(&start.name)?;
    shell(stream, args, start.test)
}

trait Duplex: Read + Write + Send + 'static {
    fn duplicate(&self) -> io::Result<Self>
    where
        Self: Sized;
    fn shutdown_write(&self);
}

impl Duplex for TcpStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Duplex for UnixStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

fn relay<S: Duplex>(mut input: UnixStream, mut socket: S) -> Result<()> {
    let mut read = input.try_clone()?;
    let mut write = socket.duplicate()?;
    let thread = thread::spawn(move || {
        let result = io::copy(&mut read, &mut write);
        write.shutdown_write();
        result
    });
    let result = io::copy(&mut socket, &mut input);
    let _ = input.shutdown(Shutdown::Both);
    let _ = thread.join();
    result?;
    Ok(())
}

#[derive(Debug)]
pub struct ShellArgs {
    pub command: String,
    pub pty: bool,
    pub v2: bool,
    pub term: String,
}

impl ShellArgs {
    pub fn parse(name: &str) -> Result<Self> {
        ensure!(
            name.len() <= 4096 && !name.contains('\0'),
            "invalid service request"
        );
        if let Some(command) = name.strip_prefix("exec:") {
            return Ok(Self {
                command: command.into(),
                pty: false,
                v2: false,
                term: "dumb".into(),
            });
        }
        if let Some(target) = name.strip_prefix("reboot:") {
            ensure!(
                [
                    "",
                    "bootloader",
                    "recovery",
                    "sideload",
                    "sideload-auto-reboot",
                    "fastboot"
                ]
                .contains(&target),
                "unsupported reboot target"
            );
            ensure!(!target.starts_with("sideload"), "sideload is not supported");
            return Ok(Self {
                command: format!("/system/bin/reboot {target}"),
                pty: false,
                v2: false,
                term: "dumb".into(),
            });
        }
        let (header, command) = name
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("unsupported service"))?;
        let mut options = header.split(',');
        ensure!(options.next() == Some("shell"), "unsupported service");
        let mut result = Self {
            command: command.into(),
            pty: command.is_empty(),
            v2: false,
            term: "dumb".into(),
        };
        for opt in options {
            match opt {
                "v2" => result.v2 = true,
                "raw" => result.pty = false,
                "pty" => result.pty = true,
                s if s.starts_with("TERM=") => {
                    ensure!(s.len() <= 128, "TERM too long");
                    result.term = s[5..].to_owned();
                }
                _ => {}
            }
        }
        Ok(result)
    }
}

fn output(
    mut read: impl Read,
    writer: Arc<Mutex<UnixStream>>,
    v2: bool,
    channel: u8,
) -> io::Result<()> {
    let mut buf = vec![0; CHUNK];
    loop {
        let n = match read.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => 0,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(());
        }
        let mut w = writer
            .lock()
            .map_err(|_| io::Error::other("output lock poisoned"))?;
        if v2 {
            protocol::shell_frame(&mut *w, channel, &buf[..n])?;
        } else {
            w.write_all(&buf[..n])?;
        }
    }
}

fn resize(fd: i32, data: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(data)?.trim_end_matches('\0');
    let numbers: Vec<u16> = text
        .split(['x', ','])
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(numbers.len() == 4, "invalid terminal size");
    let ws = libc::winsize {
        ws_row: numbers[0],
        ws_col: numbers[1],
        ws_xpixel: numbers[2],
        ws_ypixel: numbers[3],
    };
    ensure!(
        unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) } == 0,
        "terminal resize failed"
    );
    Ok(())
}

fn input(mut socket: UnixStream, mut stdin: File, v2: bool, pty: bool, pid: i32) {
    let result = (|| -> Result<()> {
        // Do not use io::copy here: Linux's socket-to-pipe splice can hold the
        // pipe lock while awaiting network input, blocking a child's close/exit.
        if !v2 {
            let mut buffer = [0; CHUNK];
            loop {
                let n = socket.read(&mut buffer)?;
                if n == 0 {
                    return Ok(());
                }
                stdin.write_all(&buffer[..n])?;
            }
        }
        loop {
            let (channel, data) = protocol::shell_read(&mut socket)?;
            match channel {
                0 => stdin.write_all(&data)?,
                4 => {
                    ensure!(data.is_empty(), "invalid close-stdin frame");
                    if pty {
                        stdin.write_all(&[4])?;
                    }
                    return Ok(());
                }
                5 => {
                    if pty {
                        resize(stdin.as_raw_fd(), &data)?;
                    }
                }
                _ => bail!("invalid shell input channel"),
            }
        }
    })();
    if result.is_err() {
        unsafe {
            libc::kill(-pid, libc::SIGHUP);
        }
    }
}

fn shell(socket: UnixStream, args: ShellArgs, test: bool) -> Result<()> {
    let shell = if test { "/bin/sh" } else { "/system/bin/sh" };
    let mut command = Command::new(shell);
    if !args.command.is_empty() {
        command.args(["-c", &args.command]);
    }
    command
        .env_clear()
        .env(
            "PATH",
            if test {
                "/usr/bin:/bin"
            } else {
                "/system/bin:/system/xbin"
            },
        )
        .env("HOME", if test { "/tmp" } else { "/data/local/tmp" })
        .env("SHELL", shell)
        .env("TERM", &args.term)
        .env(
            "USER",
            if unsafe { libc::getuid() } == 0 {
                "root"
            } else {
                "shell"
            },
        )
        .current_dir("/");
    let mut master = None;
    let mut slave = None;
    if args.pty {
        let (mut m, mut s) = (-1, -1);
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        ensure!(
            unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), &ws) }
                == 0,
            "PTY unavailable"
        );
        let m = unsafe { OwnedFd::from_raw_fd(m) };
        let s = unsafe { OwnedFd::from_raw_fd(s) };
        for fd in [&m, &s] {
            ensure!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "PTY descriptor protection failed"
            );
        }
        command
            .stdin(Stdio::from(super::duplicate(s.as_raw_fd())?))
            .stdout(Stdio::from(super::duplicate(s.as_raw_fd())?))
            .stderr(Stdio::from(super::duplicate(s.as_raw_fd())?));
        master = Some(m);
        slave = Some(s);
    } else {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    let pty = args.pty;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if pty {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::setpgid(0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    unsafe {
        libc::signal(libc::SIGTERM, terminate as *const () as usize);
        libc::signal(libc::SIGHUP, terminate as *const () as usize);
    }
    let mut child = command.spawn()?;
    drop(command);
    drop(slave);
    let pid = child.id() as i32;
    CHILD.store(pid, Ordering::Relaxed);
    let writer = Arc::new(Mutex::new(socket.try_clone()?));
    let mut readers = vec![];
    let stdin: File;
    if let Some(master) = master {
        stdin = File::from(super::duplicate(master.as_raw_fd())?);
        let w = writer.clone();
        let v2 = args.v2;
        readers.push(thread::spawn(move || output(File::from(master), w, v2, 1)));
    } else {
        stdin = File::from(OwnedFd::from(child.stdin.take().unwrap()));
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        for (reader, channel) in [
            (File::from(OwnedFd::from(out)), 1),
            (File::from(OwnedFd::from(err)), 2),
        ] {
            let w = writer.clone();
            let v2 = args.v2;
            readers.push(thread::spawn(move || output(reader, w, v2, channel)));
        }
    }
    let incoming = socket.try_clone()?;
    let v2 = args.v2;
    let input_thread = thread::spawn(move || input(incoming, stdin, v2, pty, pid));
    let status = child.wait()?;
    for reader in readers {
        let _ = reader.join();
    }
    if args.v2 {
        use std::os::unix::process::ExitStatusExt;
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)) as u8;
        let mut w = writer.lock().unwrap();
        protocol::shell_frame(&mut *w, 3, &[code])?;
    }
    let _ = socket.shutdown(Shutdown::Both);
    let _ = input_thread.join();
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    CHILD.store(0, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_options() {
        let a = ShellArgs::parse("shell,v2,pty,TERM=xterm:echo yes").unwrap();
        assert!(a.pty && a.v2);
        assert_eq!(a.term, "xterm");
        assert!(!ShellArgs::parse("exec:id").unwrap().pty);
        assert!(ShellArgs::parse("tcpip:5555").is_err());
        assert!(ShellArgs::parse("reboot:recovery;id").is_err());
    }
}
