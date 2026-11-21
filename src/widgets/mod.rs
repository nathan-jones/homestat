//! Widget definitions. A widget = a data source + a visual style; each one is
//! exposed to HomeKit as its own camera.

pub mod remote;
pub mod render;
pub mod sources;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Title, big current value, sparkline (Grafana-style "singlestat").
    #[default]
    Stat,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SystemMetric {
    /// Number of running processes.
    Processes,
    /// Global CPU usage, percent.
    Cpu,
    /// Used memory, percent.
    Memory,
    /// 1-minute load average.
    Load,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Source {
    /// Values are pushed via `POST /api/widgets/{id}/value`.
    #[default]
    Push,
    /// Sampled from the host machine.
    System { metric: SystemMetric },
    /// Random walk, handy for demos.
    Demo,
    /// A Home Assistant entity's state (or one of its attributes).
    #[serde(rename = "homeassistant")]
    HomeAssistant {
        entity_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
    },
    /// A PromQL instant query that returns a single series.
    Prometheus { query: String },
    /// Numeric MQTT payloads, optionally picked out of JSON with a dotted path
    /// (`sensor.temperature`, `values.0`).
    Mqtt {
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        json_path: Option<String>,
    },
}

impl Source {
    /// Sources whose values arrive from outside and are held between samples.
    pub fn is_remote(&self) -> bool {
        matches!(self, Source::HomeAssistant { .. } | Source::Prometheus { .. } | Source::Mqtt { .. })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WidgetDef {
    pub id: String,
    /// HAP accessory id; stable for the widget's lifetime so HomeKit keeps
    /// room assignments etc.
    pub aid: u64,
    pub name: String,
    #[serde(default)]
    pub style: Style,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub decimals: Option<u8>,
}

#[derive(Deserialize, Debug)]
pub struct NewWidget {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub style: Style,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub decimals: Option<u8>,
}

impl NewWidget {
    pub fn into_def(self, id: String, aid: u64) -> WidgetDef {
        WidgetDef {
            id,
            aid,
            name: self.name,
            style: self.style,
            source: self.source,
            unit: self.unit,
            decimals: self.decimals,
        }
    }
}

/// Immutable view handed to the renderer.
#[derive(Clone, Debug)]
pub struct WidgetSnapshot {
    pub def: WidgetDef,
    pub samples: Vec<f64>,
    /// Problem with the data source (shown on the widget), if any.
    pub status: Option<String>,
    /// Bumped on every new sample; streams re-render only when it changes.
    pub version: u64,
}
