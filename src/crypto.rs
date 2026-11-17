use crate::unix::socket::Stream;
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CString, c_int, c_void},
    io::{self, Read, Write},
    ptr::NonNull,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

unsafe extern "C" {
    fn altdb_random(out: *mut u8, len: usize) -> c_int;
    fn altdb_free(p: *mut c_void);
    fn altdb_identity(k: *mut *mut u8, kl: *mut usize, c: *mut *mut u8, cl: *mut usize) -> c_int;
    fn altdb_android_key_fingerprint(raw: *const u8, len: usize, out: *mut u8) -> c_int;
    fn altdb_tls_new(
        k: *const u8,
        kl: usize,
        c: *const u8,
        cl: usize,
        pairing: c_int,
        allowed: *const u8,
        count: usize,
    ) -> *mut c_void;
    fn altdb_tls_free(p: *mut c_void);
    fn altdb_tls_add_ca(p: *mut c_void, hex: *const std::ffi::c_char) -> c_int;
    fn altdb_tls_handshake(p: *mut c_void) -> c_int;
    fn altdb_tls_error(p: *mut c_void, n: c_int) -> c_int;
    fn altdb_tls_read(p: *mut c_void, b: *mut u8, n: c_int) -> c_int;
    fn altdb_tls_write(p: *mut c_void, b: *const u8, n: c_int) -> c_int;
    fn altdb_tls_feed(p: *mut c_void, b: *const u8, n: c_int) -> c_int;
    fn altdb_tls_drain(p: *mut c_void, b: *mut u8, n: c_int) -> c_int;
    fn altdb_tls_export(p: *mut c_void, out: *mut u8) -> c_int;
    fn altdb_tls_peer(p: *mut c_void, out: *mut u8);
    fn altdb_pake_new(
        server: c_int,
        s: *const u8,
        len: usize,
        msg: *mut u8,
        ml: *mut usize,
    ) -> *mut c_void;
    fn altdb_pake_finish(p: *mut c_void, msg: *const u8, len: usize) -> c_int;
    fn altdb_pake_crypt(
        p: *mut c_void,
        enc: c_int,
        b: *const u8,
        len: usize,
        out: *mut u8,
        ol: *mut usize,
        cap: usize,
    ) -> c_int;
    fn altdb_pake_free(p: *mut c_void);
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0; N];
    ensure!(
        unsafe { altdb_random(out.as_mut_ptr(), N) } == 1,
        "secure random unavailable"
    );
    Ok(out)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    ensure!(s.len().is_multiple_of(2) && s.is_ascii(), "invalid hex");
    s.as_bytes()
        .chunks_exact(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c)?, 16).map_err(Into::into))
        .collect()
}

