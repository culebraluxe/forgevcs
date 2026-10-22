#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "Prometheus:"
curl -fsS http://127.0.0.1:9090/-/ready || true
echo
echo "Grafana:"
curl -fsS http://127.0.0.1:3002/api/health || true
echo
echo "Alertmanager:"
curl -fsS http://127.0.0.1:9093/-/ready || true
echo

echo "Scrape targets:"
curl -fsS 'http://127.0.0.1:9090/api/v1/targets?state=active' | python3 -c '
import json, sys
data = json.load(sys.stdin)
for target in data["data"]["activeTargets"]:
    labels = target.get("labels", {})
    print("  %s: %s %s" % (labels.get("job"), target.get("health"), target.get("lastError", "")))
'

echo
echo "Firing alerts:"
curl -fsS 'http://127.0.0.1:9090/api/v1/alerts' | python3 -c '
import json, sys
alerts = json.load(sys.stdin)["data"]["alerts"]
if not alerts:
    print("  none")
else:
    for alert in alerts:
        print("  %s [%s]" % (alert["labels"].get("alertname"), alert["state"]))
'
