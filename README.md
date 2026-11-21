# homestat

A HomeKit **bridge** for homelabs: it renders numbers from **Home Assistant**,
**Prometheus** and **MQTT** as small stat tiles (title, big value, sparkline)
and exposes each one as a **camera** in the Apple Home app, so they're on your
iPhone, iPad, Mac and Apple TV.

```
                 ┌──────────────────────────── homestat ─────────────────────────────┐
 Home app ◄─HAP─►│ HAP server (pair-setup/verify, /accessories, /characteristics,     │
 (iPhone/        │             /resource snapshots)          ▲                         │
  Apple TV)      │                                           │ c# bump + mDNS re-announce
     ▲           │ Hub: widgets ── aid 2..n (bridged cameras)┘ on add/remove          │
     │           │   │                                                                │
     └─SRTP/UDP──│ stream thread: render (tiny-skia) → H.264 (openh264) → RTP → SRTP  │
                 │   ▲                                                                │
                 │ sources: HA · Prometheus · MQTT · host   web UI + API :8080          │
                 └────────────────────────────────────────────────────────────────────┘
```

* **One pairing, many cameras.** The server is a HAP bridge (aid 1); every widget
  is a bridged camera accessory with a stable aid. Adding or removing a widget
  bumps the HAP configuration number (`c#`) and re-announces over mDNS, so Home
  picks up the change without re-pairing. (HomeKit caps a bridge at 150 accessories.)
* **Self-contained.** Everything from SRP pairing to H.264 encoding and SRTP is in
  the binary. No ffmpeg or Homebridge needed. openh264 is compiled from source by
  its crate.
* **Several widget types.** Stat, gauge, bar, graph and state (see below). A new
  type is a `widgets::Style` variant plus a module in `widgets/render/`.
  Rendering is proportional, so it works at any resolution Home asks for.

## Install

Every option is a flag or an environment variable (`homestat --help`). The
pairing keys and widgets live in the data directory, so back it up: losing it
means removing the bridge from Home and pairing again (the web UI's
**Reset pairing** does the same on purpose, keeping your widgets).

### Docker

Needs **host networking** (HomeKit uses mDNS discovery and sends video over
UDP straight to the viewer), so run it on Linux.

```sh
cp .env.example .env    # fill in the sources you use
docker compose up -d
docker compose logs     # shows the pairing QR code
```

Or without compose:

```sh
docker run -d --name homestat --network host -v ./data:/data \
  -e HOMESTAT_HA_URL=http://homeassistant.local:8123 -e HOMESTAT_HA_TOKEN=... \
  ghcr.io/nathan-jones/homestat
```

Then open `http://<host>:8080`, scan the QR code with the Home app (**Add
Accessory**, then confirm *Add Anyway* for the uncertified accessory), and add
widgets.

### Home Assistant add-on

The repository is an add-on repository (`repository.yaml`, add-on in
`addon/`). The add-on talks to Home Assistant through the Supervisor (no token
needed), finds the Mosquitto add-on's broker automatically, and shows its UI
in the sidebar via ingress. See [addon/DOCS.md](addon/DOCS.md), which also
covers installing it while this repo is private.

### From source

```sh
cargo run --release -- --setup-code 031-45-154 --ha-url http://homeassistant.local:8123 --ha-token ...
```

## Configuration

| Flag | Env | Default | |
| --- | --- | --- | --- |
| `--name` | `HOMESTAT_NAME` | Homestat | Bridge name in Home |
| `--port` | `HOMESTAT_PORT` | 51826 | HomeKit TCP port (Homebridge uses this too) |
| `--api-port` | `HOMESTAT_API_PORT` | 8080 | Web UI and API |
| `--api-allow` | `HOMESTAT_API_ALLOW` | anyone | Comma separated IPs allowed to use the UI/API (loopback always is) |
| `--data-dir` | `HOMESTAT_DATA_DIR` | ./data (/data in Docker) | Keys, pairings, widgets |
| `--setup-code` | `HOMESTAT_SETUP_CODE` | random | Used on first run only |
| `--sample-interval` | `HOMESTAT_SAMPLE_INTERVAL` | 5 | Seconds between samples; sparklines show 120 |
| `--ha-url`, `--ha-token` | `HOMESTAT_HA_URL`, `HOMESTAT_HA_TOKEN` | | Home Assistant + long-lived access token |
| `--prometheus-url` | `HOMESTAT_PROMETHEUS_URL` | | Prometheus-compatible HTTP API |
| `--mqtt-url` | `HOMESTAT_MQTT_URL` | | `mqtt://[user[:pass]@]host[:port]` (no TLS yet) |

