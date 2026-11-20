//! SRTP / SRTCP sender side, AES_CM_128_HMAC_SHA1_80 (RFC 3711), KDR = 0.

use aes::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
type HmacSha1 = Hmac<Sha1>;

const TAG_LEN: usize = 10;

fn keystream(key: &[u8; 16], iv: &[u8; 16], buf: &mut [u8]) {
    Aes128Ctr::new(key.into(), iv.into()).apply_keystream(buf);
}

fn derive<const N: usize>(master_key: &[u8; 16], master_salt: &[u8; 14], label: u8) -> [u8; N] {
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(master_salt);
    iv[7] ^= label;
    let mut out = [0u8; N];
    keystream(master_key, &iv, &mut out);
    out
}

pub struct Srtp {
    rtp_key: [u8; 16],
    rtp_salt: [u8; 14],
    rtp_auth: [u8; 20],
    rtcp_key: [u8; 16],
    rtcp_salt: [u8; 14],
    rtcp_auth: [u8; 20],
    srtcp_index: u32,
}

impl Srtp {
    pub fn new(master_key: &[u8], master_salt: &[u8]) -> anyhow::Result<Self> {
        let mk: [u8; 16] = master_key.try_into().map_err(|_| anyhow::anyhow!("SRTP key must be 16 bytes"))?;
        let ms: [u8; 14] = master_salt.try_into().map_err(|_| anyhow::anyhow!("SRTP salt must be 14 bytes"))?;
        Ok(Srtp {
            rtp_key: derive(&mk, &ms, 0),
            rtp_auth: derive(&mk, &ms, 1),
            rtp_salt: derive(&mk, &ms, 2),
            rtcp_key: derive(&mk, &ms, 3),
            rtcp_auth: derive(&mk, &ms, 4),
            rtcp_salt: derive(&mk, &ms, 5),
            srtcp_index: 0,
        })
    }

    fn iv(salt: &[u8; 14], ssrc: u32, index: u64, index_offset: usize) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..14].copy_from_slice(salt);
        for (i, b) in ssrc.to_be_bytes().iter().enumerate() {
            iv[4 + i] ^= b;
        }
        // index * 2^16: the low bytes of the big-endian index land in iv[..14].
        // `index_offset` is 8 for SRTP (48-bit index) and 10 for SRTCP (31-bit).
        let idx = index.to_be_bytes();
        for i in index_offset..14 {
            iv[i] ^= idx[i - 6];
        }
        iv
    }

    /// Encrypts an RTP packet in place and appends the auth tag.
    /// `index` is the 48-bit packet index: (ROC << 16) | seq.
    pub fn protect_rtp(&self, pkt: &mut Vec<u8>, index: u64) {
        let ssrc = u32::from_be_bytes(pkt[8..12].try_into().unwrap());
        let header_len = 12 + 4 * (pkt[0] & 0x0f) as usize;
        let iv = Self::iv(&self.rtp_salt, ssrc, index, 8);
        keystream(&self.rtp_key, &iv, &mut pkt[header_len..]);
        let mut mac = HmacSha1::new_from_slice(&self.rtp_auth).unwrap();
        mac.update(pkt);
        mac.update(&((index >> 16) as u32).to_be_bytes());
        let tag = mac.finalize().into_bytes();
        pkt.extend_from_slice(&tag[..TAG_LEN]);
    }

    /// Encrypts an RTCP compound packet in place, appending E|index and auth tag.
    pub fn protect_rtcp(&mut self, pkt: &mut Vec<u8>) {
        let ssrc = u32::from_be_bytes(pkt[4..8].try_into().unwrap());
        let index = self.srtcp_index;
        self.srtcp_index = (self.srtcp_index + 1) & 0x7fff_ffff;
        let iv = Self::iv(&self.rtcp_salt, ssrc, index as u64, 10);
        keystream(&self.rtcp_key, &iv, &mut pkt[8..]);
        pkt.extend_from_slice(&(0x8000_0000 | index).to_be_bytes());
        let mut mac = HmacSha1::new_from_slice(&self.rtcp_auth).unwrap();
        mac.update(pkt);
        let tag = mac.finalize().into_bytes();
        pkt.extend_from_slice(&tag[..TAG_LEN]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    /// RFC 3711 Appendix B.3 key derivation test vectors.
    #[test]
    fn key_derivation_vectors() {
        let mk: [u8; 16] = h("E1F97A0D3E018BE0D64FA32C06DE4139").try_into().unwrap();
        let ms: [u8; 14] = h("0EC675AD498AFEEBB6960B3AABE6").try_into().unwrap();
        assert_eq!(derive::<16>(&mk, &ms, 0).to_vec(), h("C61E7A93744F39EE10734AFE3FF7A087"));
        assert_eq!(derive::<14>(&mk, &ms, 2).to_vec(), h("30CBBC08863D8C85D49DB34A9AE1"));
        assert_eq!(derive::<20>(&mk, &ms, 1).to_vec(), h("CEBE321F6FF7716B6FD4AB49AF256A156D38BAA4"));
    }

    /// RFC 3711 Appendix B.2 AES-CM keystream test vector.
    #[test]
    fn aes_cm_keystream() {
        let key: [u8; 16] = h("2B7E151628AED2A6ABF7158809CF4F3C").try_into().unwrap();
        let salt: [u8; 14] = h("F0F1F2F3F4F5F6F7F8F9FAFBFCFD").try_into().unwrap();
        let iv = Srtp::iv(&salt, 0, 0, 8);
        let mut ks = [0u8; 32];
        keystream(&key, &iv, &mut ks);
        assert_eq!(ks.to_vec(), h("E03EAD0935C95E80E166B16DD92B4EB4D23513162B02D0F72A43A2FE4A5F97AB"));
    }
}
