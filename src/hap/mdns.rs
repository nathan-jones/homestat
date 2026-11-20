use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Publishes the `_hap._tcp` service. TXT updates (e.g. a bumped `c#` when
/// widgets change) go through a worker thread which:
///  * waits until the initial name probe has finished — re-registering during
///    probing makes mdns-sd see its own new TXT as a conflict and rename us;
///  * coalesces bursts of updates into a single announcement.
pub struct Advertiser {
    tx: Sender<HashMap<String, String>>,
    daemon: ServiceDaemon,
    fullname: String,
}

const PROBE_SETTLE: Duration = Duration::from_secs(3);
const COALESCE: Duration = Duration::from_millis(300);

impl Advertiser {
    pub fn new(instance: &str, device_id: &str, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let suffix = device_id.replace(':', "");
        let host = format!("homestat-{}.local.", &suffix[suffix.len().saturating_sub(6)..]);
        let instance = instance.to_string();
        let fullname = format!("{instance}._hap._tcp.local.");
        let worker_daemon = daemon.clone();
        let (tx, rx) = channel::<HashMap<String, String>>();

        std::thread::Builder::new().name("mdns".into()).spawn(move || {
            let mut first_registered: Option<Instant> = None;
            while let Ok(mut txt) = rx.recv() {
                if let Some(t) = first_registered {
                    if let Some(wait) = PROBE_SETTLE.checked_sub(t.elapsed()) {
                        std::thread::sleep(wait);
                    }
                }
                std::thread::sleep(COALESCE);
                while let Ok(newer) = rx.try_recv() {
                    txt = newer;
                }
                let info = ServiceInfo::new("_hap._tcp.local.", &instance, &host, "", port, txt)
                    .map(|i| i.enable_addr_auto());
                let mut info = match info {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::error!("invalid mDNS service info: {e}");
                        continue;
                    }
                };
                if first_registered.is_some() {
                    info.set_requires_probe(false);
                }
                match worker_daemon.register(info) {
                    Ok(()) => {
                        tracing::debug!("mDNS record published");
                        first_registered.get_or_insert_with(Instant::now);
                    }
                    Err(e) => tracing::error!("mDNS register failed: {e}"),
                }
            }
        })?;
        Ok(Advertiser { tx, daemon, fullname })
    }

    /// Sends goodbye packets so controllers drop us immediately instead of
    /// waiting for cached records to expire.
    pub fn shutdown(&self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(Duration::from_secs(1));
        }
    }

    pub fn publish(&self, txt: HashMap<String, String>) -> Result<()> {
        self.tx.send(txt)?;
        Ok(())
    }
}
