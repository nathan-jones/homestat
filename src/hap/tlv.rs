//! TLV8 encoding as used throughout HAP (pairing + camera configuration).
//!
//! Values longer than 255 bytes are split into consecutive fragments of the
//! same type; the decoder joins them back together.

pub mod ty {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0A;
    pub const PERMISSIONS: u8 = 0x0B;
    pub const SEPARATOR: u8 = 0xFF;
}

pub mod err {
    pub const UNKNOWN: u8 = 0x01;
    pub const AUTHENTICATION: u8 = 0x02;
    pub const MAX_PEERS: u8 = 0x04;
    pub const UNAVAILABLE: u8 = 0x06;
}

#[derive(Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(mut self, t: u8, v: &[u8]) -> Self {
        self.push(t, v);
        self
    }

    pub fn push(&mut self, t: u8, v: &[u8]) {
        if v.is_empty() {
            self.buf.extend_from_slice(&[t, 0]);
            return;
        }
        for chunk in v.chunks(255) {
            self.buf.push(t);
            self.buf.push(chunk.len() as u8);
            self.buf.extend_from_slice(chunk);
        }
    }

    pub fn u8(self, t: u8, v: u8) -> Self {
        self.bytes(t, &[v])
    }

    pub fn u16(self, t: u8, v: u16) -> Self {
        self.bytes(t, &v.to_le_bytes())
    }

    pub fn u32(self, t: u8, v: u32) -> Self {
        self.bytes(t, &v.to_le_bytes())
    }

    /// Zero-length item used to delimit repeated items of the same type
    /// (camera TLVs use type 0x00, pairing lists use 0xFF).
    pub fn delimiter(self, t: u8) -> Self {
        self.bytes(t, &[])
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Decodes into an ordered list of (type, value), joining fragments.
pub fn decode(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut i = 0;
    let mut last_len = 0usize;
    while i + 2 <= data.len() {
        let t = data[i];
        let len = data[i + 1] as usize;
        let end = (i + 2 + len).min(data.len());
        let v = &data[i + 2..end];
        match out.last_mut() {
            Some((lt, lv)) if *lt == t && last_len == 255 => lv.extend_from_slice(v),
            _ => out.push((t, v.to_vec())),
        }
        last_len = len;
        i = end;
    }
    out
}

/// Convenience lookup over a decoded TLV list.
pub struct Map(pub Vec<(u8, Vec<u8>)>);

impl Map {
    pub fn parse(data: &[u8]) -> Self {
        Map(decode(data))
    }

    pub fn get(&self, t: u8) -> Option<&[u8]> {
        self.0.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice())
    }

    pub fn u8(&self, t: u8) -> Option<u8> {
        self.get(t).and_then(|v| v.first().copied())
    }

    pub fn u16(&self, t: u8) -> Option<u16> {
        self.get(t).map(le_uint).map(|v| v as u16)
    }

    #[allow(dead_code)]
    pub fn u32(&self, t: u8) -> Option<u32> {
        self.get(t).map(le_uint).map(|v| v as u32)
    }

    pub fn sub(&self, t: u8) -> Option<Map> {
        self.get(t).map(Map::parse)
    }
}

fn le_uint(v: &[u8]) -> u64 {
    v.iter().take(8).rev().fold(0u64, |acc, b| (acc << 8) | *b as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fragmented() {
        let big = vec![7u8; 600];
        let enc = Writer::new().u8(ty::STATE, 2).bytes(ty::PUBLIC_KEY, &big).build();
        let m = Map::parse(&enc);
        assert_eq!(m.u8(ty::STATE), Some(2));
        assert_eq!(m.get(ty::PUBLIC_KEY).unwrap().len(), 600);
    }

    #[test]
    fn little_endian_ints() {
        let enc = Writer::new().u16(1, 1920).u32(2, 0xdeadbeef).build();
        let m = Map::parse(&enc);
        assert_eq!(m.u16(1), Some(1920));
        assert_eq!(m.u32(2), Some(0xdeadbeef));
    }
}
