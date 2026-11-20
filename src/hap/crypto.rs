//! HKDF / ChaCha20-Poly1305 helpers and the encrypted session framing used
//! after pair-verify.

use anyhow::{anyhow, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha512;

pub fn hkdf_sha512(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt), ikm);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out).expect("32 bytes is a valid HKDF length");
    out
}

fn nonce(n: &[u8]) -> Nonce {
    let mut b = [0u8; 12];
    b[12 - n.len()..].copy_from_slice(n);
    *Nonce::from_slice(&b)
}

pub fn seal(key: &[u8; 32], nonce_bytes: &[u8], aad: &[u8], msg: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(&nonce(nonce_bytes), Payload { msg, aad })
        .expect("chacha20poly1305 encryption cannot fail")
}

pub fn open(key: &[u8; 32], nonce_bytes: &[u8], aad: &[u8], msg: &[u8]) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(&nonce(nonce_bytes), Payload { msg, aad })
        .map_err(|_| anyhow!("decryption failed"))
}

/// Frame-level encryption for an established HAP session.
pub struct Session {
    /// Accessory -> controller key ("Control-Read-Encryption-Key").
    out_key: [u8; 32],
    /// Controller -> accessory key ("Control-Write-Encryption-Key").
    in_key: [u8; 32],
    out_count: u64,
    in_count: u64,
}

const MAX_FRAME: usize = 1024;

impl Session {
    pub fn new(shared_secret: &[u8]) -> Self {
        Session {
            out_key: hkdf_sha512(shared_secret, b"Control-Salt", b"Control-Read-Encryption-Key"),
            in_key: hkdf_sha512(shared_secret, b"Control-Salt", b"Control-Write-Encryption-Key"),
            out_count: 0,
            in_count: 0,
        }
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + 64);
        for chunk in plain.chunks(MAX_FRAME) {
            let aad = (chunk.len() as u16).to_le_bytes();
            let sealed = seal(&self.out_key, &self.out_count.to_le_bytes(), &aad, chunk);
            self.out_count += 1;
            out.extend_from_slice(&aad);
            out.extend_from_slice(&sealed);
        }
        out
    }

    /// Decrypts as many complete frames as `buf` holds, draining them.
    pub fn decrypt(&mut self, buf: &mut Vec<u8>) -> Result<Vec<u8>> {
        let mut plain = Vec::new();
        loop {
            if buf.len() < 2 {
                break;
            }
            let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
            if buf.len() < 2 + len + 16 {
                break;
            }
            let frame: Vec<u8> = buf.drain(..2 + len + 16).collect();
            let p = open(&self.in_key, &self.in_count.to_le_bytes(), &frame[..2], &frame[2..])?;
            self.in_count += 1;
            plain.extend_from_slice(&p);
        }
        Ok(plain)
    }
}
