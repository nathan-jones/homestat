//! The hub: shared state behind the HAP server, admin API, samplers and streams.
//!
//! HomeKit sees a single bridge accessory (aid 1). Each widget is a bridged
//! camera accessory with a stable aid. Adding/removing widgets bumps the HAP
//! configuration number and re-announces over mDNS, which makes controllers
//! re-fetch `/accessories` without re-pairing.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{bail, Result};
use base64::Engine;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha512};

use crate::camera::CameraState;
use crate::hap::mdns::Advertiser;
use crate::state::{Pairing, Store};
use crate::widgets::{NewWidget, WidgetDef, WidgetSnapshot};

pub const HISTORY: usize = 120;
const MAX_BRIDGED: usize = 149;

#[derive(Default)]
struct Series {
    samples: VecDeque<f64>,
    version: u64,
}

/// Latest value from a remote source (Home Assistant, Prometheus, MQTT),
/// held between samples so sparklines advance at a steady rate.
#[derive(Default, Clone)]
struct Reading {
    value: Option<f64>,
    /// Unit reported by the source (used when the widget has none).
    unit: Option<String>,
    error: Option<String>,
}

/// A HAP connection, for the dashboard.
#[derive(Clone)]
pub struct LiveConn {
    pub peer: IpAddr,
    /// Pairing id once pair-verify succeeded.
    pub controller: Option<String>,
    pub since: Instant,
}

