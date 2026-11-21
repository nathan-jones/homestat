//! Remote data sources: Home Assistant, Prometheus and MQTT.
//!
//! Each source records its latest value with `Hub::set_reading` (or a problem
//! with `Hub::set_source_error`); the sampler turns those into samples. Home
//! Assistant and Prometheus are polled once per sample interval; MQTT is
//! push-based and the last message is held. On first sight of a widget, its
//! sparkline is backfilled from the source's history where one exists.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use url::Url;

use super::{Source, WidgetDef};
use crate::hub::{Hub, HISTORY};

#[derive(Clone, Debug)]
pub struct HomeAssistant {
    /// Base URL, e.g. `http://homeassistant.local:8123` (or
    /// `http://supervisor/core` inside an add-on).
    pub url: Url,
    pub token: String,
}

#[derive(Clone, Debug)]
pub struct Mqtt {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Mqtt {
    /// Parses `mqtt://[user[:password]@]host[:port]`.
    pub fn from_url(s: &str) -> Result<Mqtt> {
        let url = Url::parse(s).context("invalid MQTT URL")?;
        if url.scheme() != "mqtt" && url.scheme() != "tcp" {
            bail!("MQTT URL must start with mqtt:// (TLS is not supported yet)");
        }
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        Ok(Mqtt {
            host: url.host_str().ok_or_else(|| anyhow!("MQTT URL has no host"))?.to_string(),
            port: url.port().unwrap_or(1883),
            username: (!url.username().is_empty()).then(|| decode(url.username())),
            password: url.password().map(decode),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Connections {
    pub home_assistant: Option<HomeAssistant>,
    pub prometheus: Option<Url>,
    pub mqtt: Option<Mqtt>,
}

pub fn spawn(hub: Arc<Hub>, conns: Connections, interval: Duration) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("homestat/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client");
    if let Some(mqtt) = conns.mqtt.clone() {
        tokio::spawn(run_mqtt(hub.clone(), mqtt));
    }
    tokio::spawn(run_pollers(hub, conns, http, interval));
}

// ---- polling (Home Assistant, Prometheus) ---------------------------------------

async fn run_pollers(hub: Arc<Hub>, conns: Connections, http: reqwest::Client, interval: Duration) {
    let conns = Arc::new(conns);
    let mut backfilled: HashSet<String> = HashSet::new();
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mut tasks = tokio::task::JoinSet::new();
        for w in hub.widgets() {
            match &w.source {
                Source::Mqtt { .. } if conns.mqtt.is_none() => {
                    hub.set_source_error(&w.id, "MQTT is not configured");
                }
                Source::HomeAssistant { .. } | Source::Prometheus { .. } => {
                    let first = backfilled.insert(backfill_key(&w));
                    let (hub, conns, http) = (hub.clone(), conns.clone(), http.clone());
                    tasks.spawn(async move {
                        if first {
                            if let Err(e) = backfill(&hub, &conns, &http, &w, interval).await {
                                tracing::debug!(id = %w.id, "no history backfill: {e:#}");
                            }
                        }
                        match poll(&conns, &http, &w.source).await {
                            Ok((v, unit)) => hub.set_reading(&w.id, v, unit),
                            Err(e) => hub.set_source_error(&w.id, format!("{e:#}")),
                        }
                    });
                }
                _ => {}
            }
        }
        // Forget removed/changed widgets so they're backfilled again.
        let live: HashSet<String> = hub.widgets().iter().map(backfill_key).collect();
        backfilled.retain(|k| live.contains(k));
        while tasks.join_next().await.is_some() {}
    }
}

fn backfill_key(w: &WidgetDef) -> String {
    format!("{}|{:?}", w.id, w.source)
}

async fn poll(conns: &Connections, http: &reqwest::Client, source: &Source) -> Result<(f64, Option<String>)> {
    match source {
        Source::HomeAssistant { entity_id, attribute } => {
            let ha = conns.home_assistant.as_ref().ok_or_else(|| anyhow!("Home Assistant is not configured"))?;
            ha_state(http, ha, entity_id, attribute.as_deref()).await
        }
        Source::Prometheus { query } => {
            let base = conns.prometheus.as_ref().ok_or_else(|| anyhow!("Prometheus is not configured"))?;
            Ok((prom_query(http, base, query).await?, None))
        }
        _ => unreachable!("not a polled source"),
    }
}

async fn backfill(hub: &Hub, conns: &Connections, http: &reqwest::Client, w: &WidgetDef, step: Duration) -> Result<()> {
    let values = match &w.source {
        Source::HomeAssistant { entity_id, attribute: None } => match &conns.home_assistant {
            Some(ha) => ha_history(http, ha, entity_id, step).await?,
            None => return Ok(()),
        },
        Source::Prometheus { query } => match &conns.prometheus {
            Some(base) => prom_range(http, base, query, step).await?,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    tracing::info!(id = %w.id, samples = values.len(), "backfilled history");
    hub.backfill(&w.id, values);
    Ok(())
}

fn endpoint(base: &Url, path: &str) -> Result<Url> {
    let mut s = base.as_str().trim_end_matches('/').to_string();
    s.push_str(path);
    Ok(Url::parse(&s)?)
}

async fn get_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await.map_err(|e| anyhow!("request failed: {}", root_cause(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Prometheus says {"error": ...}, Home Assistant {"message": ...}.
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|j| ["error", "message"].iter().find_map(|k| j[k].as_str().map(str::to_string)))
            .or_else(|| short(&body));
        bail!("HTTP {}{}", status.as_u16(), msg.map(|m| format!(": {m}")).unwrap_or_default());
    }
    resp.json().await.context("invalid JSON")
}

fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

fn short(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(120).collect())
}

// ---- Home Assistant ---------------------------------------------------------------

fn valid_entity_id(id: &str) -> bool {
    id.split_once('.').is_some_and(|(d, o)| !d.is_empty() && !o.is_empty())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

async fn ha_state(
    http: &reqwest::Client,
    ha: &HomeAssistant,
    entity_id: &str,
    attribute: Option<&str>,
) -> Result<(f64, Option<String>)> {
    if !valid_entity_id(entity_id) {
        bail!("invalid entity id '{entity_id}'");
    }
    let url = endpoint(&ha.url, &format!("/api/states/{entity_id}"))?;
    let state = get_json(http.get(url).bearer_auth(&ha.token)).await.map_err(|e| {
        if e.to_string().starts_with("HTTP 404") { anyhow!("entity {entity_id} not found") } else { e }
    })?;
    let attrs = &state["attributes"];
    match attribute {
        Some(attr) => {
            let v = attrs.get(attr).ok_or_else(|| anyhow!("{entity_id} has no attribute '{attr}'"))?;
            Ok((to_number(v).ok_or_else(|| anyhow!("{attr} is not numeric: {v}"))?, None))
        }
        None => {
            let raw = &state["state"];
            if matches!(raw.as_str(), Some("unavailable" | "unknown")) {
                bail!("{entity_id} is {}", raw.as_str().unwrap());
            }
            let v = to_number(raw).ok_or_else(|| anyhow!("state is not numeric: {raw}"))?;
            Ok((v, attrs["unit_of_measurement"].as_str().map(str::to_string)))
        }
    }
}

/// The entity's state over the last `HISTORY` intervals, resampled with
/// sample-and-hold so it lines up with live samples.
async fn ha_history(http: &reqwest::Client, ha: &HomeAssistant, entity_id: &str, step: Duration) -> Result<Vec<f64>> {
    if !valid_entity_id(entity_id) {
        bail!("invalid entity id");
    }
    let now = jiff::Timestamp::now();
    let span = step * HISTORY as u32;
    let start = now - jiff::SignedDuration::try_from(span)?;
    let mut url = endpoint(&ha.url, &format!("/api/history/period/{start}"))?;
    url.query_pairs_mut()
        .append_pair("filter_entity_id", entity_id)
        .append_pair("end_time", &now.to_string())
        .append_key_only("minimal_response")
        .append_key_only("no_attributes");
    let body = get_json(http.get(url).bearer_auth(&ha.token)).await?;
    let changes: Vec<(jiff::Timestamp, f64)> = body
        .get(0)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let at: jiff::Timestamp = c["last_changed"].as_str()?.parse().ok()?;
            Some((at, to_number(&c["state"])?))
        })
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut current = None;
    for k in 0..HISTORY {
        let t = start + jiff::SignedDuration::try_from(step * k as u32)?;
        while i < changes.len() && changes[i].0 <= t {
            current = Some(changes[i].1);
            i += 1;
        }
        out.extend(current);
    }
    Ok(out)
}

// ---- Prometheus -------------------------------------------------------------------

fn prom_data(body: &Value) -> Result<&Value> {
    if body["status"] != "success" {
        bail!("{}", body["error"].as_str().unwrap_or("query failed"));
    }
    Ok(&body["data"])
}

fn prom_sample(pair: &Value) -> Option<f64> {
    pair.get(1)?.as_str()?.parse().ok()
}

async fn prom_query(http: &reqwest::Client, base: &Url, query: &str) -> Result<f64> {
    let mut url = endpoint(base, "/api/v1/query")?;
    url.query_pairs_mut().append_pair("query", query);
    let body = get_json(http.get(url)).await?;
    let data = prom_data(&body)?;
    let v = match data["resultType"].as_str() {
        Some("scalar") => prom_sample(&data["result"]),
        Some("vector") => {
            let series = data["result"].as_array().map(Vec::as_slice).unwrap_or_default();
            match series {
                [] => bail!("query returned no series"),
                [one] => prom_sample(&one["value"]),
                many => bail!("query returned {} series; aggregate it, e.g. sum(...)", many.len()),
            }
        }
        other => bail!("unsupported result type {other:?}"),
    };
    v.filter(|v| v.is_finite()).ok_or_else(|| anyhow!("query returned a non-numeric value"))
}

async fn prom_range(http: &reqwest::Client, base: &Url, query: &str, step: Duration) -> Result<Vec<f64>> {
    let end = jiff::Timestamp::now().as_second();
    let start = end - (step.as_secs().max(1) * HISTORY as u64) as i64;
    let mut url = endpoint(base, "/api/v1/query_range")?;
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("start", &start.to_string())
        .append_pair("end", &end.to_string())
        .append_pair("step", &format!("{}s", step.as_secs().max(1)));
    let body = get_json(http.get(url)).await?;
    let data = prom_data(&body)?;
    match data["result"].as_array().map(Vec::as_slice) {
        Some([one]) => Ok(one["values"].as_array().into_iter().flatten().filter_map(prom_sample).collect()),
        _ => bail!("range query did not return exactly one series"),
    }
}

// ---- MQTT -------------------------------------------------------------------------

async fn run_mqtt(hub: Arc<Hub>, cfg: Mqtt) {
    use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};

    let client_id = format!("homestat-{}", hub.device_id().replace(':', "").to_lowercase());
    let mut opts = MqttOptions::new(client_id, cfg.host.clone(), cfg.port);
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_clean_session(true);
    opts.set_max_packet_size(256 * 1024, 256 * 1024);
    if let Some(user) = &cfg.username {
        opts.set_credentials(user.clone(), cfg.password.clone().unwrap_or_default());
    }
    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    let mut subscribed: HashSet<String> = HashSet::new();
    let mut connected = false;
    let mut resync = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            ev = eventloop.poll() => match ev {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    tracing::info!("MQTT connected to {}:{}", cfg.host, cfg.port);
                    connected = true;
                    subscribed.clear();
                    resync.reset_immediately();
                }
                Ok(Event::Incoming(Packet::Publish(p))) => on_message(&hub, &p.topic, &p.payload),
                Ok(_) => {}
                Err(e) => {
                    if connected {
                        tracing::warn!("MQTT connection lost: {e}");
                    }
                    connected = false;
                    for w in hub.widgets().iter().filter(|w| matches!(w.source, Source::Mqtt { .. })) {
                        hub.set_source_error(&w.id, format!("MQTT broker unreachable: {e}"));
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            },
            _ = resync.tick(), if connected => {
                let wanted: HashSet<String> = hub
                    .widgets()
                    .into_iter()
                    .filter_map(|w| match w.source {
                        Source::Mqtt { topic, .. } if rumqttc::valid_filter(&topic) => Some(topic),
                        Source::Mqtt { .. } => {
                            hub.set_source_error(&w.id, "invalid MQTT topic filter");
                            None
                        }
                        _ => None,
                    })
                    .collect();
                for t in wanted.difference(&subscribed) {
                    tracing::debug!(topic = %t, "MQTT subscribe");
                    let _ = client.try_subscribe(t.clone(), QoS::AtMostOnce);
                }
                for t in subscribed.difference(&wanted) {
                    tracing::debug!(topic = %t, "MQTT unsubscribe");
                    let _ = client.try_unsubscribe(t.clone());
                }
                subscribed = wanted;
            }
        }
    }
}

fn on_message(hub: &Hub, topic: &str, payload: &[u8]) {
    for w in hub.widgets() {
        let Source::Mqtt { topic: filter, json_path } = &w.source else { continue };
        if !rumqttc::matches(topic, filter) {
            continue;
        }
        match parse_payload(payload, json_path.as_deref()) {
            Ok(v) => hub.set_reading(&w.id, v, None),
            Err(e) => hub.set_source_error(&w.id, format!("{topic}: {e:#}")),
        }
    }
}

fn parse_payload(payload: &[u8], json_path: Option<&str>) -> Result<f64> {
    let text = std::str::from_utf8(payload).context("payload is not UTF-8")?.trim();
    let Some(path) = json_path.filter(|p| !p.is_empty()) else {
        return to_number(&Value::String(text.to_string()))
            .or_else(|| serde_json::from_str::<Value>(text).ok().as_ref().and_then(to_number))
            .ok_or_else(|| anyhow!("payload is not numeric: {}", short(text).unwrap_or_default()));
    };
    let json: Value = serde_json::from_str(text).context("payload is not JSON")?;
    let mut cur = &json;
    for key in path.split('.') {
        cur = match cur {
            Value::Array(a) => key.parse::<usize>().ok().and_then(|i| a.get(i)),
            Value::Object(o) => o.get(key),
            _ => None,
        }
        .ok_or_else(|| anyhow!("no '{path}' in payload"))?;
    }
    to_number(cur).ok_or_else(|| anyhow!("'{path}' is not numeric: {cur}"))
}

/// Numbers, numeric strings, booleans and common on/off states.
pub fn to_number(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => {
            let s = s.trim();
            match s.to_ascii_lowercase().as_str() {
                "on" | "true" | "open" | "home" | "yes" => 1.0,
                "off" | "false" | "closed" | "not_home" | "no" => 0.0,
                _ => s.parse().ok()?,
            }
        }
        _ => return None,
    };
    n.is_finite().then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers() {
        assert_eq!(to_number(&json!(3.5)), Some(3.5));
        assert_eq!(to_number(&json!("21.4")), Some(21.4));
        assert_eq!(to_number(&json!("on")), Some(1.0));
        assert_eq!(to_number(&json!(false)), Some(0.0));
        assert_eq!(to_number(&json!("unavailable")), None);
        assert_eq!(to_number(&json!("NaN")), None);
    }

