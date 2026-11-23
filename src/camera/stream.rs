//! The streaming loop: render widget -> H.264 (openh264) -> RTP -> SRTP -> UDP.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, Profile, RateControlMode, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use openh264::OpenH264API;
use rand::RngExt;

use super::rtp::{split_annexb, RtpPacketizer};
use super::srtp::Srtp;
use super::Pending;
use crate::hub::Hub;
use crate::widgets::render;

/// Give up when the controller stops sending RTCP for this long.
const RTCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Force a keyframe this often so late joiners / packet loss recover.
const KEYFRAME_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate_kbps: u32,
    pub payload_type: u8,
    pub mtu: usize,
    pub rtcp_interval: f32,
}

pub struct StreamHandle {
    stop: Arc<AtomicBool>,
    reconfig: Arc<Mutex<Option<VideoParams>>>,
    thread: JoinHandle<()>,
}

impl StreamHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn reconfigure(&self, p: VideoParams) {
        *self.reconfig.lock().unwrap() = Some(p);
    }
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

pub fn spawn(hub: Arc<Hub>, widget_id: String, pending: Pending, params: VideoParams) -> Result<StreamHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let reconfig = Arc::new(Mutex::new(None));
    let (s, r) = (stop.clone(), reconfig.clone());
    let thread = std::thread::Builder::new().name(format!("stream-{widget_id}")).spawn(move || {
        match run(&hub, &widget_id, pending, params, &s, &r) {
            Ok(reason) => tracing::info!(widget = %widget_id, "stream ended: {reason}"),
            Err(e) => tracing::error!(widget = %widget_id, "stream failed: {e:#}"),
        }
    })?;
    Ok(StreamHandle { stop, reconfig, thread })
}

fn make_encoder(p: &VideoParams) -> Result<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(p.max_bitrate_kbps * 1000))
        .max_frame_rate(FrameRate::from_hz(p.fps as f32))
        .usage_type(UsageType::CameraVideoRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .profile(Profile::Baseline)
        .skip_frames(false);
    Ok(Encoder::with_api_config(OpenH264API::from_source(), config)?)
}

fn run(
    hub: &Hub,
    widget_id: &str,
    pending: Pending,
    mut params: VideoParams,
    stop: &AtomicBool,
    reconfig: &Mutex<Option<VideoParams>>,
) -> Result<&'static str> {
    let sock = pending.video_socket;
    sock.set_nonblocking(true)?;
    let dest = pending.dest;
    let mut srtp = match pending.crypto_suite {
        0 => Some(Srtp::new(&pending.srtp_key, &pending.srtp_salt)?),
        _ => None,
    };
    let mut rng = rand::rng();
    let mut rtp = RtpPacketizer::new(pending.ssrc, params.payload_type, params.mtu, rng.random());
    let ts_base: u32 = rng.random();
    let mut encoder = make_encoder(&params)?;

    let start = Instant::now();
    let mut last_rx = Instant::now();
    let mut last_rtcp = Instant::now() - Duration::from_secs(10);
    let mut last_keyframe = Instant::now();
    let mut rendered: Option<(u64, YUVBuffer)> = None;
    let mut next_frame = Instant::now();
    let mut rx = [0u8; 2048];
    let mut frames: u64 = 0;
    let mut stats = Stats::default();
    let mut last_stats = Instant::now();
    tracing::info!(%dest, local = ?sock.local_addr().ok(), "stream sending");

    loop {
        if stop.load(Ordering::SeqCst) {
            stats.log(widget_id, frames);
            return Ok("stopped by controller");
        }
        if let Some(p) = reconfig.lock().unwrap().take() {
            encoder = make_encoder(&p)?;
            rtp = RtpPacketizer::new(rtp.ssrc, p.payload_type, p.mtu, rng.random());
            params = p;
            rendered = None;
        }

        // Drain incoming RTCP (receiver reports) purely as a liveness signal.
        loop {
            match sock.recv_from(&mut rx) {
                Ok((n, from)) => {
                    if stats.rtcp_rx == 0 {
                        tracing::info!(%from, bytes = n, "first RTCP from controller (it is receiving us)");
                    }
                    stats.rtcp_rx += 1;
                    last_rx = Instant::now();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if last_rx.elapsed() > RTCP_TIMEOUT {
            stats.log(widget_id, frames);
            return Ok("controller went silent");
        }

        // Re-render only when the widget's data changed.
        let Some(snap) = hub.snapshot(widget_id) else {
            return Ok("widget removed");
        };
        if rendered.as_ref().is_none_or(|(v, _)| *v != snap.version) {
            let pixmap = render::render(&snap, params.width, params.height);
            let rgba = RgbaSliceU8::new(pixmap.data(), (params.width as usize, params.height as usize));
            rendered = Some((snap.version, YUVBuffer::from_rgba8_source(rgba)));
        }

        if frames == 0 || last_keyframe.elapsed() >= KEYFRAME_EVERY {
            encoder.force_intra_frame();
            last_keyframe = Instant::now();
        }
        let yuv = &rendered.as_ref().unwrap().1;
        let t_enc = Instant::now();
        let annexb = encoder.encode(yuv)?.to_vec();
        stats.encode_us += t_enc.elapsed().as_micros() as u64;
        let ts = ts_base.wrapping_add((start.elapsed().as_secs_f64() * 90_000.0) as u32);
        let nals = split_annexb(&annexb);
        for (mut pkt, index) in rtp.packetize_au(&nals, ts) {
            if let Some(s) = srtp.as_ref() {
                s.protect_rtp(&mut pkt, index);
            }
            stats.sent(sock.send_to(&pkt, dest), pkt.len());
        }
        frames += 1;
        if last_stats.elapsed() >= Duration::from_secs(5) {
            stats.log(widget_id, frames);
            last_stats = Instant::now();
        }

        if last_rtcp.elapsed().as_secs_f32() >= params.rtcp_interval.max(0.5) {
            let mut sr = rtp.sender_report();
            if let Some(s) = srtp.as_mut() {
                s.protect_rtcp(&mut sr);
            }
            stats.sent(sock.send_to(&sr, dest), sr.len());
            last_rtcp = Instant::now();
        }

        next_frame += Duration::from_secs_f64(1.0 / params.fps as f64);
        let now = Instant::now();
        if next_frame > now {
            std::thread::sleep(next_frame - now);
        } else {
            next_frame = now; // fell behind; don't try to catch up with a burst
        }
    }
}

#[derive(Default)]
struct Stats {
    packets: u64,
    bytes: u64,
    send_errors: u64,
    last_error: Option<String>,
    rtcp_rx: u64,
    encode_us: u64,
}

impl Stats {
    fn sent(&mut self, r: std::io::Result<usize>, len: usize) {
        match r {
            Ok(_) => {
                self.packets += 1;
                self.bytes += len as u64;
            }
            Err(e) => {
                if self.send_errors == 0 {
                    tracing::error!("UDP send failed: {e}");
                }
                self.send_errors += 1;
                self.last_error = Some(e.to_string());
            }
        }
    }

    fn log(&self, widget: &str, frames: u64) {
        tracing::info!(
            widget,
            frames,
            packets = self.packets,
            kbytes = self.bytes / 1024,
            send_errors = self.send_errors,
            last_error = self.last_error.as_deref().unwrap_or("-"),
            rtcp_from_controller = self.rtcp_rx,
            avg_encode_ms = self.encode_us as f64 / frames.max(1) as f64 / 1000.0,
            "stream stats"
        );
    }
}
