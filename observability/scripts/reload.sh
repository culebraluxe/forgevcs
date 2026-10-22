#!/usr/bin/env bash
set -euo pipefail
curl -fsS -X POST http://127.0.0.1:9090/-/reload
echo "Prometheus configuration and rules reloaded."
