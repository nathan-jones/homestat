//! Home Assistant add-on mode.
//!
//! Inside an add-on the Supervisor provides `SUPERVISOR_TOKEN` and writes the
//! user's options to `/data/options.json`. We map those onto the normal
//! settings: Home Assistant is reached through the Supervisor proxy, MQTT
//! credentials come from the Mosquitto add-on (if installed), and the admin UI
//! is served on the ingress port.

use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use url::Url;

use crate::widgets::remote::{HomeAssistant, Mqtt};

const OPTIONS: &str = "/data/options.json";
pub const INGRESS_PORT: u16 = 8099;
/// Address the Supervisor's ingress proxy connects from.
const INGRESS_PROXY: [u8; 4] = [172, 30, 32, 2];

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct Options {
    pub name: Option<String>,
    /// HomeKit (HAP) port; change it if Homebridge already uses 51826.
    pub port: Option<u16>,
    pub setup_code: Option<String>,
    pub sample_interval: Option<u64>,
    /// Also serve the admin UI/API on the LAN (port 8099), not only via ingress.
    pub api_lan_access: bool,
    pub prometheus_url: Option<String>,
    /// Overrides the broker discovered from the Mosquitto add-on.
    pub mqtt_url: Option<String>,
}

pub struct Addon {
    pub options: Options,
    pub data_dir: PathBuf,
    pub home_assistant: HomeAssistant,
    token: String,
}

/// `Some` when running as a Home Assistant add-on.
pub fn detect() -> Result<Option<Addon>> {
    let Ok(token) = std::env::var("SUPERVISOR_TOKEN") else { return Ok(None) };
    // Overridable so add-on mode can be exercised outside Home Assistant.
    let path = std::env::var("HOMESTAT_ADDON_OPTIONS").map(PathBuf::from).unwrap_or_else(|_| OPTIONS.into());
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    let options: Options = serde_json::from_str(&raw).context("parsing add-on options")?;
    Ok(Some(Addon {
        options,
        data_dir: path.parent().unwrap_or(Path::new("/data")).to_path_buf(),
        home_assistant: HomeAssistant { url: Url::parse("http://supervisor/core")?, token: token.clone() },
        token,
    }))
}

impl Addon {
    /// Who may reach the admin API: only the ingress proxy, unless LAN access is on.
    pub fn api_allow(&self) -> Vec<IpAddr> {
        if self.options.api_lan_access { vec![] } else { vec![IpAddr::from(INGRESS_PROXY)] }
    }

    /// MQTT broker from the options, else from the Supervisor's service registry.
    pub async fn mqtt(&self) -> Option<Mqtt> {
        if let Some(url) = self.options.mqtt_url.as_deref().filter(|s| !s.is_empty()) {
            return match Mqtt::from_url(url) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::error!("mqtt_url: {e:#}");
                    None
                }
            };
        }
        match self.discover_mqtt().await {
            Ok(m) => m,
            Err(e) => {
                tracing::info!("no MQTT service from the Supervisor ({e:#}); MQTT widgets disabled");
                None
            }
        }
    }

    async fn discover_mqtt(&self) -> Result<Option<Mqtt>> {
        #[derive(Deserialize)]
        struct Resp {
            data: Service,
        }
        #[derive(Deserialize)]
        struct Service {
            host: String,
            port: u16,
            #[serde(default)]
            ssl: bool,
            username: Option<String>,
            password: Option<String>,
        }
        let resp = reqwest::Client::new()
            .get("http://supervisor/services/mqtt")
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        let svc = resp.json::<Resp>().await?.data;
        if svc.ssl {
            tracing::warn!("the MQTT service requires TLS, which homestat doesn't support yet");
            return Ok(None);
        }
        // Host-network add-ons can't always resolve the internal add-on
        // hostname; Mosquitto also publishes its port on the host.
        let host = match (svc.host.as_str(), svc.port).to_socket_addrs() {
            Ok(_) => svc.host,
            Err(_) => {
                tracing::info!("can't resolve MQTT host {}; using 127.0.0.1", svc.host);
                "127.0.0.1".into()
            }
        };
        tracing::info!("using MQTT broker {host}:{} from the Supervisor", svc.port);
        Ok(Some(Mqtt { host, port: svc.port, username: svc.username, password: svc.password }))
    }
}
