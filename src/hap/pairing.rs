//! Pair-setup (SRP), pair-verify (X25519 + Ed25519) and pairing management.

use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use rand::Rng;
use x25519_dalek::{PublicKey, StaticSecret};

use super::crypto::{self, hkdf_sha512, Session};
use super::srp::SrpServer;
use super::tlv::{err, ty, Map, Writer};
use crate::hub::Hub;
use crate::state::Pairing;

#[derive(Default)]
pub struct ConnState {
    srp: Option<SrpServer>,
    srp_key: Option<Vec<u8>>,
    verify: Option<VerifyCtx>,
    /// Pairing id of the verified controller, once pair-verify completes.
    pub controller: Option<String>,
}

impl ConnState {
    pub fn is_admin(&self, hub: &Hub) -> bool {
        self.controller.as_deref().and_then(|id| hub.pairing(id)).is_some_and(|p| p.admin)
    }
}

struct VerifyCtx {
    shared: [u8; 32],
    acc_pub: [u8; 32],
    ctrl_pub: [u8; 32],
}

fn error(state: u8, code: u8) -> Vec<u8> {
    Writer::new().u8(ty::STATE, state).u8(ty::ERROR, code).build()
}

pub fn pair_setup(hub: &Hub, conn: &mut ConnState, body: &[u8]) -> Vec<u8> {
    let req = Map::parse(body);
    match req.u8(ty::STATE) {
        Some(1) => {
            if hub.is_paired() {
                return error(2, err::UNAVAILABLE);
            }
            let srp = SrpServer::new(b"Pair-Setup", hub.setup_code().as_bytes());
            let resp = Writer::new()
                .u8(ty::STATE, 2)
                .bytes(ty::PUBLIC_KEY, &srp.b_pub)
                .bytes(ty::SALT, &srp.salt)
                .build();
            conn.srp = Some(srp);
            resp
        }
        Some(3) => {
            let (Some(srp), Some(a), Some(m1)) = (conn.srp.as_ref(), req.get(ty::PUBLIC_KEY), req.get(ty::PROOF))
            else {
                return error(4, err::AUTHENTICATION);
            };
            match srp.verify(a, m1) {
                Some((m2, key)) => {
                    conn.srp_key = Some(key);
                    Writer::new().u8(ty::STATE, 4).bytes(ty::PROOF, &m2).build()
                }
                None => {
                    tracing::warn!("pair-setup: incorrect setup code");
                    conn.srp = None;
                    error(4, err::AUTHENTICATION)
                }
            }
        }
        Some(5) => match pair_setup_m5(hub, conn, &req) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("pair-setup M5 failed: {e:#}");
                error(6, err::AUTHENTICATION)
            }
        },
        _ => error(2, err::UNKNOWN),
    }
}

