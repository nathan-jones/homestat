//! Data provider settings (Home Assistant, Prometheus, MQTT).
//!
//! Each provider's effective settings come from, in order:
//!  1. *locked*: command-line flags / environment variables, or the Home
//!     Assistant add-on (Supervisor-provided Home Assistant, add-on options).
//!     Shown read-only in the web UI.
//!  2. *saved*: set in the web UI, stored in `<data>/providers.json`.
//!  3. *default*: auto-detected (the Mosquitto add-on's broker).
//!
//! Changes are pushed to the running sources through a watch channel, so they
//! apply without a restart.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;
use url::Url;

use crate::widgets::remote::{Connections, HomeAssistant, Mqtt};

#[derive(Serialize, Deserialize, Default, Clone)]
struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home_assistant: Option<HomeAssistant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prometheus: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mqtt: Option<Mqtt>,
}

/// Settings that the web UI can't change, with where they came from.
#[derive(Default)]
pub struct Fixed {
    pub home_assistant: Option<(HomeAssistant, String)>,
    pub prometheus: Option<(Url, String)>,
    pub mqtt: Option<(Mqtt, String)>,
}

pub struct Providers {
    path: PathBuf,
    saved: Mutex<Saved>,
    locked: Fixed,
    defaults: Fixed,
    tx: watch::Sender<Connections>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    HomeAssistant,
    Prometheus,
    Mqtt,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "home_assistant" => Some(Kind::HomeAssistant),
            "prometheus" => Some(Kind::Prometheus),
            "mqtt" => Some(Kind::Mqtt),
            _ => None,
        }
    }
}

/// What the web UI submits. Empty secret fields mean "keep the saved one".
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Input {
    pub url: String,
    pub token: String,
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub password: String,
}

