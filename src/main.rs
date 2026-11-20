mod api;
mod camera;
mod hap;
mod hub;
mod state;
mod widgets;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use hap::mdns::Advertiser;
use hub::Hub;
use state::Store;
use widgets::{NewWidget, Source, SystemMetric};

/// HomeKit bridge that exposes dynamically rendered stat widgets as cameras.
#[derive(Parser, Debug)]
struct Args {
    /// Name shown in the Home app when pairing.
    #[arg(long, default_value = "Homestat")]
    name: String,
    /// HAP (HomeKit) TCP port.
    #[arg(long, default_value_t = 51826)]
    port: u16,
    /// Admin API / preview port.
    #[arg(long, default_value_t = 8080)]
    api_port: u16,
    /// Where pairing keys and widget definitions are stored.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// Setup code (XXX-XX-XXX) to use on first run; random otherwise.
    #[arg(long)]
    setup_code: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "homestat=info".into()),
        )
        .init();
    let args = Args::parse();

    let (store, first_run) = Store::load_or_create(&args.data_dir, args.setup_code)?;
    let mdns = Advertiser::new(&args.name, &store.data.device_id, args.port)?;
    let hub = Hub::new(args.name, store, mdns);

    if first_run {
        let defaults = [
            ("Processes", SystemMetric::Processes, ""),
            ("CPU", SystemMetric::Cpu, "%"),
            ("Memory", SystemMetric::Memory, "%"),
        ];
        for (name, metric, unit) in defaults {
            hub.add_widget(NewWidget {
                id: None,
                name: name.into(),
                style: Default::default(),
                source: Source::System { metric },
                unit: unit.into(),
                decimals: None,
            })?;
        }
    }

    hub.ensure_db_schema()?;
    hub.advertise();
    widgets::sources::spawn_sampler(hub.clone());
    print_pairing_info(&hub);

    tokio::select! {
        r = hap::server::serve(hub.clone(), args.port) => r?,
        r = api::serve(hub.clone(), args.api_port) => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    hub.shutdown();
    Ok(())
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