pub fn pairing_code() -> Result<String> {
    loop {
        let n = u32::from_le_bytes(random()?);
        if n < 4_294_000_000 {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
}

pub fn host_key(key: &str) -> Result<(String, String)> {
    ensure!(key.len() <= 8190 && !key.contains('\0'), "invalid host key");
    let mut parts = key.splitn(2, char::is_whitespace);
    let encoded = parts.next().unwrap_or_default();
    let raw = STANDARD.decode(encoded)?;
    let mut fp = [0u8; 32];
    ensure!(
        unsafe { altdb_android_key_fingerprint(raw.as_ptr(), raw.len(), fp.as_mut_ptr()) } == 1,
        "invalid Android RSA key"
    );
    let name: String = parts
        .next()
        .unwrap_or("Computer")
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    Ok((hex(&fp), name))
}

#[derive(Clone, Deserialize, Serialize, Zeroize)]
pub struct Identity {
    pub key: String,
    pub certificate: String,
    pub guid: String,
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let (mut key, mut cert) = (std::ptr::null_mut(), std::ptr::null_mut());
        let (mut kl, mut cl) = (0, 0);
        let ok = unsafe { altdb_identity(&mut key, &mut kl, &mut cert, &mut cl) };
        // Both outputs, including partial allocations on failure, belong to us.
        let k = if key.is_null() {
            vec![]
        } else {
            unsafe { std::slice::from_raw_parts(key, kl).to_vec() }
        };
        let c = if cert.is_null() {
            vec![]
        } else {
            unsafe { std::slice::from_raw_parts(cert, cl).to_vec() }
        };
        unsafe {
            altdb_free(key.cast());
            altdb_free(cert.cast());
        }
        ensure!(ok == 1, "identity generation failed");
        Ok(Self {
            key: String::from_utf8(k)?,
            certificate: String::from_utf8(c)?,
            guid: format!("altdb-{}", hex(&random::<16>()?)),
        })
    }
}

pub struct Pake {
    ptr: NonNull<c_void>,
    pub message: Vec<u8>,
}

impl Drop for Pake {
    fn drop(&mut self) {
        unsafe { altdb_pake_free(self.ptr.as_ptr()) }
    }
}

impl Pake {
    pub fn new(server: bool, secret: &[u8]) -> Result<Self> {
        let mut msg = vec![0; 32];
        let mut len = 0;
        let ptr = NonNull::new(unsafe {
            altdb_pake_new(
                server as c_int,
                secret.as_ptr(),
                secret.len(),
                msg.as_mut_ptr(),
                &mut len,
            )
        })
        .ok_or_else(|| anyhow::anyhow!("SPAKE2 initialization failed"))?;
        msg.truncate(len);
        Ok(Self { ptr, message: msg })
    }

    pub fn finish(&mut self, other: &[u8]) -> Result<()> {
        ensure!(other.len() == 32, "invalid SPAKE2 message length");
        ensure!(
            unsafe { altdb_pake_finish(self.ptr.as_ptr(), other.as_ptr(), other.len()) } == 1,
            "SPAKE2 exchange failed"
        );
        Ok(())
    }

    pub fn crypt(&mut self, encrypt: bool, input: &[u8]) -> Result<Vec<u8>> {
        ensure!(input.len() <= 16400, "pairing message too large");
        let mut out = vec![0; input.len() + 16];
        let mut len = 0;
        ensure!(
            unsafe {
                altdb_pake_crypt(
                    self.ptr.as_ptr(),
                    encrypt as c_int,
                    input.as_ptr(),
                    input.len(),
                    out.as_mut_ptr(),
                    &mut len,
                    out.len(),
                )
            } == 1,
            "pairing authentication failed"
        );
        out.truncate(len);
        Ok(out)
    }
}

/// A single owner drives SSL and both BIOs. Never read/write one SSL concurrently.
pub struct Tls {
    ptr: NonNull<c_void>,
    socket: Stream,
    outgoing: Vec<u8>,
    offset: usize,
}
// SAFETY: ownership may move between threads, but no SSL operation is shared.
unsafe impl Send for Tls {}
impl Drop for Tls {
    fn drop(&mut self) {
        unsafe { altdb_tls_free(self.ptr.as_ptr()) }
    }
}

impl Tls {
    pub fn accept(
        socket: impl Into<Stream>,
        identity: &Identity,
        allowed: &[String],
        pairing: bool,
    ) -> Result<Self> {
        let socket = socket.into();
        // Interactive traffic must not wait for a previous small TCP segment
        // to be acknowledged. This also covers the independent pairing socket.
        socket.nodelay()?;
        let mut keys = vec![];
        for fp in allowed {
            let b = unhex(fp)?;
            ensure!(b.len() == 32, "invalid fingerprint");
            keys.extend(b);
        }
        let ptr = NonNull::new(unsafe {
            altdb_tls_new(
                identity.key.as_ptr(),
                identity.key.len(),
                identity.certificate.as_ptr(),
                identity.certificate.len(),
                pairing as c_int,
                keys.as_ptr(),
                allowed.len(),
            )
        })
        .ok_or_else(|| anyhow::anyhow!("TLS context creation failed"))?;
        let mut tls = Self {
            ptr,
            socket,
            outgoing: vec![],
            offset: 0,
        };
        for fp in allowed {
            let name = CString::new(fp.to_uppercase())?;
            ensure!(
                unsafe { altdb_tls_add_ca(tls.ptr.as_ptr(), name.as_ptr()) } == 1,
                "TLS CA list failed"
            );
        }
        tls.socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        tls.socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            ensure!(Instant::now() < deadline, "TLS handshake timeout");
            let n = unsafe { altdb_tls_handshake(tls.ptr.as_ptr()) };
            let error = if n == 1 {
                0
            } else {
                unsafe { altdb_tls_error(tls.ptr.as_ptr(), n) }
            };
            tls.flush()?;
            if n == 1 {
                return Ok(tls);
            }
            match error {
                2 => tls.feed()?,
                3 => {}
                _ => bail!("TLS handshake rejected"),
            }
        }
    }

    fn feed(&mut self) -> io::Result<()> {
        let mut buf = [0; 65536];
        let n = self.socket.read(&mut buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TLS peer disconnected",
            ));
        }
        if unsafe { altdb_tls_feed(self.ptr.as_ptr(), buf.as_ptr(), n as c_int) } != n as c_int {
            return Err(io::Error::other("TLS input failed"));
        }
        Ok(())
    }

    fn drain(&mut self) -> io::Result<()> {
        // Keep allocated storage bounded even when a saturated socket never
        // completely empties between successive SSL_write calls.
        if self.offset > 0 {
            self.outgoing.drain(..self.offset);
            self.offset = 0;
        }
        let mut buf = [0; 65536];
        loop {
            let n =
                unsafe { altdb_tls_drain(self.ptr.as_ptr(), buf.as_mut_ptr(), buf.len() as c_int) };
            if n <= 0 {
                break;
            }
            self.outgoing.extend_from_slice(&buf[..n as usize]);
            if self.queued() > 8 * 1024 * 1024 {
                return Err(io::Error::other("TLS output queue limit"));
            }
        }
        Ok(())
    }

    pub fn queued(&self) -> usize {
        self.outgoing.len() - self.offset
    }

    pub fn socket(&self) -> &Stream {
        &self.socket
    }

    pub fn nonblocking(&self) -> io::Result<()> {
        self.socket.set_nonblocking(true)
    }

    pub fn exporter(&self) -> Result<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(vec![0; 64]);
        ensure!(
            unsafe { altdb_tls_export(self.ptr.as_ptr(), out.as_mut_ptr()) } == 1,
            "TLS exporter failed"
        );
        Ok(out)
    }

    pub fn peer(&self) -> String {
        let mut out = [0; 32];
        unsafe {
            altdb_tls_peer(self.ptr.as_ptr(), out.as_mut_ptr());
        }
        hex(&out)
    }
}

