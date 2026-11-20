//! Accessory database: the bridge (aid 1) plus one camera accessory per widget.

use base64::Engine;
use serde_json::{json, Value};

use crate::camera;
use crate::hub::Hub;

pub mod status {
    pub const READ_ONLY: i64 = -70404;
    pub const WRITE_ONLY: i64 = -70405;
    pub const NOT_FOUND: i64 = -70409;
    pub const INVALID_VALUE: i64 = -70410;
}

pub const BRIDGE_AID: u64 = 1;

// Accessory Information service (both bridge and cameras)
const IID_IDENTIFY: u64 = 2;
const IID_MANUFACTURER: u64 = 3;
const IID_MODEL: u64 = 4;
const IID_NAME: u64 = 5;
const IID_SERIAL: u64 = 6;
const IID_FIRMWARE: u64 = 7;
// Bridge: Protocol Information service
const IID_PROTO_VERSION: u64 = 9;
// Cameras: Camera RTP Stream Management service
const IID_SUPPORTED_VIDEO: u64 = 9;
const IID_SUPPORTED_AUDIO: u64 = 10;
const IID_SUPPORTED_RTP: u64 = 11;
const IID_SELECTED_RTP: u64 = 12;
const IID_STREAMING_STATUS: u64 = 13;
const IID_SETUP_ENDPOINTS: u64 = 14;
const IID_ACTIVE: u64 = 15;
// Cameras: Microphone service (required by the HAP IP camera profile; we never send audio)
const IID_MIC_MUTE: u64 = 17;

/// Bump when the shape of the accessory database changes, so existing
/// controllers re-fetch `/accessories` (see `Hub::ensure_db_schema`).
pub const DB_SCHEMA: u32 = 2;

struct Char {
    iid: u64,
    ty: &'static str,
    perms: &'static [&'static str],
    format: &'static str,
}

const fn c(iid: u64, ty: &'static str, perms: &'static [&'static str], format: &'static str) -> Char {
    Char { iid, ty, perms, format }
}

const INFO: [Char; 6] = [
    c(IID_IDENTIFY, "14", &["pw"], "bool"),
    c(IID_MANUFACTURER, "20", &["pr"], "string"),
    c(IID_MODEL, "21", &["pr"], "string"),
    c(IID_NAME, "23", &["pr"], "string"),
    c(IID_SERIAL, "30", &["pr"], "string"),
    c(IID_FIRMWARE, "52", &["pr"], "string"),
];

const PROTOCOL: [Char; 1] = [c(IID_PROTO_VERSION, "37", &["pr"], "string")];

const CAMERA_RTP: [Char; 7] = [
    c(IID_SUPPORTED_VIDEO, "114", &["pr"], "tlv8"),
    c(IID_SUPPORTED_AUDIO, "115", &["pr"], "tlv8"),
    c(IID_SUPPORTED_RTP, "116", &["pr"], "tlv8"),
    c(IID_SELECTED_RTP, "117", &["pr", "pw"], "tlv8"),
    c(IID_STREAMING_STATUS, "120", &["pr", "ev"], "tlv8"),
    c(IID_SETUP_ENDPOINTS, "118", &["pr", "pw"], "tlv8"),
    c(IID_ACTIVE, "B0", &["pr", "pw", "ev"], "uint8"),
];

const MICROPHONE: [Char; 1] = [c(IID_MIC_MUTE, "11A", &["pr", "pw", "ev"], "bool")];

fn services(aid: u64) -> Vec<(u64, &'static str, &'static [Char])> {
    if aid == BRIDGE_AID {
        vec![(1, "3E", &INFO), (8, "A2", &PROTOCOL)]
    } else {
        vec![(1, "3E", &INFO), (8, "110", &CAMERA_RTP), (16, "112", &MICROPHONE)]
    }
}

fn find_char(aid: u64, iid: u64) -> Option<&'static Char> {
    services(aid).into_iter().flat_map(|(_, _, chars)| chars.iter()).find(|c| c.iid == iid)
}

fn exists(hub: &Hub, aid: u64) -> bool {
    aid == BRIDGE_AID || hub.widget_by_aid(aid).is_some()
}

fn b64(v: &[u8]) -> Value {
    Value::String(base64::engine::general_purpose::STANDARD.encode(v))
}

