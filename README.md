# homestat

Proof of concept: a Rust HomeKit **bridge** that exposes dynamically rendered
stat widgets as **cameras** in the Apple Home app.

```
                 ┌──────────────────────────── homestat ─────────────────────────────┐
 Home app ◄─HAP─►│ HAP server (pair-setup/verify, /accessories, /characteristics,     │
 (iPhone/        │             /resource snapshots)          ▲                         │
  Apple TV)      │                                           │ c# bump + mDNS re-announce
     ▲           │ Hub: widgets ── aid 2..n (bridged cameras)┘ on add/remove          │
     │           │   │                                                                │
     └─SRTP/UDP──│ stream thread: render (tiny-skia) → H.264 (openh264) → RTP → SRTP  │
                 │   ▲                                                                │
                 │ samplers (cpu/mem/processes/demo)   admin API :8080 (push values)  │
                 └────────────────────────────────────────────────────────────────────┘
```

* **One pairing, many cameras.** The server is a HAP bridge (aid 1); every widget
  is a bridged camera accessory with a stable aid. Adding or removing a widget
  bumps the HAP configuration number (`c#`) and re-announces over mDNS, so Home
  picks up the change without re-pairing. (HomeKit caps a bridge at 150 accessories.)
* **Self-contained.** Everything from SRP pairing to H.264 encoding and SRTP is in
  the binary. No ffmpeg or Homebridge needed. openh264 is compiled from source by
  its crate.
* **Styles are pluggable.** `widgets::Style` currently has one variant, `Stat`
  (title, big value, sparkline, modelled on Grafana's singlestat). A new style is
  a new enum variant plus a render function in `widgets/render.rs`. Rendering is
  proportional, so it works at any resolution Home asks for.

## Run

```sh
cargo run --release -- --setup-code 031-45-154     # omit for a random code
```

On first run it creates `./data/state.json` (keys, pairings, widgets) with three
widgets: Processes, CPU, Memory. It prints a QR code. In the Home app go to
**Add Accessory**, then scan it or enter the code (the accessory is uncertified,
so confirm *Add Anyway*).

Flags: `--name`, `--port` (HAP, 51826), `--api-port` (8080), `--data-dir`.
To unpair or reset, stop the server and delete `data/`.

## Admin API

Open `http://localhost:8080/` to see live previews of every widget.

```sh
# list widgets (with latest values)
curl localhost:8080/api/widgets

# add a widget fed by pushes
curl -X POST localhost:8080/api/widgets -H 'content-type: application/json' \
  -d '{"name": "Solar", "unit": "kW", "source": {"type": "push"}}'

# push values
curl -X POST localhost:8080/api/widgets/solar/value -H 'content-type: application/json' -d '{"value": 3.4}'

# other sources: {"type":"system","metric":"processes|cpu|memory|load"}, {"type":"demo"}

# preview at any size
curl -o w.png 'localhost:8080/api/widgets/solar/preview.png?width=1280&height=720'

# remove
curl -X DELETE localhost:8080/api/widgets/solar
```

The admin API has no authentication. It's a PoC, so keep it on a trusted network.

## Layout

| Path | What |
| --- | --- |
| `src/hap/` | HomeKit Accessory Protocol: TLV8, SRP-6a, ChaCha20-Poly1305 sessions, pairing, accessory DB, HTTP server, mDNS |
| `src/camera/` | Stream negotiation (SetupEndpoints / SelectedRTPStreamConfiguration), RTP H.264 packetizer, SRTP, encode loop |
| `src/widgets/` | Widget model, renderer, data samplers |
| `src/hub.rs` | Shared state; dynamic widget add/remove → config number bump |
| `src/api.rs` | Admin HTTP API + preview page |
| `tools/e2e_check.py` | Acts as a HomeKit controller to test a running server end to end |

## Testing

```sh
cargo test   # TLV, SRP, SRTP (RFC 3711 vectors), RTP packetization, formatting
```

`tools/e2e_check.py` pairs with a fresh server and checks every layer against
an independent implementation:

* aiohomekit (Home Assistant's HomeKit client) for pairing
* `cryptography` for session encryption
* libsrtp for SRTP
* ffmpeg (via PyAV) for H.264 decoding

The script's docstring has setup instructions.

## Known gaps / next steps

* **Tested against a real home (Apple TV/HomePod hubs + iPhone).** Pairing,
  snapshots and live streams work, including several cameras at once. Two
  lessons from that: HomeKit ignores cameras that lack a Microphone service,
  and controllers connect over IPv6 too, so the server listens dual-stack.
  To debug a stream, run with `RUST_LOG=homestat=debug`. The periodic
  `stream stats` line shows send errors and how many RTCP reports came
  back from the controller.
* No event notifications (subscriptions are accepted but nothing is pushed). Not
  needed for cameras.
* No audio. A configuration is advertised but no audio is sent.
* Video is re-rendered only when a widget's data changes. Frames are still encoded
  at the requested fps, but static frames compress to a few bytes.
* Possible next steps: more styles (gauge, multi-series), widget config file /
  YAML, HTTP/MQTT/Prometheus sources, auth on the admin API.

Fonts: Open Sans (SIL OFL, `assets/fonts/OFL.txt`).
