# Deployment

By default zenshim listens on `127.0.0.1` only, so nothing outside your
machine can reach it. This page covers the ways to change that, and how
to run it unattended.

zenshim speaks plain HTTP. Whenever it is reachable from other machines,
the **gateway key is the only thing protecting it**, and it travels
unencrypted unless something in front adds TLS (the tunnel does).

## Public tunnel

```bash
zenshim --tunnel
```

or press `T` in the app. This starts a free
[cloudflared quick tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/do-more-with-tunnels/trycloudflare/):
no account, and a public `https://….trycloudflare.com` address that
forwards to your gateway. Use it as the base URL from anywhere (add
`/v1` for OpenAI clients).

- `cloudflared` is used from your `PATH` if present; otherwise it is
  downloaded once to `~/.local/share/zenshim/`. On Apple-silicon Macs
  install it yourself (`brew install cloudflared`).
- The address is new each time the tunnel starts.
- Callers are rate-limited individually, by the address Cloudflare
  reports for them.
- If you suspect the key has leaked, press `R` to replace it.

## Your own network or domain

```bash
zenshim --host 0.0.0.0
```

listens on every interface. For anything beyond a trusted LAN, put a
reverse proxy with TLS in front and keep zenshim itself on `127.0.0.1`.
A minimal Caddy example:

```
zen.example.com {
    reverse_proxy 127.0.0.1:8480 {
        flush_interval -1
    }
}
```

For nginx, turn off response buffering for streaming; zenshim already
sends `X-Accel-Buffering: no`. A proxy on the same machine is trusted to
report the caller's address in `X-Forwarded-For`, so the rate limit
applies per real client.

## Running without a terminal

Started by a service manager, in a container or with its output
redirected, zenshim prints log lines instead of drawing the app.
Configure it with flags or environment variables and add `--no-wizard`
so it never waits for input:

```bash
ZENSHIM_KEYS=key1,key2 zenshim --no-tui --no-wizard
```

On the first such start, zenshim builds its config from those settings,
generates a gateway key and saves both. The key is deliberately not
printed, so it does not end up in logs:

```bash
zenshim --show-key
```

To set the key yourself, pass `ZENSHIM_GATEWAY_KEY`.

zenshim shuts down cleanly on `SIGINT` and `SIGTERM`: it stops accepting
requests, gives answers in flight a few seconds, and stops the tunnel
and any CLI backends.

## Docker

The [`Dockerfile`](../Dockerfile) builds from source:

```bash
docker build -t zenshim .
docker run -d --name zenshim \
  -p 127.0.0.1:8480:8480 \
  -v zenshim:/data \
  zenshim
docker exec zenshim zenshim --show-key
```

Releases also publish a ready-made image at `ghcr.io/rabden/zenshim`
(tags `latest` and the version number) for `linux/amd64` and
`linux/arm64`.

- The config and the generated key live in the `/data` volume.
- The process runs as an unprivileged user.
- Pass settings with `-e`, for example `-e ZENSHIM_KEYS=key1,key2`.
- **Rate limit.** Requests reach the container from Docker's bridge
  address, so all callers look like one remote client and share its 120
  requests a minute. If only you can reach the port, set
  `-e ZENSHIM_RATE_LIMIT=0`.
- **No CLI route.** The image does not contain OpenCode, so it uses the
  direct route only.

[`deploy/docker-compose.yml`](../deploy/docker-compose.yml) runs the same
thing with a read-only root filesystem, all capabilities dropped and
`no-new-privileges`:

```bash
docker compose -f deploy/docker-compose.yml up -d
```

## systemd

[`deploy/zenshim.service`](../deploy/zenshim.service) runs zenshim as a
throwaway user that owns only `/var/lib/zenshim`, with a restricted
filesystem view and system-call filter.

```bash
sudo install -m 755 zenshim /usr/local/bin/zenshim
sudo install -m 644 deploy/zenshim.service /etc/systemd/system/
sudo systemctl enable --now zenshim
sudo ZENSHIM_CONFIG_DIR=/var/lib/zenshim zenshim --show-key
```

Settings go in `/etc/zenshim.env`, one `NAME=value` per line:

```
ZENSHIM_KEYS=key1,key2
ZENSHIM_PORT=8480
```

To use the CLI route under this unit, install OpenCode somewhere the
service can read (not under a home directory, which the unit hides) and
set `ZENSHIM_OPENCODE` to its path.

## Metrics

`GET /metrics` needs the gateway key.

```bash
curl -H "Authorization: Bearer $KEY" http://127.0.0.1:8480/metrics
```

returns JSON: version, uptime, request totals, tokens, average time,
the state of the routes, and per-key counters. For Prometheus, ask for
text:

```yaml
scrape_configs:
  - job_name: zenshim
    metrics_path: /metrics
    params: { format: [prometheus] }
    authorization: { credentials: sk-zenshim-… }
    static_configs:
      - targets: ["127.0.0.1:8480"]
```

| Metric | Type | Meaning |
|---|---|---|
| `zenshim_uptime_seconds` | gauge | seconds since start |
| `zenshim_requests_total` | counter | requests received |
| `zenshim_requests_succeeded_total` | counter | requests answered successfully |
| `zenshim_requests_failed_total` | counter | requests answered with an error |
| `zenshim_requests_active` | gauge | requests in flight |
| `zenshim_request_seconds_sum_ms` | counter | milliseconds spent on successful requests |
| `zenshim_tokens_total{direction}` | counter | tokens reported by Zen, `input` and `output` |
| `zenshim_direct_blocked_seconds` | gauge | seconds until the direct route is retried; `0` when in use |
| `zenshim_key_served_total{key,route}` | counter | requests served per upstream key and route |
| `zenshim_key_resting_seconds{key,route}` | gauge | seconds an exhausted route still rests |
| `zenshim_key_limited_total{key}` | counter | quota and rate-limit responses per key |
| `zenshim_key_failed_total{key}` | counter | other failures per key |

Keys are numbered in the order shown on the Keys tab; the keys
themselves never appear. Counters start at zero on each start.

`GET /health` and `GET /ready` answer without a key, for liveness and
readiness probes.