    #[test]
    fn payloads() {
        assert_eq!(parse_payload(b" 42 ", None).unwrap(), 42.0);
        assert_eq!(parse_payload(b"\"7\"", None).unwrap(), 7.0);
        let j = br#"{"sensor":{"temp":19.5},"vals":[1,2]}"#;
        assert_eq!(parse_payload(j, Some("sensor.temp")).unwrap(), 19.5);
        assert_eq!(parse_payload(j, Some("vals.1")).unwrap(), 2.0);
        assert!(parse_payload(j, Some("sensor.nope")).is_err());
        assert!(parse_payload(b"hello", None).is_err());
    }

    #[test]
    fn mqtt_urls() {
        let m = Mqtt::from_url("mqtt://user:p%40ss@broker.lan").unwrap();
        assert_eq!((m.host.as_str(), m.port), ("broker.lan", 1883));
        assert_eq!(m.username.as_deref(), Some("user"));
        assert_eq!(m.password.as_deref(), Some("p@ss"));
        assert!(Mqtt::from_url("mqtts://x").is_err());
    }

    #[test]
    fn prom_results() {
        let body = json!({"status":"success","data":{"resultType":"vector","result":[{"metric":{},"value":[1, "2.5"]}]}});
        let data = prom_data(&body).unwrap();
        assert_eq!(prom_sample(&data["result"][0]["value"]), Some(2.5));
        assert!(prom_data(&json!({"status":"error","error":"bad"})).is_err());
    }
}
