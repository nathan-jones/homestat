//! Admin HTTP API and web UI (separate from the HAP port).
//!
//! All URLs in the UI are relative so it also works behind Home Assistant
//! ingress, which serves it under `/api/hassio_ingress/<token>/`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::hub::Hub;
use crate::providers::{Input, Kind, Providers, Resolved};
use crate::widgets::remote;
use crate::widgets::{render, NewWidget, Source};

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    providers: Arc<Providers>,
}

pub async fn serve(hub: Arc<Hub>, port: u16, allow: Vec<IpAddr>, providers: Arc<Providers>) -> anyhow::Result<()> {
    let allow = Arc::new(allow);
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/pairing", get(pairing))
        .route("/api/pairing/reset", post(reset_pairing))
        .route("/api/pairing/qr.svg", get(pairing_qr))
        .route("/api/providers", get(providers_view))
        .route("/api/providers/{kind}", put(provider_save).delete(provider_clear))
        .route("/api/providers/{kind}/test", post(provider_test))
        .route("/api/widgets", get(list).post(create))
        .route("/api/widgets/{id}", get(get_one).put(update).delete(remove))
        .route("/api/widgets/{id}/value", post(push))
        .route("/api/widgets/{id}/preview.png", get(preview))
        .layer(middleware::from_fn(move |ci, req, next| allowlist(allow.clone(), ci, req, next)))
        .with_state(AppState { hub, providers });
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("web UI on http://localhost:{port}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

async fn allowlist(
    allow: Arc<Vec<IpAddr>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let ip = peer.ip().to_canonical();
    if allow.is_empty() || ip.is_loopback() || allow.contains(&ip) {
        next.run(req).await
    } else {
        tracing::debug!(%ip, "admin API request refused");
        StatusCode::FORBIDDEN.into_response()
    }
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

async fn status(State(st): State<AppState>) -> Json<serde_json::Value> {
    let hub = &st.hub;
    let conns = st.providers.current();
    Json(json!({
        "name": hub.name,
        "paired": hub.is_paired(),
        "setup_code": hub.setup_code(),
        "setup_uri": hub.setup_uri(),
        "sources": {
            "home_assistant": conns.home_assistant.is_some(),
            "prometheus": conns.prometheus.is_some(),
            "mqtt": conns.mqtt.is_some(),
        },
    }))
}

// ---- data providers -------------------------------------------------------------

async fn providers_view(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(st.providers.view())
}

fn kind(s: &str) -> Result<Kind, Response> {
    Kind::parse(s).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown provider"))
}

async fn provider_save(State(st): State<AppState>, Path(k): Path<String>, Json(input): Json<Input>) -> Response {
    let k = match kind(&k) {
        Ok(k) => k,
        Err(r) => return r,
    };
    let result = st.providers.resolve(k, &input).and_then(|r| st.providers.save(k, Some(r)));
    match result {
        Ok(()) => Json(st.providers.view()).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn provider_clear(State(st): State<AppState>, Path(k): Path<String>) -> Response {
    let k = match kind(&k) {
        Ok(k) => k,
        Err(r) => return r,
    };
    match st.providers.save(k, None) {
        Ok(()) => Json(st.providers.view()).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// Tries the submitted settings without saving them.
async fn provider_test(State(st): State<AppState>, Path(k): Path<String>, Json(input): Json<Input>) -> Response {
    let k = match kind(&k) {
        Ok(k) => k,
        Err(r) => return r,
    };
    let result = match st.providers.resolve(k, &input) {
        Ok(Resolved::HomeAssistant(h)) => remote::test_home_assistant(&h).await,
        Ok(Resolved::Prometheus(u)) => remote::test_prometheus(&u).await,
        Ok(Resolved::Mqtt(m)) => remote::test_mqtt(&m).await,
        Err(e) => Err(e),
    };
    Json(match result {
        Ok(msg) => json!({ "ok": true, "message": msg }),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }),
    })
    .into_response()
}

/// HomeKit pairing state: setup code, paired controllers and which of them
/// are connected right now.
async fn pairing(State(st): State<AppState>) -> Json<serde_json::Value> {
    let hub = &st.hub;
    let conns = hub.connections();
    let since = |c: &crate::hub::LiveConn| c.since.elapsed().as_secs();
    let pairings: Vec<_> = hub
        .pairings()
        .iter()
        .map(|p| {
            let live: Vec<_> = conns
                .iter()
                .filter(|c| c.controller.as_deref() == Some(p.id.as_str()))
                .map(|c| json!({ "ip": c.peer.to_string(), "connected_secs": since(c) }))
                .collect();
            json!({ "id": p.id, "admin": p.admin, "connections": live })
        })
        .collect();
    Json(json!({
        "paired": hub.is_paired(),
        "device_id": hub.device_id(),
        "setup_code": hub.setup_code(),
        "setup_uri": hub.setup_uri(),
        "pairings": pairings,
        "unverified_connections": conns.iter().filter(|c| c.controller.is_none()).count(),
    }))
}

async fn reset_pairing(State(st): State<AppState>) -> Response {
    match st.hub.reset_pairing() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn pairing_qr(State(st): State<AppState>) -> Response {
    match qrcode::QrCode::new(st.hub.setup_uri().as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(200, 200)
                .quiet_zone(true)
                .build();
            ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "no-store")], svg).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

fn widget_json(hub: &Hub, w: &crate::widgets::WidgetDef, latest: Option<f64>) -> serde_json::Value {
    let mut v = serde_json::to_value(w).unwrap();
    v["value"] = json!(latest);
    v["status"] = json!(hub.source_status(&w.id));
    v
}

async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    let latest = st.hub.latest_values();
    let widgets: Vec<_> =
        st.hub.widgets().iter().map(|w| widget_json(&st.hub, w, latest.get(&w.id).copied())).collect();
    Json(json!(widgets))
}

async fn get_one(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.hub.widgets().iter().find(|w| w.id == id) {
        Some(w) => Json(widget_json(&st.hub, w, st.hub.latest_values().get(&id).copied())).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such widget"),
    }
}

/// Catches obviously broken sources before they become a camera.
fn validate(req: &NewWidget) -> Result<(), String> {
    if req.name.trim().is_empty() {
        return Err("name is required".into());
    }
    let empty = |s: &str| s.trim().is_empty();
    match &req.source {
        Source::HomeAssistant { entity_id, .. } if !entity_id.contains('.') => {
            Err("entity_id must look like sensor.living_room_temperature".into())
        }
        Source::Prometheus { query } if empty(query) => Err("query is required".into()),
        Source::Mqtt { topic, .. } if empty(topic) || !rumqttc::valid_filter(topic) => {
            Err("topic must be a valid MQTT topic filter".into())
        }
        _ => Ok(()),
    }
}

async fn create(State(st): State<AppState>, Json(req): Json<NewWidget>) -> Response {
    if let Err(e) = validate(&req) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    match st.hub.add_widget(req) {
        Ok(def) => (StatusCode::CREATED, Json(def)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn update(State(st): State<AppState>, Path(id): Path<String>, Json(req): Json<NewWidget>) -> Response {
    if let Err(e) = validate(&req) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    match st.hub.update_widget(&id, req) {
        Ok(Some(def)) => Json(def).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such widget"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn remove(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.hub.remove_widget(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such widget"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct PushValue {
    value: f64,
}

async fn push(State(st): State<AppState>, Path(id): Path<String>, Json(body): Json<PushValue>) -> Response {
    if st.hub.push_value(&id, body.value) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        err(StatusCode::NOT_FOUND, "no such widget (or value not finite)")
    }
}

#[derive(Deserialize)]
struct Size {
    width: Option<u32>,
    height: Option<u32>,
}

async fn preview(State(st): State<AppState>, Path(id): Path<String>, Query(size): Query<Size>) -> Response {
    let Some(snap) = st.hub.snapshot(&id) else {
        return err(StatusCode::NOT_FOUND, "no such widget");
    };
    let (w, h) = (size.width.unwrap_or(640).clamp(16, 3840), size.height.unwrap_or(360).clamp(16, 2160));
    match tokio::task::spawn_blocking(move || render::render_png(&snap, w, h)).await {
        Ok(Ok(png)) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], png).into_response(),
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, "render failed"),
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}
