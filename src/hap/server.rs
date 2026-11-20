//! Minimal HTTP/1.1 server speaking HAP over TCP (with session encryption
//! enabled after pair-verify).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::crypto::Session;
use super::db;
use super::pairing::{self, ConnState};
use crate::camera::Link;
use crate::hub::Hub;
use crate::widgets::render;

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, v: Value) -> Self {
        Response { status, content_type: "application/hap+json", body: v.to_string().into_bytes() }
    }
    fn tlv(body: Vec<u8>) -> Self {
        Response { status: 200, content_type: "application/pairing+tlv8", body }
    }
    fn empty(status: u16) -> Self {
        Response { status, content_type: "application/hap+json", body: vec![] }
    }
    fn to_bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            204 => "No Content",
            207 => "Multi-Status",
            400 => "Bad Request",
            404 => "Not Found",
            470 => "Connection Authorization Required",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, reason);
        if !self.body.is_empty() || self.status != 204 {
            out.push_str(&format!("Content-Type: {}\r\n", self.content_type));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

pub async fn serve(hub: Arc<Hub>, port: u16) -> Result<()> {
    // Dual-stack: controllers (iPhones especially) often connect over IPv6,
    // since mDNS advertises our IPv6 addresses too.
    let sock = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, None)?;
    sock.set_only_v6(false)?;
    sock.set_reuse_address(true)?;
    sock.bind(&SocketAddr::from(([0u16; 8], port)).into())?;
    sock.listen(128)?;
    sock.set_nonblocking(true)?;
    let listener = TcpListener::from_std(sock.into())?;
    tracing::info!("HAP server listening on [::]:{port} (IPv4 + IPv6)");
    loop {
        let (stream, peer) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            tracing::debug!(%peer, "controller connected");
            if let Err(e) = handle_conn(hub, stream).await {
                tracing::debug!(%peer, "connection closed: {e:#}");
            }
        });
    }
}

async fn handle_conn(hub: Arc<Hub>, mut stream: TcpStream) -> Result<()> {
    stream.set_nodelay(true)?;
    let scope_id = match stream.peer_addr()? {
        SocketAddr::V6(a) => a.scope_id(),
        SocketAddr::V4(_) => 0,
    };
    let link = Link { local_ip: stream.local_addr()?.ip().to_canonical(), scope_id };
    let mut conn = ConnState::default();
    let mut session: Option<Session> = None;
    let mut raw = Vec::new();
    let mut plain = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        match session.as_mut() {
            Some(s) => {
                raw.extend_from_slice(&buf[..n]);
                plain.extend(s.decrypt(&mut raw)?);
            }
            None => plain.extend_from_slice(&buf[..n]),
        }
        while let Some((req, used)) = parse_request(&plain)? {
            plain.drain(..used);
            tracing::debug!(method = %req.method, path = %req.path, query = %req.query, "request");
            if req.path == "/characteristics" && req.method == "PUT" {
                tracing::debug!(body = %String::from_utf8_lossy(&req.body), "characteristics write");
            }
            let (resp, upgrade) = route(&hub, &mut conn, req, link, session.is_some()).await;
            tracing::debug!(status = resp.status, len = resp.body.len(), "response");
            if resp.status == 207 || resp.status >= 400 {
                tracing::debug!(body = %String::from_utf8_lossy(&resp.body), "non-2xx/207 body");
            }
            let bytes = resp.to_bytes();
            match session.as_mut() {
                Some(s) => stream.write_all(&s.encrypt(&bytes)).await?,
                None => stream.write_all(&bytes).await?,
            }
            if let Some(mut s) = upgrade {
                // Anything left over was already sent encrypted by the controller.
                raw.append(&mut plain);
                plain = s.decrypt(&mut raw)?;
                session = Some(s);
            }
        }
    }
}

fn parse_request(data: &[u8]) -> Result<Option<(Request, usize)>> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let header_len = match req.parse(data)? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => return Ok(None),
    };
    let content_len = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok()?.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if data.len() < header_len + content_len {
        return Ok(None);
    }
    let full_path = req.path.unwrap_or("/");
    let (path, query) = full_path.split_once('?').unwrap_or((full_path, ""));
    Ok(Some((
        Request {
            method: req.method.unwrap_or("GET").to_string(),
            path: path.to_string(),
            query: query.to_string(),
            body: data[header_len..header_len + content_len].to_vec(),
        },
        header_len + content_len,
    )))
}