impl Read for Tls {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let n = unsafe {
                altdb_tls_read(
                    self.ptr.as_ptr(),
                    buf.as_mut_ptr(),
                    buf.len().min(i32::MAX as usize) as c_int,
                )
            };
            if n > 0 {
                return Ok(n as usize);
            }
            let error = unsafe { altdb_tls_error(self.ptr.as_ptr(), n) };
            match error {
                2 => {
                    self.feed()?;
                }
                3 => self.flush()?,
                6 => return Ok(0),
                _ => return Err(io::Error::other("TLS read failed")),
            }
        }
    }
}

impl Write for Tls {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe {
            altdb_tls_write(
                self.ptr.as_ptr(),
                buf.as_ptr(),
                buf.len().min(i32::MAX as usize) as c_int,
            )
        };
        if n <= 0 {
            return Err(io::Error::other("TLS write failed"));
        }
        self.drain()?;
        match self.flush() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            result => result?,
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain()?;
        while self.offset < self.outgoing.len() {
            let n = self.socket.write(&self.outgoing[self.offset..])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "TLS socket closed",
                ));
            }
            self.offset += n;
        }
        self.outgoing.clear();
        self.offset = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pake_authenticates_and_rejects_replay() {
        let mut a = Pake::new(false, b"123456channel").unwrap();
        let mut b = Pake::new(true, b"123456channel").unwrap();
        a.finish(&b.message).unwrap();
        b.finish(&a.message).unwrap();
        let c = a.crypt(true, b"authenticated").unwrap();
        assert_eq!(b.crypt(false, &c).unwrap(), b"authenticated");
        assert!(b.crypt(false, &c).is_err());
        assert!(a.finish(&b.message).is_err());
    }
    #[test]
    fn wrong_code_or_channel_fails() {
        for secret in [b"654321channel".as_slice(), b"123456different"] {
            let mut a = Pake::new(false, b"123456channel").unwrap();
            let mut b = Pake::new(true, secret).unwrap();
            a.finish(&b.message).unwrap();
            b.finish(&a.message).unwrap();
            assert!(b.crypt(false, &a.crypt(true, b"data").unwrap()).is_err());
        }
    }
    #[test]
    fn identity_and_codes() {
        let i = Identity::generate().unwrap();
        assert!(i.certificate.contains("CERTIFICATE"));
        assert!(pairing_code().unwrap().bytes().all(|b| b.is_ascii_digit()));
    }
    #[test]
    fn malformed_host_keys() {
        for key in ["", "YWJj", "bad\0host"] {
            assert!(host_key(key).is_err());
        }
    }
}
