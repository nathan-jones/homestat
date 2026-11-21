//! The sampler: appends one sample per widget per interval. Built-in sources
//! are measured here; remote sources (see `remote`) contribute their latest
//! held value, so every sparkline advances at the same steady rate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use super::{Source, SystemMetric};
use crate::hub::Hub;

pub fn spawn_sampler(hub: Arc<Hub>, interval: Duration) {
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut sys = match System::new() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("system metrics unavailable: {e:?}");
                    return;
                }
            };
            let mut demo: HashMap<String, f64> = HashMap::new();
            let mut rng = rand::thread_rng();
            loop {
                let widgets = hub.widgets();
                let needs = |m: SystemMetric| {
                    widgets.iter().any(|w| w.source == Source::System { metric: m })
                };
                if needs(SystemMetric::Processes) {
                    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
                }
                if needs(SystemMetric::Cpu) {
                    sys.refresh_cpu_usage();
                }
                if needs(SystemMetric::Memory) {
                    sys.refresh_memory();
                }
                for w in &widgets {
                    let value = match &w.source {
                        Source::Push => continue,
                        s if s.is_remote() => match hub.reading(&w.id) {
                            Some(v) => v,
                            None => continue,
                        },
                        Source::System { metric } => match metric {
                            SystemMetric::Processes => sys.processes().len() as f64,
                            SystemMetric::Cpu => sys.global_cpu_usage() as f64,
                            SystemMetric::Memory => {
                                100.0 * sys.used_memory() as f64 / sys.total_memory().max(1) as f64
                            }
                            SystemMetric::Load => match System::load_average() {
                                Ok(l) => l.one,
                                Err(_) => continue,
                            },
                        },
                        Source::Demo => {
                            let v = demo.entry(w.id.clone()).or_insert_with(|| rng.gen_range(20.0..80.0));
                            *v = (*v + rng.gen_range(-4.0..4.0)).clamp(0.0, 100.0);
                            *v
                        }
                        _ => continue,
                    };
                    hub.push_value(&w.id, value);
                }
                std::thread::sleep(interval);
            }
        })
        .expect("spawn sampler thread");
}
