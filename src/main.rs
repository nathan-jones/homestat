mod addon;
mod api;
mod camera;
mod hap;
mod hub;
mod providers;
mod state;
mod widgets;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use url::Url;

use hap::mdns::Advertiser;
use hub::Hub;
use providers::{Fixed, Providers};
use state::Store;
use widgets::remote::{HomeAssistant, Mqtt};
use widgets::{NewWidget, Source, SystemMetric};

/// HomeKit bridge that exposes dynamically rendered stat widgets as cameras.
///
/// Every flag can also be set with the environment variable shown.
#[derive(Parser, Debug)]
struct Args {
    /// Name shown in the Home app when pairing.
    #[arg(long, env = "HOMESTAT_NAME", default_value = "Homestat")]
    name: String,
    /// HAP (HomeKit) TCP port.
    #[arg(long, env = "HOMESTAT_PORT", default_value_t = 51826)]
    port: u16,
    /// Admin API / web UI port.
    #[arg(long, env = "HOMESTAT_API_PORT", default_value_t = 8080)]
    api_port: u16,
    /// Only accept admin API connections from these IPs (comma separated;
    /// loopback is always allowed). Empty = anyone.
    #[arg(long, env = "HOMESTAT_API_ALLOW", value_delimiter = ',')]
    api_allow: Vec<IpAddr>,
    /// Where pairing keys and widget definitions are stored.
    #[arg(long, env = "HOMESTAT_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,
    /// Setup code (XXX-XX-XXX) to use on first run; random otherwise.
    #[arg(long, env = "HOMESTAT_SETUP_CODE")]
    setup_code: Option<String>,
    /// Seconds between samples (sparklines show the last 120).
    #[arg(long, env = "HOMESTAT_SAMPLE_INTERVAL", default_value_t = 5)]
    sample_interval: u64,
    /// Home Assistant base URL, e.g. http://homeassistant.local:8123
    #[arg(long, env = "HOMESTAT_HA_URL", requires = "ha_token")]
    ha_url: Option<Url>,
    /// Home Assistant long-lived access token.
    #[arg(long, env = "HOMESTAT_HA_TOKEN", hide_env_values = true)]
    ha_token: Option<String>,
    /// Prometheus base URL, e.g. http://prometheus:9090
    #[arg(long, env = "HOMESTAT_PROMETHEUS_URL")]
    prometheus_url: Option<Url>,
    /// MQTT broker, mqtt://[user[:password]@]host[:port]
    #[arg(long, env = "HOMESTAT_MQTT_URL", hide_env_values = true)]
    mqtt_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "homestat=info".into()),
        )
        .init();
    let mut args = Args::parse();

    // Settings from flags/env (and the add-on) are fixed; the rest can be
    // configured in the web UI (see `providers`).
    let env = |var: &str| format!("the {var} setting");
    let mut locked = Fixed {
        home_assistant: match (args.ha_url.take(), args.ha_token.take()) {
            (Some(url), Some(token)) => Some((HomeAssistant { url, token }, env("HOMESTAT_HA_URL"))),
            _ => None,
        },
        prometheus: args.prometheus_url.take().map(|u| (u, env("HOMESTAT_PROMETHEUS_URL"))),
        mqtt: args.mqtt_url.as_deref().map(Mqtt::from_url).transpose()?.map(|m| (m, env("HOMESTAT_MQTT_URL"))),
    };
    let mut defaults = Fixed::default();

    if let Some(addon) = addon::detect()? {
        tracing::info!("running as a Home Assistant add-on");
        let o = &addon.options;
        if let Some(name) = o.name.clone().filter(|s| !s.is_empty()) {
            args.name = name;
        }
        args.port = o.port.unwrap_or(args.port);
        args.setup_code = o.setup_code.clone().filter(|s| !s.is_empty()).or(args.setup_code);
        args.sample_interval = o.sample_interval.unwrap_or(args.sample_interval);
        args.data_dir = addon.data_dir.clone();
        args.api_port = addon::INGRESS_PORT;
        args.api_allow = addon.api_allow();
        locked.home_assistant = Some((addon.home_assistant.clone(), "the Home Assistant Supervisor".into()));
        if let Some(url) = o.prometheus_url.as_deref().filter(|s| !s.is_empty()) {
            locked.prometheus = Some((Url::parse(url)?, "the add-on's prometheus_url option".into()));
        }
        match addon.mqtt_option() {
            Ok(Some(m)) => locked.mqtt = Some((m, "the add-on's mqtt_url option".into())),
            other => {
                if let Err(e) = other {
                    tracing::error!("ignoring the mqtt_url option: {e:#}");
                }
                defaults.mqtt = addon.mqtt_service().await.map(|m| (m, "the Mosquitto add-on".into()));
            }
        }
    }
    let interval = Duration::from_secs(args.sample_interval.clamp(1, 3600));
    std::fs::create_dir_all(&args.data_dir)?;
    let providers = Arc::new(Providers::load(&args.data_dir, locked, defaults)?);
    let conns = providers.current();
    tracing::info!(
        home_assistant = conns.home_assistant.as_ref().map(|h| providers::redact(&h.url)),
        prometheus = conns.prometheus.as_ref().map(providers::redact),
        mqtt = conns.mqtt.as_ref().map(|m| format!("{}:{}", m.host, m.port)),
        "data sources"
    );

    let (store, first_run) = Store::load_or_create(&args.data_dir, args.setup_code)?;
    let mdns = Advertiser::new(&args.name, &store.data.device_id, args.port)?;
    let hub = Hub::new(args.name, store, mdns);

    if first_run {
        // Host-wide metrics (also meaningful inside a container).
        let defaults = [
            ("CPU", SystemMetric::Cpu, "%"),
            ("Memory", SystemMetric::Memory, "%"),
            ("Load", SystemMetric::Load, ""),
        ];
        for (name, metric, unit) in defaults {
            hub.add_widget(NewWidget {
                id: None,
                name: name.into(),
                style: Default::default(),
                source: Source::System { metric },
                unit: unit.into(),
                decimals: None,
                colors: Default::default(),
                min: None,
                max: None,
                states: vec![],
            })?;
        }
    }

    hub.ensure_db_schema()?;
    hub.advertise();
    widgets::sources::spawn_sampler(hub.clone(), interval);
    widgets::remote::spawn(hub.clone(), providers.subscribe(), interval);
    print_pairing_info(&hub);

    tokio::select! {
        r = hap::server::serve(hub.clone(), args.port) => r?,
        r = api::serve(hub.clone(), args.api_port, args.api_allow, providers.clone()) => r?,
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    hub.shutdown();
    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop` / the Supervisor.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn print_pairing_info(hub: &Hub) {
    if hub.is_paired() {
        tracing::info!("already paired ({} controller(s)); {} widget(s)", hub.pairings().len(), hub.widgets().len());
        return;
    }
    let uri = hub.setup_uri();
    if let Ok(code) = qrcode::QrCode::new(uri.as_bytes()) {
        use qrcode::render::unicode::Dense1x2;
        let qr = code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).build();
        println!("\n{qr}");
    }
    println!("\n  Add accessory in the Home app → scan the QR code, or enter: {}\n", hub.setup_code());
}
