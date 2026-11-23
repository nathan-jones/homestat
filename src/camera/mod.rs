//! HomeKit camera stream negotiation (Camera RTP Stream Management service).
//!
//! Flow: controller writes SetupEndpoints (its address + SRTP keys), reads the
//! response (our address/ports/SSRC), then writes SelectedRTPStreamConfiguration
//! with command=start and the chosen resolution/fps/bitrate.

pub mod rtp;
pub mod srtp;
pub mod stream;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngExt;

use crate::hap::tlv::{Map, Writer};
use crate::hub::Hub;
use stream::{StreamHandle, VideoParams};

/// Resolutions offered to HomeKit (width, height, fps).
const RESOLUTIONS: &[(u16, u16, u8)] = &[
    (1920, 1080, 30),
    (1280, 960, 30),
    (1280, 720, 30),
    (1024, 768, 30),
    (640, 480, 30),
    (640, 360, 30),
    (480, 360, 30),
    (480, 270, 30),
    (320, 240, 30),
    (320, 240, 15), // Apple Watch
    (320, 180, 30),
];

const CRYPTO_AES_CM_128: u8 = 0;
const CRYPTO_NONE: u8 = 2;

pub struct Pending {
    pub dest: SocketAddr,
    pub crypto_suite: u8,
    pub srtp_key: Vec<u8>,
    pub srtp_salt: Vec<u8>,
    pub ssrc: u32,
    pub video_socket: UdpSocket,
    _audio_socket: UdpSocket,
}

pub struct CameraState {
    pub active: u8,
    pub selected: Vec<u8>,
    pub setup_response: Vec<u8>,
    pending: HashMap<Vec<u8>, Pending>,
    streams: HashMap<Vec<u8>, StreamHandle>,
}

impl Default for CameraState {
    fn default() -> Self {
        CameraState {
            active: 1,
            selected: vec![],
            setup_response: vec![],
            pending: HashMap::new(),
            streams: HashMap::new(),
        }
    }
}

impl CameraState {
    pub fn stop_all(self) {
        for (_, s) in self.streams {
            s.stop();
        }
    }
}

pub fn with_state<T>(hub: &Hub, aid: u64, f: impl FnOnce(&mut CameraState) -> T) -> T {
    let mut cams = hub.cameras.lock().unwrap();
    let state = cams.entry(aid).or_default();
    // Opportunistically drop streams whose thread has ended.
    state.streams.retain(|_, s| !s.is_finished());
    f(state)
}

pub fn supported_video_config() -> Vec<u8> {
    let params = Writer::new()
        .u8(1, 0) // profile: constrained baseline (what openh264 produces)
        .u8(2, 0) // level 3.1
        .delimiter(0)
        .u8(2, 1) // level 3.2
        .delimiter(0)
        .u8(2, 2) // level 4.0
        .u8(3, 0) // packetization mode: non-interleaved
        .build();
    let mut codec = Writer::new().u8(1, 0).bytes(2, &params); // codec: H.264
    for (i, (w, h, fps)) in RESOLUTIONS.iter().enumerate() {
        if i > 0 {
            codec = codec.delimiter(0);
        }
        let attr = Writer::new().u16(1, *w).u16(2, *h).u8(3, *fps).build();
        codec = codec.bytes(3, &attr);
    }
    Writer::new().bytes(1, &codec.build()).build()
}

pub fn supported_audio_config() -> Vec<u8> {
    // We never send audio, but HomeKit expects a configuration. Opus mono 24kHz.
    let params = Writer::new().u8(1, 1).u8(2, 0).u8(3, 2).build();
    let codec = Writer::new().u8(1, 3).bytes(2, &params).build();
    Writer::new().bytes(1, &codec).u8(2, 0).build()
}

pub fn supported_rtp_config() -> Vec<u8> {
    Writer::new().u8(2, CRYPTO_AES_CM_128).build()
}

pub fn streaming_status(hub: &Hub, aid: u64) -> Vec<u8> {
    // 0 = available. Multiple concurrent sessions are supported, so never "in use".
    let _ = (hub, aid);
    Writer::new().u8(1, 0).build()
}

/// The HAP connection a stream is being negotiated over.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// Our address on that connection (IPv4-mapped addresses unmapped).
    pub local_ip: IpAddr,
    /// IPv6 scope of the controller's address, needed to reach link-local peers.
    pub scope_id: u32,
}