fn pair_setup_m5(hub: &Hub, conn: &mut ConnState, req: &Map) -> anyhow::Result<Vec<u8>> {
    let k = conn.srp_key.take().ok_or_else(|| anyhow::anyhow!("no SRP session"))?;
    conn.srp = None;
    let enc_key = hkdf_sha512(&k, b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info");
    let data = req.get(ty::ENCRYPTED_DATA).ok_or_else(|| anyhow::anyhow!("missing encrypted data"))?;
    let sub = Map::parse(&crypto::open(&enc_key, b"PS-Msg05", &[], data)?);
    let (Some(ctrl_id), Some(ctrl_ltpk), Some(sig)) =
        (sub.get(ty::IDENTIFIER), sub.get(ty::PUBLIC_KEY), sub.get(ty::SIGNATURE))
    else {
        anyhow::bail!("missing controller identity");
    };

    let ctrl_x = hkdf_sha512(&k, b"Pair-Setup-Controller-Sign-Salt", b"Pair-Setup-Controller-Sign-Info");
    let info = [ctrl_x.as_slice(), ctrl_id, ctrl_ltpk].concat();
    let vk = VerifyingKey::from_bytes(ctrl_ltpk.try_into()?)?;
    vk.verify(&info, &Signature::from_slice(sig)?)?;

    let ctrl_id_str = String::from_utf8(ctrl_id.to_vec())?;
    hub.add_pairing(Pairing { id: ctrl_id_str.clone(), ltpk: hex::encode(ctrl_ltpk), admin: true })?;
    tracing::info!(controller = %ctrl_id_str, "paired with controller");

    let acc_x = hkdf_sha512(&k, b"Pair-Setup-Accessory-Sign-Salt", b"Pair-Setup-Accessory-Sign-Info");
    let acc_id = hub.device_id();
    let key = hub.signing_key();
    let acc_ltpk = key.verifying_key().to_bytes();
    let acc_info = [acc_x.as_slice(), acc_id.as_bytes(), &acc_ltpk].concat();
    let acc_sig = key.sign(&acc_info).to_bytes();
    let sub = Writer::new()
        .bytes(ty::IDENTIFIER, acc_id.as_bytes())
        .bytes(ty::PUBLIC_KEY, &acc_ltpk)
        .bytes(ty::SIGNATURE, &acc_sig)
        .build();
    let enc = crypto::seal(&enc_key, b"PS-Msg06", &[], &sub);
    Ok(Writer::new().u8(ty::STATE, 6).bytes(ty::ENCRYPTED_DATA, &enc).build())
}

/// Returns the response, plus an encrypted session to switch to after sending it.
pub fn pair_verify(hub: &Hub, conn: &mut ConnState, body: &[u8]) -> (Vec<u8>, Option<Session>) {
    let req = Map::parse(body);
    match req.u8(ty::STATE) {
        Some(1) => {
            let Some(ctrl_pub) = req.get(ty::PUBLIC_KEY).and_then(|v| <[u8; 32]>::try_from(v).ok()) else {
                return (error(2, err::UNKNOWN), None);
            };
            let mut seed = [0u8; 32];
            rand::rng().fill_bytes(&mut seed);
            let secret = StaticSecret::from(seed);
            let acc_pub = PublicKey::from(&secret).to_bytes();
            let shared = secret.diffie_hellman(&PublicKey::from(ctrl_pub)).to_bytes();

            let acc_id = hub.device_id();
            let info = [acc_pub.as_slice(), acc_id.as_bytes(), &ctrl_pub].concat();
            let sig = hub.signing_key().sign(&info).to_bytes();
            let sub = Writer::new().bytes(ty::IDENTIFIER, acc_id.as_bytes()).bytes(ty::SIGNATURE, &sig).build();
            let key = hkdf_sha512(&shared, b"Pair-Verify-Encrypt-Salt", b"Pair-Verify-Encrypt-Info");
            let enc = crypto::seal(&key, b"PV-Msg02", &[], &sub);
            conn.verify = Some(VerifyCtx { shared, acc_pub, ctrl_pub });
            let resp = Writer::new()
                .u8(ty::STATE, 2)
                .bytes(ty::PUBLIC_KEY, &acc_pub)
                .bytes(ty::ENCRYPTED_DATA, &enc)
                .build();
            (resp, None)
        }
        Some(3) => match pair_verify_m3(hub, conn, &req) {
            Ok(session) => (Writer::new().u8(ty::STATE, 4).build(), Some(session)),
            Err(e) => {
                tracing::warn!("pair-verify failed: {e:#}");
                (error(4, err::AUTHENTICATION), None)
            }
        },
        _ => (error(2, err::UNKNOWN), None),
    }
}

fn pair_verify_m3(hub: &Hub, conn: &mut ConnState, req: &Map) -> anyhow::Result<Session> {
    let ctx = conn.verify.take().ok_or_else(|| anyhow::anyhow!("no verify session"))?;
    let key = hkdf_sha512(&ctx.shared, b"Pair-Verify-Encrypt-Salt", b"Pair-Verify-Encrypt-Info");
    let data = req.get(ty::ENCRYPTED_DATA).ok_or_else(|| anyhow::anyhow!("missing encrypted data"))?;
    let sub = Map::parse(&crypto::open(&key, b"PV-Msg03", &[], data)?);
    let ctrl_id = String::from_utf8(sub.get(ty::IDENTIFIER).unwrap_or_default().to_vec())?;
    let pairing = hub.pairing(&ctrl_id).ok_or_else(|| anyhow::anyhow!("unknown controller {ctrl_id}"))?;
    let ltpk: [u8; 32] = hex::decode(&pairing.ltpk)?.try_into().map_err(|_| anyhow::anyhow!("bad ltpk"))?;
    let info = [ctx.ctrl_pub.as_slice(), ctrl_id.as_bytes(), &ctx.acc_pub].concat();
    let sig = Signature::from_slice(sub.get(ty::SIGNATURE).unwrap_or_default())?;
    VerifyingKey::from_bytes(&ltpk)?.verify(&info, &sig)?;
    tracing::debug!(controller = %ctrl_id, "pair-verify ok");
    conn.controller = Some(ctrl_id);
    Ok(Session::new(&ctx.shared))
}

pub fn pairings(hub: &Hub, conn: &ConnState, body: &[u8]) -> Vec<u8> {
    if !conn.is_admin(hub) {
        return error(2, err::AUTHENTICATION);
    }
    let req = Map::parse(body);
    let ok = Writer::new().u8(ty::STATE, 2).build();
    match req.u8(ty::METHOD) {
        // Add pairing
        Some(3) => {
            let (Some(id), Some(pk)) = (req.get(ty::IDENTIFIER), req.get(ty::PUBLIC_KEY)) else {
                return error(2, err::UNKNOWN);
            };
            let p = Pairing {
                id: String::from_utf8_lossy(id).into_owned(),
                ltpk: hex::encode(pk),
                admin: req.u8(ty::PERMISSIONS) == Some(1),
            };
            match hub.add_pairing(p) {
                Ok(()) => ok,
                Err(_) => error(2, err::MAX_PEERS),
            }
        }
        // Remove pairing
        Some(4) => {
            let id = String::from_utf8_lossy(req.get(ty::IDENTIFIER).unwrap_or_default()).into_owned();
            let _ = hub.remove_pairing(&id);
            ok
        }
        // List pairings
        Some(5) => {
            let mut w = Writer::new().u8(ty::STATE, 2);
            for (i, p) in hub.pairings().iter().enumerate() {
                if i > 0 {
                    w = w.delimiter(ty::SEPARATOR);
                }
                w = w
                    .bytes(ty::IDENTIFIER, p.id.as_bytes())
                    .bytes(ty::PUBLIC_KEY, &hex::decode(&p.ltpk).unwrap_or_default())
                    .u8(ty::PERMISSIONS, p.admin as u8);
            }
            w.build()
        }
        _ => error(2, err::UNKNOWN),
    }
}