### Data sources in the web UI

Home Assistant, Prometheus and MQTT can also be set up in the web UI: click a
source's name at the top of the page. **Test** checks the settings without
saving them. **Save** applies them immediately, with no restart: widgets
recover within one sample interval and MQTT reconnects. Settings are stored
in `<data dir>/providers.json`, readable only by its owner because it holds
the token and password, which the UI never shows again. A source set by a
flag or environment variable (or by the add-on) wins over the UI and appears
there as read-only.

## Widget sources

| `source` | Fields | |
| --- | --- | --- |
| `homeassistant` | `entity_id`, `attribute`? | Numeric state or attribute; `on`/`off`-style states become 1/0; unit taken from the entity |
| `prometheus` | `query` | Instant query that returns exactly one series (wrap in `sum()` etc.) |
| `mqtt` | `topic`, `json_path`? | Topic filter (wildcards OK); number payload, or JSON picked with a dotted path like `temperature` or `values.0` |
| `system` | `metric` | `cpu`, `memory`, `load`, `processes` of the machine homestat runs on |
| `push` | | Values POSTed to the API |
| `demo` | | Random walk |

Every widget samples at the same steady rate. Remote values are held between
updates, which matters for MQTT sensors that publish rarely. Home Assistant
and Prometheus widgets start with their recent history filled in. If a source
fails (entity missing, broker down, bad query), the widget keeps its last
value and shows the error in amber on the tile itself, so it's visible in
Home.

## Widget types

Set with `"style"`, or pick one in the widget dialog.

