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
    /// Semicircular dial between `min` and `max` (default 0–100).
    Gauge,
    /// Big value over a horizontal bar between `min` and `max` (default 0–100).
    Bar,
    /// Full time-series chart with a y-axis; auto-scaled unless `min`/`max` set.
    Graph,
    /// A word for the current value, from `states` (e.g. 0 → Closed, 1 → Open).
    State,
}

/// One entry of a State widget's value → label mapping.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StateMap {
    pub value: f64,
    pub label: String,
    /// Colour of the label and the tile's tint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgb>,
}

/// A colour, written `#rrggbb` (`#rgb` is accepted too).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Rgb(pub [u8; 3]);

impl TryFrom<String> for Rgb {
    type Error = String;
    fn try_from(s: String) -> Result<Rgb, String> {
        let hex = s.trim().trim_start_matches('#');
        let hex: String = match hex.len() {
            3 => hex.chars().flat_map(|c| [c, c]).collect(),
            6 => hex.to_string(),
            _ => return Err(format!("invalid colour '{s}', expected #rrggbb")),
        };
        let v = u32::from_str_radix(&hex, 16).map_err(|_| format!("invalid colour '{s}', expected #rrggbb"))?;
        Ok(Rgb([(v >> 16) as u8, (v >> 8) as u8, v as u8]))
    }
}

impl From<Rgb> for String {
    fn from(c: Rgb) -> String {
        format!("#{:02x}{:02x}{:02x}", c.0[0], c.0[1], c.0[2])
    }
}

/// Per-widget colours. Unset ones use the default (dark, Grafana-like)
/// theme; an unset graph fill is a tint of the line colour.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Colors {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgb>,
    /// Title and value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<Rgb>,
    /// Sparkline stroke.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<Rgb>,
    /// Area under the sparkline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<Rgb>,
}

impl Colors {
    pub fn is_default(&self) -> bool {
        *self == Colors::default()
    }
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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
    #[serde(default, skip_serializing_if = "Colors::is_default")]
    pub colors: Colors,
    /// Scale for gauge/bar/graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Labels for the State style.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<StateMap>,
}

#[derive(Deserialize, Debug, Clone)]
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
    #[serde(default)]
    pub colors: Colors,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub states: Vec<StateMap>,
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
            colors: self.colors,
            min: self.min,
            max: self.max,
            states: self.states,
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
