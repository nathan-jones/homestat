//! SRP-6a server (3072-bit group, SHA-512) with the conventions HomeKit uses:
//! k = H(N | PAD(g)), u = H(PAD(A) | PAD(B)), x = H(s | H(I ":" P)),
//! K = H(PAD(S)), M1 = H(H(N) xor H(g) | H(I) | s | A | B | K), M2 = H(A | M1 | K).

use num_bigint::BigUint;
use rand::Rng;
use sha2::{Digest, Sha512};

const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 5;
const N_LEN: usize = 384;

fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap()
}

fn pad(v: &BigUint) -> Vec<u8> {
    let b = v.to_bytes_be();
    let mut out = vec![0u8; N_LEN.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

fn h(parts: &[&[u8]]) -> Vec<u8> {
    let mut d = Sha512::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().to_vec()
}

pub struct SrpServer {
    pub salt: [u8; 16],
    pub b_pub: Vec<u8>,
    b: BigUint,
    v: BigUint,
    username: Vec<u8>,
}

impl SrpServer {
    pub fn new(username: &[u8], password: &[u8]) -> Self {
        let mut rng = rand::rng();
        let mut salt = [0u8; 16];
        rng.fill_bytes(&mut salt);
        let mut b_bytes = [0u8; 32];
        rng.fill_bytes(&mut b_bytes);
        Self::with_values(username, password, salt, BigUint::from_bytes_be(&b_bytes))
    }

    fn with_values(username: &[u8], password: &[u8], salt: [u8; 16], b: BigUint) -> Self {
        let n = n();
        let g = BigUint::from(G);
        let x = BigUint::from_bytes_be(&h(&[&salt, &h(&[username, b":", password])]));
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&h(&[&n.to_bytes_be(), &pad(&g)]));
        let b_pub = (k * &v + g.modpow(&b, &n)) % &n;
        SrpServer { salt, b_pub: pad(&b_pub), b, v, username: username.to_vec() }
    }

    /// Verifies the client's proof. Returns (M2, session key K) on success.
    pub fn verify(&self, a_pub: &[u8], m1: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let n = n();
        let a = BigUint::from_bytes_be(a_pub);
        if (&a % &n) == BigUint::ZERO {
            return None;
        }
        let a_padded = pad(&a);
        let u = BigUint::from_bytes_be(&h(&[&a_padded, &self.b_pub]));
        let s = (a * self.v.modpow(&u, &n)).modpow(&self.b, &n);
        let key = h(&[&pad(&s)]);

        let hn = h(&[&n.to_bytes_be()]);
        let hg = h(&[&BigUint::from(G).to_bytes_be()]);
        let hxor: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
        let expected = h(&[&hxor, &h(&[&self.username]), &self.salt, a_pub, &self.b_pub, &key]);
        if expected.as_slice() != m1 {
            return None;
        }
        let m2 = h(&[a_pub, m1, &key]);
        Some((m2, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_transcribed_correctly() {
        let n = n();
        assert_eq!(n.bits(), 3072);
        // Fermat check: any typo in the constant would almost surely fail this.
        let two = BigUint::from(2u32);
        assert_eq!(two.modpow(&(&n - 1u32), &n), BigUint::from(1u32));
    }

    /// Simulates an iOS client against the server using the same formulas.
    #[test]
    fn client_server_agree() {
        let (user, pass) = (b"Pair-Setup".as_slice(), b"111-22-333".as_slice());
        let server = SrpServer::new(user, pass);
        let n = n();
        let g = BigUint::from(G);
        let a = BigUint::from(123456789u64);
        let a_pub = pad(&g.modpow(&a, &n));
        let b_pub = BigUint::from_bytes_be(&server.b_pub);
        let k = BigUint::from_bytes_be(&h(&[&n.to_bytes_be(), &pad(&g)]));
        let u = BigUint::from_bytes_be(&h(&[&a_pub, &server.b_pub]));
        let x = BigUint::from_bytes_be(&h(&[&server.salt, &h(&[user, b":", pass])]));
        let gx = g.modpow(&x, &n);
        let base = (&b_pub + &n * &k - (&k * gx) % &n) % &n;
        let s = base.modpow(&(a + u * x), &n);
        let key = h(&[&pad(&s)]);
        let hn = h(&[&n.to_bytes_be()]);
        let hg = h(&[&g.to_bytes_be()]);
        let hxor: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
        let m1 = h(&[&hxor, &h(&[user]), &server.salt, &a_pub, &server.b_pub, &key]);
        let (m2, server_key) = server.verify(&a_pub, &m1).expect("proof accepted");
        assert_eq!(server_key, key);
        assert_eq!(m2, h(&[&a_pub, &m1, &key]));
    }
}