impl Providers {
    pub fn load(data_dir: &Path, locked: Fixed, defaults: Fixed) -> Result<Providers> {
        let path = data_dir.join("providers.json");
        let saved = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                // Don't refuse to start over a damaged file (e.g. power loss).
                let bad = path.with_extension("json.bad");
                tracing::error!("providers.json is unreadable ({e}); moved to {} and starting without saved data sources", bad.display());
                let _ = std::fs::rename(&path, &bad);
                Saved::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e).context("reading providers.json"),
        };
        let p = Providers { path, saved: Mutex::new(saved), locked, defaults, tx: watch::Sender::new(Connections::default()) };
        p.publish();
        Ok(p)
    }

    pub fn subscribe(&self) -> watch::Receiver<Connections> {
        self.tx.subscribe()
    }

    pub fn current(&self) -> Connections {
        self.tx.borrow().clone()
    }

    fn effective(&self, saved: &Saved) -> Connections {
        fn pick<T: Clone>(locked: &Option<(T, String)>, saved: &Option<T>, default: &Option<(T, String)>) -> Option<T> {
            locked.as_ref().map(|l| l.0.clone()).or_else(|| saved.clone()).or_else(|| default.as_ref().map(|d| d.0.clone()))
        }
        Connections {
            home_assistant: pick(&self.locked.home_assistant, &saved.home_assistant, &self.defaults.home_assistant),
            prometheus: pick(&self.locked.prometheus, &saved.prometheus, &self.defaults.prometheus),
            mqtt: pick(&self.locked.mqtt, &saved.mqtt, &self.defaults.mqtt),
        }
    }

    fn publish(&self) {
        let conns = self.effective(&self.saved.lock().unwrap());
        self.tx.send_if_modified(|cur| {
            let changed = *cur != conns;
            *cur = conns;
            changed
        });
    }

    fn locked_by(&self, kind: Kind) -> Option<&str> {
        match kind {
            Kind::HomeAssistant => self.locked.home_assistant.as_ref().map(|l| l.1.as_str()),
            Kind::Prometheus => self.locked.prometheus.as_ref().map(|l| l.1.as_str()),
            Kind::Mqtt => self.locked.mqtt.as_ref().map(|l| l.1.as_str()),
        }
    }

    /// Settings for the web UI. Secrets are never returned, only whether set.
    pub fn view(&self) -> Value {
        let saved = self.saved.lock().unwrap().clone();
        let conns = self.effective(&saved);
        let origin = |locked: Option<&String>, saved: bool, default: Option<&String>| match (locked, saved, default) {
            (Some(by), _, _) => json!({ "kind": "locked", "by": by }),
            (None, true, _) => json!({ "kind": "saved" }),
            (None, false, Some(by)) => json!({ "kind": "default", "by": by }),
            _ => json!({ "kind": "none" }),
        };
        json!({
            "home_assistant": {
                "configured": conns.home_assistant.is_some(),
                "origin": origin(self.locked.home_assistant.as_ref().map(|l| &l.1), saved.home_assistant.is_some(), self.defaults.home_assistant.as_ref().map(|d| &d.1)),
                "url": conns.home_assistant.as_ref().map(|h| redact(&h.url)),
                "token_set": conns.home_assistant.as_ref().is_some_and(|h| !h.token.is_empty()),
            },
            "prometheus": {
                "configured": conns.prometheus.is_some(),
                "origin": origin(self.locked.prometheus.as_ref().map(|l| &l.1), saved.prometheus.is_some(), self.defaults.prometheus.as_ref().map(|d| &d.1)),
                "url": conns.prometheus.as_ref().map(redact),
                "password_set": conns.prometheus.as_ref().is_some_and(|u| u.password().is_some()),
            },
            "mqtt": {
                "configured": conns.mqtt.is_some(),
                "origin": origin(self.locked.mqtt.as_ref().map(|l| &l.1), saved.mqtt.is_some(), self.defaults.mqtt.as_ref().map(|d| &d.1)),
                "host": conns.mqtt.as_ref().map(|m| m.host.as_str()),
                "port": conns.mqtt.as_ref().map(|m| m.port),
                "username": conns.mqtt.as_ref().and_then(|m| m.username.as_deref()),
                "password_set": conns.mqtt.as_ref().is_some_and(|m| m.password.is_some()),
            },
        })
    }

    /// Turns UI input into settings. A blank secret reuses the *saved* one,
    /// but only for the same server (and user), so a token or password is
    /// never sent somewhere new without being typed in again. Locked and
    /// auto-detected secrets are never reused.
    pub fn resolve(&self, kind: Kind, input: &Input) -> Result<Resolved> {
        if let Some(by) = self.locked_by(kind) {
            bail!("set by {by}; change it there");
        }
        let saved = self.saved.lock().unwrap().clone();
        Ok(match kind {
            Kind::HomeAssistant => {
                let url = http_url(&input.url)?;
                let token = match input.token.trim() {
                    "" => saved
                        .home_assistant
                        .filter(|h| same_server(&h.url, &url))
                        .map(|h| h.token)
                        .ok_or_else(|| anyhow!("enter the access token (needed for a new server)"))?,
                    t => t.to_string(),
                };
                Resolved::HomeAssistant(HomeAssistant { url, token })
            }
            Kind::Prometheus => Resolved::Prometheus(keep_url_password(http_url(&input.url)?, saved.prometheus.as_ref())),
            Kind::Mqtt => {
                let host = input.host.trim();
                let bad_colon = host.contains(':') && !(host.starts_with('[') && host.ends_with(']'));
                if host.is_empty() || host.contains(['/', ' ', '@']) || bad_colon {
                    bail!("enter the broker's host name or IP address (the port goes in its own field)");
                }
                let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
                let port = input.port.unwrap_or(1883);
                let username = Some(input.username.trim().to_string()).filter(|u| !u.is_empty());
                let password = match input.password.as_str() {
                    "" if username.is_some() => saved
                        .mqtt
                        .filter(|m| m.host == host && m.port == port && m.username == username)
                        .and_then(|m| m.password),
                    "" => None,
                    p => Some(p.to_string()),
                };
                Resolved::Mqtt(Mqtt { host, port, username, password })
            }
        })
    }

    /// Saves settings for a provider (`None` clears them) and applies them.
    pub fn save(&self, kind: Kind, value: Option<Resolved>) -> Result<()> {
        if let Some(by) = self.locked_by(kind) {
            bail!("set by {by}; change it there");
        }
        {
            let mut saved = self.saved.lock().unwrap();
            let mut next = saved.clone();
            match (kind, value) {
                (Kind::HomeAssistant, Some(Resolved::HomeAssistant(h))) => next.home_assistant = Some(h),
                (Kind::HomeAssistant, None) => next.home_assistant = None,
                (Kind::Prometheus, Some(Resolved::Prometheus(u))) => next.prometheus = Some(u),
                (Kind::Prometheus, None) => next.prometheus = None,
                (Kind::Mqtt, Some(Resolved::Mqtt(m))) => next.mqtt = Some(m),
                (Kind::Mqtt, None) => next.mqtt = None,
                _ => bail!("settings don't match the provider"),
            }
            write_private(&self.path, &serde_json::to_vec_pretty(&next)?)?;
            *saved = next;
        }
        tracing::info!(?kind, "data provider settings saved");
        self.publish();
        Ok(())
    }
}

pub enum Resolved {
    HomeAssistant(HomeAssistant),
    Prometheus(Url),
    Mqtt(Mqtt),
}

