use crate::crypto::{self, Identity, Pake, Tls};
use crate::unix::socket::Stream;
use anyhow::{Result, ensure};
use std::io::{Read, Write};
use zeroize::Zeroizing;

pub const PEER_SIZE: usize = 8192;
fn send(tls: &mut Tls, kind: u8, body: &[u8]) -> Result<()> {
    tls.write_all(&[1, kind])?;
    tls.write_all(&(body.len() as u32).to_be_bytes())?;
    tls.write_all(body)?;
    tls.flush()?;
    Ok(())
}

fn receive(tls: &mut Tls, kind: u8) -> Result<Vec<u8>> {
    let mut h = [0; 6];
    tls.read_exact(&mut h)?;
    ensure!(h[0] == 1 && h[1] == kind, "invalid pairing header");
    let len = u32::from_be_bytes(h[2..].try_into().unwrap()) as usize;
    ensure!(len > 0 && len <= PEER_SIZE * 2, "invalid pairing length");
    let mut body = vec![0; len];
    tls.read_exact(&mut body)?;
    Ok(body)
}

pub fn pair(socket: impl Into<Stream>, identity: &Identity, code: &str) -> Result<String> {
    pair_with_commit(socket, identity, code, |_| Ok(()))
}

pub fn pair_with_commit(
    socket: impl Into<Stream>,
    identity: &Identity,
    code: &str,
    commit: impl FnOnce(&str) -> Result<()>,
) -> Result<String> {
    let mut tls = Tls::accept(socket, identity, &[], true)?;
    let mut secret = Zeroizing::new(code.as_bytes().to_vec());
    secret.extend_from_slice(&tls.exporter()?);
    let mut pake = Pake::new(true, &secret)?;
    send(&mut tls, 0, &pake.message)?;
    pake.finish(&receive(&mut tls, 0)?)?;
    let mut peer = Zeroizing::new(vec![0; PEER_SIZE]);
    peer[0] = 1;
    ensure!(identity.guid.len() < PEER_SIZE - 1, "device GUID too long");
    peer[1..1 + identity.guid.len()].copy_from_slice(identity.guid.as_bytes());
    let their = Zeroizing::new(pake.crypt(false, &receive(&mut tls, 1)?)?);
    ensure!(
        their.len() == PEER_SIZE && their[0] == 0,
        "invalid pairing peer information"
    );
    let end = their[1..]
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| anyhow::anyhow!("unterminated host key"))?;
    let key = std::str::from_utf8(&their[1..1 + end])?.to_owned();
    crypto::host_key(&key)?;
    // The stock client sends its PeerInfo before reading ours. Persist trust
    // before acknowledging success, so a disk failure cannot look like pairing.
    commit(&key)?;
    send(&mut tls, 1, &pake.crypt(true, &peer)?)?;
    Ok(key)
}
