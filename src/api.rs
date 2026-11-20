//! Admin HTTP API for managing widgets (separate from the HAP port).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::hub::Hub;
use crate::widgets::{render, NewWidget};

pub async fn serve(hub: Arc<Hub>, port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/pairing", get(pairing))
        .route("/api/widgets", get(list).post(create))
        .route("/api/widgets/{id}", delete(remove))
        .route("/api/widgets/{id}/value", post(push))
        .route("/api/widgets/{id}/preview.png", get(preview))
        .with_state(hub);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("admin API on http://localhost:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

async fn pairing(State(hub): State<Arc<Hub>>) -> Json<serde_json::Value> {
    Json(json!({
        "paired": hub.is_paired(),
        "setup_code": hub.setup_code(),
        "setup_uri": hub.setup_uri(),
    }))
}

async fn list(State(hub): State<Arc<Hub>>) -> Json<serde_json::Value> {
    let latest = hub.latest_values();
    let widgets: Vec<_> = hub
        .widgets()
        .into_iter()
        .map(|w| {
            let mut v = serde_json::to_value(&w).unwrap();
            v["value"] = json!(latest.get(&w.id));
            v
        })
        .collect();
    Json(json!(widgets))
}

async fn create(State(hub): State<Arc<Hub>>, Json(req): Json<NewWidget>) -> Response {
    match hub.add_widget(req) {
        Ok(def) => (StatusCode::CREATED, Json(def)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn remove(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.remove_widget(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such widget"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct PushValue {
    value: f64,
}

async fn push(State(hub): State<Arc<Hub>>, Path(id): Path<String>, Json(body): Json<PushValue>) -> Response {
    if hub.push_value(&id, body.value) {
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

async fn preview(State(hub): State<Arc<Hub>>, Path(id): Path<String>, Query(size): Query<Size>) -> Response {
    let Some(snap) = hub.snapshot(&id) else {
        return err(StatusCode::NOT_FOUND, "no such widget");
    };
    let (w, h) = (size.width.unwrap_or(640).clamp(16, 3840), size.height.unwrap_or(360).clamp(16, 2160));
    match tokio::task::spawn_blocking(move || render::render_png(&snap, w, h)).await {
        Ok(Ok(png)) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], png).into_response(),
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, "render failed"),
    }
}

async fn index(State(hub): State<Arc<Hub>>) -> Html<String> {
    let tiles: String = hub
        .widgets()
        .iter()
        .map(|w| {
            format!(
                r#"<figure><img data-src="/api/widgets/{id}/preview.png?width=480&height=270" width="480" height="270"><figcaption>{name} &middot; aid {aid} &middot; <code>{id}</code></figcaption></figure>"#,
                id = w.id,
                name = html_escape(&w.name),
                aid = w.aid
            )
        })
        .collect();
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>homestat</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>body{{background:#111;color:#ccc;font:14px system-ui;margin:16px}}main{{display:flex;flex-wrap:wrap;gap:16px}}
figure{{margin:0}}img{{max-width:100%;height:auto;display:block;border-radius:6px}}figcaption{{margin-top:4px;color:#888}}</style></head>
<body><h1>homestat</h1><p>Setup code <b>{code}</b> &middot; {paired}</p><main>{tiles}</main>
<script>function r(){{document.querySelectorAll('img').forEach(i=>i.src=i.dataset.src+'&t='+Date.now())}}r();setInterval(r,2000)</script>
</body></html>"#,
        code = hub.setup_code(),
        paired = if hub.is_paired() { "paired" } else { "not paired yet" },
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
