//! On-disk state: accessory identity, controller pairings, and widget definitions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::widgets::WidgetDef;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pairing {
    pub id: String,
    /// Controller Ed25519 long-term public key, hex.
    pub ltpk: String,
    pub admin: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistedState {
    /// HAP device id, formatted like a MAC address.
    pub device_id: String,
    pub setup_code: String,
    pub setup_id: String,
    /// Accessory Ed25519 long-term secret key, hex.
    pub ltsk: String,
    pub pairings: Vec<Pairing>,
    pub config_number: u32,
    pub next_aid: u64,
    pub widgets: Vec<WidgetDef>,
    /// `hap::db::DB_SCHEMA` the config number was last bumped for.
    #[serde(default)]
    pub db_schema: u32,
}

pub struct Store {
    path: PathBuf,
    pub data: PersistedState,
}

impl Store {
    pub fn load_or_create(dir: &Path, setup_code: Option<String>) -> Result<(Store, bool)> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("state.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let data: PersistedState = serde_json::from_str(&raw).context("parsing state.json")?;
            return Ok((Store { path, data }, false));
        }
        let identity = Identity::generate();
        let data = PersistedState {
            device_id: identity.device_id,
            setup_code: setup_code.unwrap_or_else(random_setup_code),
            setup_id: identity.setup_id,
            ltsk: identity.ltsk,
            pairings: vec![],
            config_number: 1,
            next_aid: 2,
            widgets: vec![],
            db_schema: crate::hap::db::DB_SCHEMA,
        };
        let store = Store { path, data };
        store.save()?;
        Ok((store, true))
    }

    /// Factory reset of the HomeKit side: a brand-new accessory identity and
    /// no pairings. Widgets (and the setup code) are kept.
    pub fn reset_identity(&mut self) -> Result<()> {
        let identity = Identity::generate();
        self.data.device_id = identity.device_id;
        self.data.setup_id = identity.setup_id;
        self.data.ltsk = identity.ltsk;
        self.data.pairings.clear();
        self.data.config_number = 1;
        self.save()
    }

    pub fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.data)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

struct Identity {
    device_id: String,
    setup_id: String,
    ltsk: String,
}

impl Identity {
    fn generate() -> Identity {
        let mut rng = rand::rng();
        let mac: Vec<String> = (0..6).map(|_| format!("{:02X}", rng.random::<u8>())).collect();
        Identity {
            device_id: mac.join(":"),
            setup_id: (0..4)
                .map(|_| {
                    let c = rng.random_range(0..36u8);
                    (if c < 10 { b'0' + c } else { b'A' + c - 10 }) as char
                })
                .collect(),
            ltsk: hex::encode(rng.random::<[u8; 32]>()),
        }
    }
}

fn random_setup_code() -> String {
    const INVALID: [&str; 2] = ["12345678", "87654321"];
    let mut rng = rand::rng();
    loop {
        let digits: String = (0..8).map(|_| char::from(b'0' + rng.random_range(0..10u8))).collect();
        let all_same = digits.chars().all(|c| Some(c) == digits.chars().next());
        if !all_same && !INVALID.contains(&digits.as_str()) {
            return format!("{}-{}-{}", &digits[0..3], &digits[3..5], &digits[5..8]);
        }
    }
}