fn same_server(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// `http://user@host` with no password keeps the saved password for the
/// same server and user (the UI never receives it, see `redact`).
fn keep_url_password(mut url: Url, saved: Option<&Url>) -> Url {
    if url.password().is_none() && !url.username().is_empty() {
        if let Some(old) = saved.filter(|o| same_server(o, &url) && o.username() == url.username()) {
            let _ = url.set_password(old.password());
        }
    }
    url
}

/// A URL safe to show or log: any password is removed.
pub fn redact(url: &Url) -> String {
    let mut u = url.clone();
    let _ = u.set_password(None);
    u.to_string()
}

fn http_url(s: &str) -> Result<Url> {
    let s = s.trim();
    // "prometheus:9090" would parse with scheme "prometheus", so add one.
    let with_scheme = if s.contains("://") { s.to_string() } else { format!("http://{s}") };
    let url = Url::parse(&with_scheme).context("enter a URL like http://host:port")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        bail!("enter a URL like http://host:port");
    }
    Ok(url)
}

/// Writes atomically, readable only by us (the file holds tokens/passwords).
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("homestat-providers-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn precedence_and_persistence() {
        let d = dir();
        let broker = |h: &str| Mqtt { host: h.into(), port: 1883, username: None, password: None };
        let defaults = Fixed { mqtt: Some((broker("auto"), "Mosquitto add-on".into())), ..Default::default() };
        let locked = Fixed { prometheus: Some((Url::parse("http://env:9090").unwrap(), "env".into())), ..Default::default() };
        let p = Providers::load(&d, locked, defaults).unwrap();
        assert_eq!(p.current().mqtt.unwrap().host, "auto");
        assert!(p.save(Kind::Prometheus, None).is_err(), "locked providers can't be changed");

        let input = Input { host: "saved".into(), username: "u".into(), password: "pw".into(), ..Default::default() };
        p.save(Kind::Mqtt, Some(p.resolve(Kind::Mqtt, &input).unwrap())).unwrap();
        assert_eq!(p.current().mqtt.as_ref().unwrap().host, "saved");

        // Blank password keeps the saved one (same broker and user).
        let input = Input { host: "saved".into(), username: "u".into(), ..Default::default() };
        let Resolved::Mqtt(m) = p.resolve(Kind::Mqtt, &input).unwrap() else { panic!() };
        assert_eq!(m.password.as_deref(), Some("pw"));

        // Survives a restart, and secrets aren't in the view.
        let p2 = Providers::load(&d, Fixed::default(), Fixed::default()).unwrap();
        assert_eq!(p2.current().mqtt.unwrap().host, "saved");
        assert!(!p2.view().to_string().contains("pw"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join("providers.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn secrets_only_reused_for_the_same_server() {
        let d = dir();
        let locked = Fixed {
            home_assistant: Some((HomeAssistant { url: Url::parse("http://supervisor/core").unwrap(), token: "SUP".into() }, "x".into())),
            ..Default::default()
        };
        let p = Providers::load(&d, locked, Fixed::default()).unwrap();
        // Locked: can't even be tested with other settings (would leak SUP).
        let probe = Input { url: "http://attacker".into(), ..Default::default() };
        assert!(p.resolve(Kind::HomeAssistant, &probe).is_err());

        let p = Providers::load(&d, Fixed::default(), Fixed::default()).unwrap();
        let first = Input { host: "a".into(), username: "u".into(), password: "pw".into(), ..Default::default() };
        p.save(Kind::Mqtt, Some(p.resolve(Kind::Mqtt, &first).unwrap())).unwrap();
        let elsewhere = Input { host: "attacker".into(), username: "u".into(), ..Default::default() };
        let Resolved::Mqtt(m) = p.resolve(Kind::Mqtt, &elsewhere).unwrap() else { panic!() };
        assert_eq!(m.password, None);
        assert!(p.resolve(Kind::Mqtt, &Input { host: "a:1883".into(), ..Default::default() }).is_err());

        // Prometheus basic-auth password: hidden from the view, kept on re-save.
        let with_pw = Input { url: "http://me:s3cret@prom:9090".into(), ..Default::default() };
        p.save(Kind::Prometheus, Some(p.resolve(Kind::Prometheus, &with_pw).unwrap())).unwrap();
        assert!(!p.view().to_string().contains("s3cret"));
        let resave = Input { url: p.view()["prometheus"]["url"].as_str().unwrap().into(), ..Default::default() };
        let Resolved::Prometheus(u) = p.resolve(Kind::Prometheus, &resave).unwrap() else { panic!() };
        assert_eq!(u.password(), Some("s3cret"));
        let moved = Input { url: "http://me@evil:9090".into(), ..Default::default() };
        let Resolved::Prometheus(u) = p.resolve(Kind::Prometheus, &moved).unwrap() else { panic!() };
        assert_eq!(u.password(), None);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn damaged_file_does_not_block_startup() {
        let d = dir();
        std::fs::write(d.join("providers.json"), "").unwrap();
        let p = Providers::load(&d, Fixed::default(), Fixed::default()).unwrap();
        assert!(p.current().mqtt.is_none());
        assert!(d.join("providers.json.bad").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn urls() {
        assert_eq!(http_url("prometheus:9090").unwrap().as_str(), "http://prometheus:9090/");
        assert!(http_url("ftp://x").is_err());
        assert!(http_url("").is_err());
    }
}
