#!/usr/bin/env bash
# Start the ForgeVCS metrics stack: Prometheus + Blackbox + Alertmanager +
# Alert relay + Grafana. Everything binds to 127.0.0.1 only.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

docker info >/dev/null 2>&1 || {
  echo "Docker is not running. Start Docker Desktop and retry." >&2
  exit 1
}

if [[ ! -f .env ]]; then
  cp .env.example .env
  echo "Created observability/.env from .env.example (set a real Grafana password)."
fi

docker compose up -d

for i in $(seq 1 60); do
  if curl -fsS http://127.0.0.1:9090/-/ready >/dev/null 2>&1 \
     && curl -fsS http://127.0.0.1:3002/api/health >/dev/null 2>&1; then
    echo "metrics stack ready after ${i}s"
    echo "  Grafana:      http://127.0.0.1:3002  (admin / value in observability/.env)"
    echo "  Prometheus:   http://127.0.0.1:9090"
    echo "  Alertmanager: http://127.0.0.1:9093"
    echo "  Dashboard:    http://127.0.0.1:3002/d/forgevcs-overview"
    exit 0
  fi
  sleep 1
done

echo "stack did not become ready in 60s; run: docker compose ps && docker compose logs" >&2
exit 1
