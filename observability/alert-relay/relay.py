#!/usr/bin/env python3
"""Minimal Alertmanager webhook relay.

Receives Alertmanager webhook POSTs, logs a readable summary, and optionally
forwards a Slack/Discord-compatible message to ALERT_WEBHOOK_URL.

No dependencies, no secrets in code. The relay is deliberately small: metrics
storage/dashboards are Prometheus/Grafana; this only delivers notifications.
"""
from __future__ import annotations

import json
import os
import sys
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer


WEBHOOK = os.environ.get("ALERT_WEBHOOK_URL", "").strip()


def format_text(payload: dict) -> str:
    status = payload.get("status", "unknown").upper()
    alerts = payload.get("alerts", [])
    lines = [f"ForgeVCS {status}: {len(alerts)} alert(s)"]
    for alert in alerts[:10]:
        labels = alert.get("labels", {})
        annotations = alert.get("annotations", {})
        lines.append(
            f"- {labels.get('alertname', 'alert')} [{labels.get('severity', 'unknown')}]: "
            f"{annotations.get('summary', '')}"
        )
    if len(alerts) > 10:
        lines.append(f"... and {len(alerts) - 10} more")
    return "\n".join(lines)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 - BaseHTTPRequestHandler API
        if self.path != "/alerts":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            self.send_error(400, "invalid JSON")
            return

        text = format_text(payload)
        print(text, flush=True)

        if WEBHOOK:
            # Slack accepts {"text": ...}; Discord accepts {"content": ...}.
            body = json.dumps({"text": text, "content": text}).encode()
            req = urllib.request.Request(
                WEBHOOK,
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    print(f"forwarded: HTTP {response.status}", flush=True)
            except Exception as exc:  # noqa: BLE001 - delivery must not crash the relay
                print(f"forward failed: {exc}", file=sys.stderr, flush=True)

        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def do_GET(self):  # noqa: N802
        if self.path == "/healthz":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"ok")
            return
        self.send_error(404)

    def log_message(self, fmt, *args):
        print(fmt % args, flush=True)


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8080"))
    print(f"alert relay listening on :{port}", flush=True)
    HTTPServer(("0.0.0.0", port), Handler).serve_forever()