pub struct Hub {
    pub name: String,
    signing_key: RwLock<SigningKey>,
    store: Mutex<Store>,
    /// Bumped whenever pairings change; connections re-check that their
    /// controller is still paired and close otherwise.
    pairing_epoch: tokio::sync::watch::Sender<u64>,
    connections: Mutex<HashMap<u64, LiveConn>>,
    next_conn: AtomicU64,
    series: RwLock<HashMap<String, Series>>,
    readings: RwLock<HashMap<String, Reading>>,
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
            signing_key: RwLock::new(SigningKey::from_bytes(&ltsk)),
            store: Mutex::new(store),
            pairing_epoch: tokio::sync::watch::Sender::new(0),
            connections: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            series: RwLock::new(HashMap::new()),
            readings: RwLock::new(HashMap::new()),
            cameras: Mutex::new(HashMap::new()),
            mdns,
        })
    }

    fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> T) -> T {
        f(&mut self.store.lock().unwrap())
    }

    // ---- identity / pairing ------------------------------------------------

    pub fn signing_key(&self) -> SigningKey {
        self.signing_key.read().unwrap().clone()
    }

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
        self.pairing_epoch.send_modify(|e| *e += 1);
        if !self.is_paired() {
            tracing::info!("last pairing removed; accessory is pairable again");
            self.advertise();
        }
        Ok(())
    }

    /// Forgets every controller and takes on a new accessory identity, so the
    /// bridge can be added to Home again from scratch. The old bridge in the
    /// Home app stops responding and should be removed there.
    pub fn reset_pairing(&self) -> Result<()> {
        let ltsk = self.with_store(|s| {
            s.reset_identity()?;
            Ok::<_, anyhow::Error>(s.data.ltsk.clone())
        })?;
        let ltsk: [u8; 32] = hex::decode(ltsk)?.try_into().map_err(|_| anyhow::anyhow!("bad ltsk"))?;
        *self.signing_key.write().unwrap() = SigningKey::from_bytes(&ltsk);
        for (_, cam) in self.cameras.lock().unwrap().drain() {
            cam.stop_all();
        }
        self.pairing_epoch.send_modify(|e| *e += 1);
        tracing::info!(device_id = %self.device_id(), "pairing reset; accessory is pairable again");
        self.advertise();
        Ok(())
    }

    pub fn pairing_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.pairing_epoch.subscribe()
    }

    // ---- live HAP connections ---------------------------------------------------

    pub fn conn_opened(&self, peer: IpAddr) -> u64 {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.connections.lock().unwrap().insert(id, LiveConn { peer, controller: None, since: Instant::now() });
        id
    }

    pub fn conn_verified(&self, id: u64, controller: &str) {
        if let Some(c) = self.connections.lock().unwrap().get_mut(&id) {
            c.controller = Some(controller.to_string());
        }
    }

    pub fn conn_closed(&self, id: u64) {
        self.connections.lock().unwrap().remove(&id);
    }

    pub fn connections(&self) -> Vec<LiveConn> {
        self.connections.lock().unwrap().values().cloned().collect()
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

    /// Changes a widget in place, keeping its id and aid (so HomeKit keeps its
    /// room and settings).
    pub fn update_widget(&self, id: &str, req: NewWidget) -> Result<Option<WidgetDef>> {
        let updated = self.with_store(|s| {
            let Some(w) = s.data.widgets.iter_mut().find(|w| w.id == id) else { return Ok(None) };
            let old = w.clone();
            *w = req.into_def(old.id.clone(), old.aid);
            let new = w.clone();
            s.save()?;
            Ok::<_, anyhow::Error>(Some((old, new)))
        })?;
        let Some((old, new)) = updated else { return Ok(None) };
        if old.source != new.source {
            self.series.write().unwrap().remove(id);
            self.readings.write().unwrap().remove(id);
        }
        self.touch(id);
        tracing::info!(id, aid = new.aid, "widget updated");
        if old.name != new.name {
            self.bump_config();
        }
        Ok(Some(new))
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
        self.readings.write().unwrap().remove(id);
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

    /// Seeds an empty series with historical samples (oldest first).
    pub fn backfill(&self, id: &str, values: Vec<f64>) {
        let mut series = self.series.write().unwrap();
        let s = series.entry(id.to_string()).or_default();
        if !s.samples.is_empty() {
            return;
        }
        let skip = values.len().saturating_sub(HISTORY);
        s.samples.extend(values.into_iter().skip(skip).filter(|v| v.is_finite()));
        s.version += 1;
    }

    // ---- remote readings -------------------------------------------------------

    pub fn set_reading(&self, id: &str, value: f64, unit: Option<String>) {
        let cleared = {
            let mut readings = self.readings.write().unwrap();
            let r = readings.entry(id.to_string()).or_default();
            r.value = Some(value);
            r.unit = unit.filter(|u| !u.is_empty());
            r.error.take().is_some()
        };
        if cleared {
            tracing::info!(id, "source recovered");
            self.touch(id);
        }
    }

    /// Records a source problem. The widget keeps its history but stops
    /// advancing and shows the message until a value arrives again.
    pub fn set_source_error(&self, id: &str, msg: impl Into<String>) {
        let msg = msg.into();
        let changed = {
            let mut readings = self.readings.write().unwrap();
            let r = readings.entry(id.to_string()).or_default();
            let changed = r.error.as_deref() != Some(msg.as_str());
            r.error = Some(msg.clone());
            changed
        };
        if changed {
            tracing::warn!(id, "source error: {msg}");
            self.touch(id);
        }
    }

    /// The held value to sample for a remote widget, unless its source is failing.
    pub fn reading(&self, id: &str) -> Option<f64> {
        self.readings.read().unwrap().get(id).filter(|r| r.error.is_none()).and_then(|r| r.value)
    }

    pub fn source_status(&self, id: &str) -> Option<String> {
        let def = self.with_store(|s| s.data.widgets.iter().find(|w| w.id == id).cloned())?;
        let readings = self.readings.read().unwrap();
        match readings.get(id) {
            Some(Reading { error: Some(e), .. }) => Some(e.clone()),
            Some(Reading { value: Some(_), .. }) => None,
            _ if def.source.is_remote() => Some("waiting for data".into()),
            _ => None,
        }
    }

    /// Forces streams to re-render (e.g. after the status line changed).
    fn touch(&self, id: &str) {
        self.series.write().unwrap().entry(id.to_string()).or_default().version += 1;
    }

    pub fn snapshot(&self, id: &str) -> Option<WidgetSnapshot> {
        let mut def = self.with_store(|s| s.data.widgets.iter().find(|w| w.id == id).cloned())?;
        let status = self.source_status(id);
        if def.unit.is_empty() {
            if let Some(unit) = self.readings.read().unwrap().get(id).and_then(|r| r.unit.clone()) {
                def.unit = unit;
            }
        }
        let series = self.series.read().unwrap();
        let (samples, version) = series
            .get(id)
            .map(|s| (s.samples.iter().copied().collect(), s.version))
            .unwrap_or_default();
        Some(WidgetSnapshot { def, samples, status, version })
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
