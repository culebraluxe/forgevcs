#!/usr/bin/env bash
# Send a synthetic Alertmanager alert through the relay. Use this to verify
# Slack/Discord delivery without waiting for a real incident.
set -euo pipefail
curl -fsS -X POST http://127.0.0.1:9093/api/v2/alerts \
  -H 'Content-Type: application/json' \
  -d '[{"labels":{"alertname":"ForgeVCSWatchdogTest","severity":"warning"},"annotations":{"summary":"Synthetic ForgeVCS alert test"}}]'
echo
echo "Sent. Check the alert-relay logs: docker compose logs alert-relay"
