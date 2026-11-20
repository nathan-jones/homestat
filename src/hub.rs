//! The hub: shared state behind the HAP server, admin API, samplers and streams.
//!
//! HomeKit sees a single bridge accessory (aid 1). Each widget is a bridged
//! camera accessory with a stable aid. Adding/removing widgets bumps the HAP
//! configuration number and re-announces over mDNS, which makes controllers
//! re-fetch `/accessories` without re-pairing.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Result};
use base64::Engine;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha512};

use crate::camera::CameraState;
use crate::hap::mdns::Advertiser;
use crate::state::{Pairing, Store};
use crate::widgets::{NewWidget, WidgetDef, WidgetSnapshot};

const HISTORY: usize = 120;
const MAX_BRIDGED: usize = 149;

#[derive(Default)]
struct Series {
    samples: VecDeque<f64>,
    version: u64,
}

pub struct Hub {
    pub name: String,
    pub signing_key: SigningKey,
    store: Mutex<Store>,
    series: RwLock<HashMap<String, Series>>,
    pub cameras: Mutex<HashMap<u64, CameraState>>,
    mdns: Advertiser,
}

impl Hub {
    pub fn new(name: String, store: Store, mdns: Advertiser) -> Arc<Hub> {
        let ltsk: [u8; 32] = hex::decode(&store.data.ltsk)
            .ok()
            .and_then(|v| v.try_into().ok())
            .expect("valid ltsk in state.json");
        Arc::new(Hub {
            name,
            signing_key: SigningKey::from_bytes(&ltsk),
            store: Mutex::new(store),
            series: RwLock::new(HashMap::new()),
            cameras: Mutex::new(HashMap::new()),
            mdns,
        })
    }

    fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> T) -> T {
        f(&mut self.store.lock().unwrap())
    }

    // ---- identity / pairing ------------------------------------------------

    pub fn device_id(&self) -> String {
        self.with_store(|s| s.data.device_id.clone())
    }

    pub fn setup_code(&self) -> String {
        self.with_store(|s| s.data.setup_code.clone())
    }

    pub fn is_paired(&self) -> bool {
        self.with_store(|s| !s.data.pairings.is_empty())
    }

    pub fn pairing(&self, id: &str) -> Option<Pairing> {
        self.with_store(|s| s.data.pairings.iter().find(|p| p.id == id).cloned())
    }

    pub fn pairings(&self) -> Vec<Pairing> {
        self.with_store(|s| s.data.pairings.clone())
    }

    pub fn add_pairing(&self, p: Pairing) -> Result<()> {
        let was_paired = self.is_paired();
        self.with_store(|s| {
            if let Some(existing) = s.data.pairings.iter_mut().find(|e| e.id == p.id) {
                existing.admin = p.admin;
            } else {
                if s.data.pairings.len() >= 16 {
                    bail!("max peers");
                }
                s.data.pairings.push(p);
            }
            s.save()
        })?;
        if !was_paired {
            self.advertise();
        }
        Ok(())
    }

    pub fn remove_pairing(&self, id: &str) -> Result<()> {
        self.with_store(|s| {
            s.data.pairings.retain(|p| p.id != id);
            s.save()
        })?;
        if !self.is_paired() {
            tracing::info!("last pairing removed; accessory is pairable again");
            self.advertise();
        }
        Ok(())
    }

    /// `X-HM://` setup URI (encodes the setup code for QR pairing).
    pub fn setup_uri(&self) -> String {
        let (code, setup_id) = self.with_store(|s| (s.data.setup_code.clone(), s.data.setup_id.clone()));
        let digits: u64 = code.replace('-', "").parse().unwrap_or(0);
        const CATEGORY_BRIDGE: u64 = 2;
        const FLAG_IP: u64 = 1 << 28;
        let mut v = (CATEGORY_BRIDGE << 31) | FLAG_IP | digits;
        let mut s = Vec::new();
        while v > 0 {
            s.push(char::from_digit((v % 36) as u32, 36).unwrap().to_ascii_uppercase());
            v /= 36;
        }
        let payload: String = s.into_iter().rev().collect();
        format!("X-HM://{payload:0>9}{setup_id}")
    }

    pub fn advertise(&self) {
        let (cn, id, setup_id) = self.with_store(|s| {
            (s.data.config_number, s.data.device_id.clone(), s.data.setup_id.clone())
        });
        let sh = base64::engine::general_purpose::STANDARD
            .encode(&Sha512::digest(format!("{setup_id}{id}").as_bytes())[..4]);
        let txt: HashMap<String, String> = [
            ("c#", cn.to_string()),
            ("ff", "0".into()),
            ("id", id),
            ("md", self.name.clone()),
            ("pv", "1.1".into()),
            ("s#", "1".into()),
            ("sf", if self.is_paired() { "0" } else { "1" }.into()),
            ("ci", "2".into()),
            ("sh", sh),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        if let Err(e) = self.mdns.publish(txt) {
            tracing::error!("mDNS publish failed: {e:#}");
        }
    }

    pub fn shutdown(&self) {
        self.mdns.shutdown();
    }

    /// If this build changed the accessory database layout, bump the config
    /// number so paired controllers re-read `/accessories`.
    pub fn ensure_db_schema(&self) -> Result<()> {
        let schema = crate::hap::db::DB_SCHEMA;
        let changed = self.with_store(|s| {
            if s.data.db_schema == schema {
                return Ok(false);
            }
            s.data.db_schema = schema;
            s.data.config_number = if s.data.config_number >= 65535 { 1 } else { s.data.config_number + 1 };
            s.save().map(|_| true)
        })?;
        if changed {
            tracing::info!("accessory database layout changed; bumped config number");
        }
        Ok(())
    }

    fn bump_config(&self) {
        self.with_store(|s| {
            s.data.config_number = if s.data.config_number >= 65535 { 1 } else { s.data.config_number + 1 };
            if let Err(e) = s.save() {
                tracing::error!("saving state: {e:#}");
            }
        });
        self.advertise();
    }

    // ---- widgets -------------------------------------------------------------

    pub fn widgets(&self) -> Vec<WidgetDef> {
        self.with_store(|s| s.data.widgets.clone())
    }

    pub fn widget_by_aid(&self, aid: u64) -> Option<WidgetDef> {
        self.with_store(|s| s.data.widgets.iter().find(|w| w.aid == aid).cloned())
    }

    pub fn add_widget(&self, req: NewWidget) -> Result<WidgetDef> {
        let def = self.with_store(|s| {
            if s.data.widgets.len() >= MAX_BRIDGED {
                bail!("a HomeKit bridge supports at most {MAX_BRIDGED} accessories");
            }
            let id = req.id.clone().unwrap_or_else(|| slug(&req.name));
            if id.is_empty() || s.data.widgets.iter().any(|w| w.id == id) {
                bail!("widget id '{id}' is empty or already exists");
            }
            let def = req.into_def(id, s.data.next_aid);
            s.data.next_aid += 1;
            s.data.widgets.push(def.clone());
            s.save()?;
            Ok(def)
        })?;
        tracing::info!(id = %def.id, aid = def.aid, "widget added");
        self.bump_config();
        Ok(def)
    }

    pub fn remove_widget(&self, id: &str) -> Result<bool> {
        let removed = self.with_store(|s| {
            let before = s.data.widgets.len();
            let aid = s.data.widgets.iter().find(|w| w.id == id).map(|w| w.aid);
            s.data.widgets.retain(|w| w.id != id);
            s.save().map(|_| (s.data.widgets.len() != before).then_some(aid).flatten())
        })?;
        let Some(aid) = removed else { return Ok(false) };
        if let Some(cam) = self.cameras.lock().unwrap().remove(&aid) {
            cam.stop_all();
        }
        self.series.write().unwrap().remove(id);
        tracing::info!(id, aid, "widget removed");
        self.bump_config();
        Ok(true)
    }

    pub fn push_value(&self, id: &str, value: f64) -> bool {
        if !value.is_finite() || !self.with_store(|s| s.data.widgets.iter().any(|w| w.id == id)) {
            return false;
        }
        let mut series = self.series.write().unwrap();
        let s = series.entry(id.to_string()).or_default();
        s.samples.push_back(value);
        while s.samples.len() > HISTORY {
            s.samples.pop_front();
        }
        s.version += 1;
        true
    }

    pub fn snapshot(&self, id: &str) -> Option<WidgetSnapshot> {
        let def = self.with_store(|s| s.data.widgets.iter().find(|w| w.id == id).cloned())?;
        let series = self.series.read().unwrap();
        let (samples, version) = series
            .get(id)
            .map(|s| (s.samples.iter().copied().collect(), s.version))
            .unwrap_or_default();
        Some(WidgetSnapshot { def, samples, version })
    }

    pub fn latest_values(&self) -> BTreeMap<String, f64> {
        self.series
            .read()
            .unwrap()
            .iter()
            .filter_map(|(k, s)| s.samples.back().map(|v| (k.clone(), *v)))
            .collect()
    }
}

fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}
