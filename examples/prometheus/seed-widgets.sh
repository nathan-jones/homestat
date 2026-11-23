#!/bin/sh
# Adds one widget of each style backed by the example Prometheus.
# Usage: seed-widgets.sh [homestat API URL]   (default http://localhost:8080)
set -eu
API=${1:-http://localhost:8080}

add() {
  printf '%s: ' "$1"
  curl -fs -X PUT "$API/api/widgets/$1" -H 'content-type: application/json' -d "$2" >/dev/null && echo ok ||
    curl -fsS -X POST "$API/api/widgets" -H 'content-type: application/json' -d "$(printf '%s' "$2" | sed "s/^{/{\"id\": \"$1\", /")" >/dev/null && echo created
}

add prom-cpu '{"name": "CPU", "unit": "%", "decimals": 0, "style": "stat",
  "source": {"type": "prometheus", "query": "100 * (1 - avg(rate(node_cpu_seconds_total{mode=\"idle\"}[1m])))"},
  "thresholds": [{"from": 80, "color": "#ff9f0a"}, {"from": 95, "color": "#ff453a"}]}'

add prom-memory '{"name": "Memory", "unit": "%", "decimals": 0, "style": "gauge", "min": 0, "max": 100,
  "source": {"type": "prometheus", "query": "100 * (1 - sum(node_memory_MemAvailable_bytes) / sum(node_memory_MemTotal_bytes))"},
  "thresholds": [{"from": 80, "color": "#ff9f0a"}, {"from": 95, "color": "#ff453a"}]}'

add prom-load '{"name": "Load (1m)", "decimals": 2, "style": "graph",
  "source": {"type": "prometheus", "query": "sum(node_load1)"}}'

add prom-network '{"name": "Network in", "unit": "KB/s", "decimals": 1, "style": "bar", "min": 0, "max": 100,
  "source": {"type": "prometheus", "query": "sum(rate(node_network_receive_bytes_total{device!=\"lo\"}[1m])) / 1024"}}'

add prom-up '{"name": "Node exporter", "style": "state",
  "source": {"type": "prometheus", "query": "sum(up{job=\"node\"})"},
  "states": [{"value": 0, "label": "Down", "color": "#ff453a"},
             {"value": 1, "label": "Up", "color": "#34c759"}]}'