pub fn accessories_json(hub: &Hub) -> Value {
    let aids = std::iter::once(BRIDGE_AID).chain(hub.widgets().into_iter().map(|w| w.aid));
    let accessories: Vec<Value> = aids
        .map(|aid| {
            let services: Vec<Value> = services(aid)
                .into_iter()
                .map(|(iid, ty, chars)| {
                    let chars: Vec<Value> = chars
                        .iter()
                        .map(|ch| {
                            let mut o = json!({"iid": ch.iid, "type": ch.ty, "perms": ch.perms, "format": ch.format});
                            if ch.format == "uint8" {
                                o["minValue"] = json!(0);
                                o["maxValue"] = json!(1);
                                o["minStep"] = json!(1);
                            }
                            if ch.perms.contains(&"pr") {
                                if let Ok(v) = read(hub, aid, ch.iid) {
                                    o["value"] = v;
                                }
                            }
                            o
                        })
                        .collect();
                    json!({"iid": iid, "type": ty, "characteristics": chars})
                })
                .collect();
            json!({"aid": aid, "services": services})
        })
        .collect();
    json!({ "accessories": accessories })
}

pub fn read(hub: &Hub, aid: u64, iid: u64) -> Result<Value, i64> {
    let ch = find_char(aid, iid).filter(|_| exists(hub, aid)).ok_or(status::NOT_FOUND)?;
    if !ch.perms.contains(&"pr") {
        return Err(status::WRITE_ONLY);
    }
    let widget = hub.widget_by_aid(aid);
    let v = match iid {
        IID_MANUFACTURER => json!("homestat"),
        IID_MODEL => json!(if aid == BRIDGE_AID { "Widget Bridge" } else { "Widget Camera" }),
        IID_NAME => json!(widget.map(|w| w.name).unwrap_or_else(|| hub.name.clone())),
        IID_SERIAL => json!(widget.map(|w| w.id).unwrap_or_else(|| hub.device_id())),
        IID_FIRMWARE => json!(env!("CARGO_PKG_VERSION")),
        IID_PROTO_VERSION if aid == BRIDGE_AID => json!("1.1.0"),
        IID_SUPPORTED_VIDEO => b64(&camera::supported_video_config()),
        IID_SUPPORTED_AUDIO => b64(&camera::supported_audio_config()),
        IID_SUPPORTED_RTP => b64(&camera::supported_rtp_config()),
        IID_SELECTED_RTP => b64(&camera::with_state(hub, aid, |c| c.selected.clone())),
        IID_STREAMING_STATUS => b64(&camera::streaming_status(hub, aid)),
        IID_SETUP_ENDPOINTS => b64(&camera::with_state(hub, aid, |c| c.setup_response.clone())),
        IID_ACTIVE => json!(camera::with_state(hub, aid, |c| c.active)),
        IID_MIC_MUTE => json!(false),
        _ => return Err(status::NOT_FOUND),
    };
    Ok(v)
}

/// Returns `Some(value)` when the write produced a write-response value.
pub fn write(hub: &std::sync::Arc<Hub>, link: camera::Link, aid: u64, iid: u64, value: &Value) -> Result<Option<Value>, i64> {
    let ch = find_char(aid, iid).filter(|_| exists(hub, aid)).ok_or(status::NOT_FOUND)?;
    if !ch.perms.contains(&"pw") {
        return Err(status::READ_ONLY);
    }
    let tlv = || {
        value
            .as_str()
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .ok_or(status::INVALID_VALUE)
    };
    match iid {
        IID_IDENTIFY => {
            tracing::info!(aid, "identify requested");
            Ok(None)
        }
        IID_SETUP_ENDPOINTS => {
            let resp = camera::setup_endpoints(hub, aid, &tlv()?, link).map_err(|e| {
                tracing::warn!("setup endpoints failed: {e:#}");
                status::INVALID_VALUE
            })?;
            Ok(Some(b64(&resp)))
        }
        IID_SELECTED_RTP => {
            camera::select_stream_config(hub, aid, &tlv()?).map_err(|e| {
                tracing::warn!("selected stream config failed: {e:#}");
                status::INVALID_VALUE
            })?;
            Ok(None)
        }
        IID_ACTIVE => {
            let v = value.as_u64().or_else(|| value.as_bool().map(u64::from)).ok_or(status::INVALID_VALUE)?;
            camera::with_state(hub, aid, |c| c.active = (v != 0) as u8);
            Ok(None)
        }
        IID_MIC_MUTE => Ok(None),
        _ => Err(status::READ_ONLY),
    }
}
