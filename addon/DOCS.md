# Homestat

Homestat turns numbers from Home Assistant, Prometheus or MQTT into small
dashboard tiles (title, big value, sparkline) and shows each one as a
**camera** in the Apple Home app, so they appear on your iPhone, iPad, Mac and
Apple TV.

## Setup

1. Start the add-on and open **Homestat** in the sidebar.
2. In the Home app choose **Add Accessory** and scan the QR code shown there
   (or type the setup code). Confirm **Add Anyway** for the uncertified
   accessory.
3. Click **Add widget**, pick a source, and the new camera appears in Home a
   few seconds later.

Home Assistant entities work without any setup. MQTT works automatically if
the Mosquitto broker add-on is installed. Prometheus needs `prometheus_url`.

## Options

| Option | |
| --- | --- |
| `name` | Bridge name in the Home app. |
| `port` | HomeKit port. Change it if Homebridge already uses 51826. |
| `setup_code` | Setup code (`123-45-678`) used on **first start** only. Random if empty. |
| `sample_interval` | Seconds between samples. Sparklines show the last 120. |
| `api_lan_access` | Also serve the web UI and API on `http://<ha-host>:8099` without Home Assistant login. Needed only to push values from other machines. |
| `prometheus_url` | e.g. `http://192.168.1.20:9090`. |
| `mqtt_url` | `mqtt://user:pass@host:1883`. Overrides the Mosquitto add-on. |

## Widget sources

* **Home Assistant**: any entity with a numeric state (`sensor.*`,
  `input_number.*`, ...), or a numeric attribute. `on`/`off` style states
  become 1/0. The unit comes from the entity unless you set one.
* **Prometheus**: a PromQL query returning a single series. Wrap
  multi-series queries in `sum(...)`, `max(...)` etc.
* **MQTT**: a topic (wildcards allowed) whose payload is a number, or JSON
  with a dotted `json_path` such as `temperature` or `sensors.0.value`
  (handy for Zigbee2MQTT).

Home Assistant and Prometheus widgets start with their recent history
filled in.

## Resetting

To pair again from scratch (e.g. after moving to a new Home), click **Reset
pairing…** in the Homestat panel. The bridge gets a new HomeKit identity and
your widgets are kept. Then in the Home app remove the old, now unresponsive
bridge (**Home Settings → Hubs & Bridges → Homestat → Remove Bridge**) and add
it again with the new QR code.

## Installing from a private copy

While the repository is private, Home Assistant can't add it as an add-on
repository. Instead, copy this `addon` folder to `/addons/homestat` on your
Home Assistant host (Samba or SSH add-on), then **Settings → Add-ons → Add-on
store → ⋮ → Check for updates**, and install Homestat from *Local add-ons*.
This pulls the prebuilt image, which requires its GHCR package to be public.
Run `addon/make-local.sh` to produce a folder that builds from source instead
(slow on a Raspberry Pi: expect 20+ minutes).