async fn route(
    hub: &Arc<Hub>,
    conn: &mut ConnState,
    req: Request,
    link: Link,
    encrypted: bool,
) -> (Response, Option<Session>) {
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/pair-setup") => return (Response::tlv(pairing::pair_setup(hub, conn, &req.body)), None),
        ("POST", "/pair-verify") => {
            let (body, session) = pairing::pair_verify(hub, conn, &req.body);
            return (Response::tlv(body), session);
        }
        ("POST", "/identify") => {
            return if hub.is_paired() {
                (Response::json(400, json!({"status": -70401})), None)
            } else {
                tracing::info!("identify (unpaired)");
                (Response::empty(204), None)
            };
        }
        _ => {}
    }
    if !encrypted || conn.controller.is_none() {
        return (Response::json(470, json!({"status": -70401})), None);
    }
    let resp = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/accessories") => Response::json(200, db::accessories_json(hub)),
        ("GET", "/characteristics") => get_characteristics(hub, &req.query),
        ("PUT", "/characteristics") => put_characteristics(hub, &req.body, link),
        ("PUT", "/prepare") => Response::json(200, json!({"status": 0})),
        ("POST", "/pairings") => Response::tlv(pairing::pairings(hub, conn, &req.body)),
        ("POST", "/resource") => snapshot(hub, &req.body).await,
        _ => Response::empty(404),
    };
    (resp, None)
}

fn get_characteristics(hub: &Hub, query: &str) -> Response {
    let ids = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("id="))
        .unwrap_or("")
        .split(',')
        .filter_map(|pair| {
            let (a, i) = pair.split_once('.')?;
            Some((a.parse::<u64>().ok()?, i.parse::<u64>().ok()?))
        });
    let mut any_err = false;
    let results: Vec<(u64, u64, Result<Value, i64>)> =
        ids.map(|(aid, iid)| (aid, iid, db::read(hub, aid, iid))).collect();
    let chars: Vec<Value> = results
        .into_iter()
        .map(|(aid, iid, r)| match r {
            Ok(v) => json!({"aid": aid, "iid": iid, "value": v}),
            Err(code) => {
                any_err = true;
                json!({"aid": aid, "iid": iid, "status": code})
            }
        })
        .collect();
    let chars: Vec<Value> = if any_err {
        chars
            .into_iter()
            .map(|mut c| {
                c.as_object_mut().unwrap().entry("status").or_insert(json!(0));
                c
            })
            .collect()
    } else {
        chars
    };
    Response::json(if any_err { 207 } else { 200 }, json!({ "characteristics": chars }))
}

fn put_characteristics(hub: &Arc<Hub>, body: &[u8], link: Link) -> Response {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"status": -70410}));
    };
    let mut results = Vec::new();
    let mut multi = false;
    for c in req["characteristics"].as_array().cloned().unwrap_or_default() {
        let (Some(aid), Some(iid)) = (c["aid"].as_u64(), c["iid"].as_u64()) else { continue };
        let mut status = 0i64;
        let mut value = None;
        if let Some(ev) = c.get("ev") {
            // Event subscriptions are accepted but not delivered in this PoC.
            tracing::trace!(aid, iid, ?ev, "event subscription");
        }
        if let Some(v) = c.get("value") {
            match db::write(hub, link, aid, iid, v) {
                Ok(resp) => value = resp,
                Err(code) => status = code,
            }
        }
        let wants_response = c["r"].as_bool().unwrap_or(false);
        let mut out = json!({"aid": aid, "iid": iid, "status": status});
        if wants_response {
            if let Some(v) = value {
                out["value"] = v;
            }
            multi = true;
        }
        multi |= status != 0;
        results.push(out);
    }
    if multi {
        Response::json(207, json!({ "characteristics": results }))
    } else {
        Response::empty(204)
    }
}

async fn snapshot(hub: &Arc<Hub>, body: &[u8]) -> Response {
    let req: Value = serde_json::from_slice(body).unwrap_or_default();
    let width = req["image-width"].as_u64().unwrap_or(640).clamp(16, 3840) as u32;
    let height = req["image-height"].as_u64().unwrap_or(360).clamp(16, 2160) as u32;
    let widget = match req["aid"].as_u64() {
        Some(aid) => hub.widget_by_aid(aid),
        None => hub.widgets().into_iter().next(),
    };
    let Some(snap) = widget.and_then(|w| hub.snapshot(&w.id)) else {
        return Response::empty(404);
    };
    tracing::info!(aid = ?req["aid"], width, height, "snapshot requested");
    let jpeg = tokio::task::spawn_blocking(move || render::render_jpeg(&snap, width, height)).await;
    match jpeg {
        Ok(Ok(bytes)) => Response { status: 200, content_type: "image/jpeg", body: bytes },
        _ => Response::empty(500),
    }
}