pub fn setup_endpoints(hub: &Hub, aid: u64, data: &[u8], link: Link) -> Result<Vec<u8>> {
    let local_ip = link.local_ip;
    tracing::debug!(tlv = %hex::encode(data), "SetupEndpoints request");
    let req = Map::parse(data);
    let session_id = req.get(1).context("missing session id")?.to_vec();
    let addr = req.sub(3).context("missing controller address")?;
    let ip_str = std::str::from_utf8(addr.get(2).context("missing ip")?)?;
    let ip: IpAddr = ip_str.split('%').next().unwrap_or(ip_str).parse()?;
    let video_port = addr.u16(3).context("missing video port")?;
    let srtp = req.sub(4).context("missing video SRTP params")?;
    let suite = srtp.u8(1).unwrap_or(CRYPTO_NONE);
    let key = srtp.get(2).unwrap_or_default().to_vec();
    let salt = srtp.get(3).unwrap_or_default().to_vec();
    let audio_srtp = req.get(5).unwrap_or_default().to_vec();
    if suite != CRYPTO_AES_CM_128 && suite != CRYPTO_NONE {
        bail!("unsupported SRTP crypto suite {suite}");
    }

    let bind_ip: IpAddr = if ip.is_ipv4() { [0, 0, 0, 0].into() } else { [0u16; 8].into() };
    let video_socket = UdpSocket::bind((bind_ip, 0))?;
    let audio_socket = UdpSocket::bind((bind_ip, 0))?;
    let local_video_port = video_socket.local_addr()?.port();
    let local_audio_port = audio_socket.local_addr()?.port();
    let ssrc: u32 = rand::rng().random();

    let our_addr = Writer::new()
        .u8(1, local_ip.is_ipv6() as u8)
        .bytes(2, local_ip.to_string().as_bytes())
        .u16(3, local_video_port)
        .u16(4, local_audio_port)
        .build();
    // HomeKit lets the accessory echo the controller's SRTP key for its own stream.
    let our_srtp = Writer::new().u8(1, suite).bytes(2, &key).bytes(3, &salt).build();
    let response = Writer::new()
        .bytes(1, &session_id)
        .u8(2, 0) // status: success
        .bytes(3, &our_addr)
        .bytes(4, &our_srtp)
        .bytes(5, &audio_srtp)
        .u32(6, ssrc)
        .u32(7, rand::rng().random())
        .build();

    tracing::info!(aid, controller = %ip, video_port, suite, local = %local_ip, local_video_port, "stream endpoints prepared");
    tracing::debug!(tlv = %hex::encode(&response), "SetupEndpoints response");
    let pending = Pending {
        dest: match ip {
            IpAddr::V6(v6) if v6.is_unicast_link_local() => {
                SocketAddr::V6(std::net::SocketAddrV6::new(v6, video_port, 0, link.scope_id))
            }
            _ => SocketAddr::new(ip, video_port),
        },
        crypto_suite: suite,
        srtp_key: key,
        srtp_salt: salt,
        ssrc,
        video_socket,
        _audio_socket: audio_socket,
    };
    with_state(hub, aid, |c| {
        c.pending.insert(session_id, pending);
        c.setup_response = response.clone();
    });
    Ok(response)
}

fn parse_video_params(video: &Map) -> Result<VideoParams> {
    let attrs = video.sub(3).ok_or_else(|| anyhow!("missing video attributes"))?;
    let rtp = video.sub(4).ok_or_else(|| anyhow!("missing RTP params"))?;
    let rtcp_interval = rtp
        .get(4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(f32::from_le_bytes)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.5);
    Ok(VideoParams {
        width: attrs.u16(1).unwrap_or(1280) as u32,
        height: attrs.u16(2).unwrap_or(720) as u32,
        fps: attrs.u8(3).unwrap_or(30).clamp(1, 30) as u32,
        max_bitrate_kbps: rtp.u16(3).unwrap_or(299).max(50) as u32,
        payload_type: rtp.u8(1).unwrap_or(99),
        mtu: rtp.u16(5).unwrap_or(1378) as usize,
        rtcp_interval,
    })
}

pub fn select_stream_config(hub: &std::sync::Arc<Hub>, aid: u64, data: &[u8]) -> Result<()> {
    tracing::debug!(tlv = %hex::encode(data), "SelectedRTPStreamConfiguration write");
    let req = Map::parse(data);
    let control = req.sub(1).context("missing session control")?;
    let session_id = control.get(1).context("missing session id")?.to_vec();
    let command = control.u8(2).context("missing command")?;
    tracing::info!(aid, command, "stream control");
    with_state(hub, aid, |c| c.selected = data.to_vec());

    match command {
        // End
        0 => {
            let handle = with_state(hub, aid, |c| {
                c.pending.remove(&session_id);
                c.streams.remove(&session_id)
            });
            if let Some(h) = handle {
                tracing::info!(aid, "stream end requested");
                h.stop();
            }
        }
        // Start
        1 => {
            let params = parse_video_params(&req.sub(2).context("missing video params")?)?;
            let pending =
                with_state(hub, aid, |c| c.pending.remove(&session_id)).context("unknown stream session")?;
            let widget = hub.widget_by_aid(aid).context("camera has no widget")?;
            tracing::info!(
                aid, widget = %widget.id,
                "starting stream {}x{}@{} {}kbps",
                params.width, params.height, params.fps, params.max_bitrate_kbps
            );
            let handle = stream::spawn(hub.clone(), widget.id, pending, params)?;
            with_state(hub, aid, |c| c.streams.insert(session_id, handle));
        }
        // Suspend / resume: treated as no-ops for a synthetic feed.
        2 | 3 => {}
        // Reconfigure
        4 => {
            let params = parse_video_params(&req.sub(2).context("missing video params")?)?;
            with_state(hub, aid, |c| {
                if let Some(h) = c.streams.get(&session_id) {
                    tracing::info!(aid, "reconfigure to {}x{}@{}", params.width, params.height, params.fps);
                    h.reconfigure(params);
                }
            });
        }
        other => bail!("unknown stream command {other}"),
    }
    Ok(())
}
