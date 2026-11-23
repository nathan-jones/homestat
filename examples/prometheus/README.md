# Prometheus test setup

A local Prometheus with a node-exporter, plus a script that adds one widget
of each style backed by it.

```sh
# 1. Prometheus on :9090 (docker compose works the same way)
podman compose -f examples/prometheus/compose.yml up -d

# 2. homestat pointed at it (a separate data dir keeps your real widgets out of it)
cargo run --release -- --prometheus-url http://localhost:9090 --data-dir ./data-prom

# 3. Widgets: CPU (stat), Memory (gauge), Load (graph), Network in (bar), Node exporter (state)
./examples/prometheus/seed-widgets.sh            # or pass the API URL, default http://localhost:8080
```

Open http://localhost:8080 to see them. The graphs start with history,
because homestat backfills from Prometheus's range API. Running the script
again updates the widgets rather than duplicating them.

To try the failure path, stop the exporter:

```sh
podman compose -f examples/prometheus/compose.yml stop node-exporter
```

Within one sample interval, "Node exporter" turns red and says **Down**
(`up` drops to 0). Memory and Load show "query returned no series" in amber
and keep their last value. CPU and Network carry on for about a minute,
until their `rate(...[1m])` window runs dry. Use `start` to bring it back.

On macOS, node-exporter reports the Docker/Podman VM, not your Mac.

Tear down: `podman compose -f examples/prometheus/compose.yml down`

> If pulling fails with `docker-credential-desktop: executable file not found`,
> pull the images directly first: `podman pull docker.io/prom/prometheus:v3.5.0 docker.io/prom/node-exporter:v1.9.1`
