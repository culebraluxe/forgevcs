# ForgeVCS observability stack

Self-hosted collector/dashboard/alert layer for the ForgeVCS production service.
It closes the hosted half of TD-6 without changing the Rust service.

```
Vercel ForgeVCS  ──/metrics──────────►  Prometheus  ──►  Grafana (dashboard)
      │                                   │
      └──/health, /ready──► Blackbox ─────┘
                                          └──► Alertmanager ──► alert-relay ──► Slack/Discord webhook
```

## What is included

- **Prometheus** scrapes `https://culebraluxe-forgevcs.vercel.app/metrics` every 30s.
- **Blackbox exporter** probes `/health` and `/ready`.
- **Grafana** auto-provisions the `ForgeVCS — Overview` dashboard and Prometheus datasource.
- **Alertmanager + alert relay** evaluate the rules below and post to a webhook.
- Everything binds to `127.0.0.1`; no host ports are exposed publicly.

Dashboard panels: service up, readiness, land success ratio, conflict pauses, land outcomes,
land p95/p99, conflict pauses by operation, continue outcomes, ref-CAS contention, RPC p95 by code,
and storage access.

## Quick start

```bash
cd observability
cp .env.example .env          # set GRAFANA_ADMIN_PASSWORD and optionally ALERT_WEBHOOK_URL
./scripts/start.sh
```

Open:

- Grafana: <http://127.0.0.1:3002/d/forgevcs-overview> — anonymous Viewer is enabled; admin login uses `.env`
- Prometheus: <http://127.0.0.1:9090>
- Alertmanager: <http://127.0.0.1:9093>

Check health/targets/alerts:

```bash
./scripts/status.sh
```

Reload after editing rules:

```bash
./scripts/reload.sh
```

Send a synthetic alert through the relay:

```bash
./scripts/test-alert.sh
docker compose logs alert-relay
```

Stop without deleting history:

```bash
./scripts/stop.sh
```

Prometheus retains 30 days in the `prometheus-data` volume.

## Alert rules

| Alert | Condition | Severity |
| --- | --- | --- |
| `ForgeVCSMetricsDown` | `/metrics` scrape `up == 0` for 2m | critical |
| `ForgeVCSNotReady` | `/ready` probe fails for 2m | critical |
| `ForgeVCSLandFailureRatioHigh` | failed land attempts > 5% over 5m | warning |
| `ForgeVCSConflictPausesSpike` | conflict pauses > 0.1/s over 5m | warning |
| `ForgeVCSRefCasContention` | ref CAS failures > 0.5/s over 10m | warning |
| `ForgeVCSLandP99Slow` | land p99 > 2s over 10m | warning |
| `ForgeVCSConflictContinueFailing` | continue `land_failed`/`error` > 0 over 10m | warning |

All thresholds are in `prometheus/rules/forgevcs.yml`; tune after real traffic.

### Notification target

Set `ALERT_WEBHOOK_URL` in `.env` and restart. The relay sends a generic JSON body containing
both `text` (Slack-compatible) and `content` (Discord-compatible). With it empty, alerts are still
evaluated and logged by `alert-relay`.

## Important production caveats

- **Vercel does not scrape `/metrics`.** This stack is the collector; run it on a machine/VPS that
  is expected to be up. A laptop that sleeps creates scrape gaps.
- **Metrics are per process and counters reset on cold start.** Use `rate`/`increase` over a window,
  not raw counters, for alerts. With multiple instances, aggregate with `sum(...)`.
- **`/metrics` is currently public.** If that is not acceptable, restrict it at the Vercel edge;
  the Prometheus scrape must then send the bypass header.
- **No distributed traces.** This is metrics + readiness only; tracing remains a separate item.
- **Do not commit `.env`.** Only `.env.example` is tracked.
- The Prometheus target is hardcoded to the production alias in `prometheus/prometheus.yml`; edit
  it for staging or a different deployment.

## Files

```
docker-compose.yml
prometheus/prometheus.yml
prometheus/rules/forgevcs.yml
blackbox/blackbox.yml
alertmanager/alertmanager.yml
alert-relay/relay.py
grafana/provisioning/...
grafana/dashboards/forgevcs.json
scripts/{start,stop,status,reload,test-alert}.sh
```