| Style | Shows | Options |
| --- | --- | --- |
| `stat` (default) | Title, big value, sparkline (Grafana singlestat) | |
| `gauge` | Semicircular dial with the value in the middle | `min`, `max` (default 0–100) |
| `bar` | Big value over a horizontal progress bar | `min`, `max` (default 0–100) |
| `graph` | Full chart with y-axis and gridlines, current value top right | `min`, `max` (default: auto-scaled to the data, so 19–23 °C isn't a flat line) |
| `state` | A word for the current value, in its colour on a tinted tile | `states`: `[{"value": 0, "label": "Closed", "color": "#34c759"}, ...]`; without states, 0/1 show as Off/On |

```sh
curl -X POST localhost:8080/api/widgets -H 'content-type: application/json' -d '{
  "name": "Garage door", "style": "state",
  "source": {"type": "homeassistant", "entity_id": "binary_sensor.garage_door"},
  "states": [{"value": 0, "label": "Closed", "color": "#34c759"},
             {"value": 1, "label": "Open", "color": "#ff453a"}]}'
```

Home Assistant's `on`/`off`, `open`/`closed` and `home`/`not_home` states
arrive as 1/0, so they work with State widgets directly.

## Appearance

Each widget can set its own colours: `background`, `text` (title and
value), `line` and `fill` (the sparkline), as `#rrggbb`. Unset ones use the
default dark theme, an unset fill is a tint of the line colour, and the
border follows the background. The widget dialog has preset themes, colour
pickers and a live preview. Over the API:

```sh
curl -X PUT localhost:8080/api/widgets/solar -H 'content-type: application/json' -d '{
  "name": "Solar", "unit": "kW", "source": {"type": "mqtt", "topic": "solar/power"},
  "colors": {"background": "#f5f5f7", "text": "#1d1d1f", "line": "#0a84ff"}}'
```

Colour changes reach open camera views immediately, and the HomeKit name and
room are kept.

## Web UI and API

The web UI (`http://<host>:8080`) handles HomeKit pairing (QR code and setup
code, paired controllers and which are connected right now, and a reset that
gives the bridge a fresh identity so it can be added again), shows live
previews, and lets you add, edit and remove widgets. Editing keeps the camera's identity, so its room
and settings in Home survive. The API underneath:

```sh
curl localhost:8080/api/widgets                       # list, with value + status
curl -X POST localhost:8080/api/widgets -H 'content-type: application/json' \
  -d '{"name": "Solar", "unit": "kW", "source": {"type": "homeassistant", "entity_id": "sensor.solar_power"}}'
curl -X PUT localhost:8080/api/widgets/solar -H 'content-type: application/json' \
  -d '{"name": "Solar", "decimals": 2, "source": {"type": "mqtt", "topic": "solar/power"}}'
curl -X POST localhost:8080/api/widgets/solar/value -H 'content-type: application/json' -d '{"value": 3.4}'  # push sources
curl -o w.png 'localhost:8080/api/widgets/solar/preview.png?width=1280&height=720'
curl -X DELETE localhost:8080/api/widgets/solar
curl localhost:8080/api/pairing                       # paired?, setup code, controllers + live connections
curl -X POST localhost:8080/api/pairing/reset         # forget all controllers, new HomeKit identity
```

The API has no authentication of its own. Keep it on a trusted network or
restrict it with `--api-allow`. In the add-on, only Home Assistant's
authenticated ingress can reach it by default.

## Layout

| Path | What |
| --- | --- |
| `src/hap/` | HomeKit Accessory Protocol: TLV8, SRP-6a, ChaCha20-Poly1305 sessions, pairing, accessory DB, HTTP server, mDNS |
| `src/camera/` | Stream negotiation (SetupEndpoints / SelectedRTPStreamConfiguration), RTP H.264 packetizer, SRTP, encode loop |
| `src/widgets/` | Widget model, renderer, sampler, remote sources (HA/Prometheus/MQTT) |
| `src/hub.rs` | Shared state; dynamic widget add/edit/remove → config number bump |
| `src/api.rs`, `src/ui.html` | Web UI and API |
| `src/addon.rs` | Home Assistant add-on mode (options, Supervisor MQTT discovery) |
| `addon/` | Home Assistant add-on definition |
| `tools/e2e_check.py` | Acts as a HomeKit controller to test a running server end to end |

## Testing

```sh
cargo test   # TLV, SRP, SRTP (RFC 3711 vectors), RTP packetization, rendering, source parsing
```

`tools/e2e_check.py` pairs with a fresh server and checks every layer against
an independent implementation:

* aiohomekit (Home Assistant's HomeKit client) for pairing
* `cryptography` for session encryption
* libsrtp for SRTP
* ffmpeg (via PyAV) for H.264 decoding

The script's docstring has setup instructions.

CI (`.github/workflows/ci.yml`) runs the tests and publishes a multi-arch
(amd64/arm64) image to `ghcr.io/<owner>/homestat`: `:edge` from main, and
`:<version>` and `:latest` from `v*` tags. The add-on's `version` must match a
published tag.

## Known gaps / next steps

* **Tested against a real home (Apple TV/HomePod hubs + iPhone).** Pairing,
  snapshots and live streams work, including several cameras at once. Two
  lessons from that: HomeKit ignores cameras that lack a Microphone service,
  and controllers connect over IPv6 too, so the server listens dual-stack.
  To debug a stream, run with `RUST_LOG=homestat=debug`. The periodic
  `stream stats` line shows send errors and how many RTCP reports came
  back from the controller.
* No MQTT over TLS yet, and Home Assistant is polled rather than subscribed
  to over its websocket.
* Stat sparklines are zero-based like Grafana's, which flattens values that
  sit far from zero (e.g. temperatures). Use the graph type for those.
* No audio. A configuration is advertised but no audio is sent.
* No event notifications (subscriptions are accepted but nothing is pushed).
  Not needed for cameras.
* Possible next steps: threshold colours, multi-series graphs, widget config file
  (YAML) for GitOps setups, doorbell-style alerts on thresholds.

Fonts: Open Sans (SIL OFL, `assets/fonts/OFL.txt`).
