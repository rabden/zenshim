# Configuration

Settings come from three places, strongest first: **command-line
flags**, **environment variables**, and the **config file**.

## Flags and environment variables

### Starting

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--setup` | | | Run the interactive setup again. |
| `--no-wizard` | | | Never ask questions; use flags, environment and the saved config. |
| `--no-tui` | | | Print a summary and log lines instead of the full-screen app. |
| `--show-key` | | | Print the saved gateway key and exit. |

### Keys and models

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--keys k1,k2` | `ZENSHIM_KEYS` | saved, else `public` | Upstream Zen keys. `public` is the anonymous free tier. |
| `--models a,b` | `ZENSHIM_MODELS` | saved, else every `-free` model | Models to serve. |
| `--all-models` | | | Serve the whole live catalog. |
| `--gateway-key KEY` | `ZENSHIM_GATEWAY_KEY` | saved, else generated | What clients authenticate with. |

### Network

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--host ADDR` | `ZENSHIM_HOST` | `127.0.0.1` | Address to listen on. `0.0.0.0` listens on every interface. |
| `--port N` | `ZENSHIM_PORT` | `8480` | Port to listen on. |
| `--tunnel` | `ZENSHIM_TUNNEL` | off | Open a public cloudflared quick tunnel. |
| `--format F` | | `both` | Dialects to serve: `openai`, `anthropic` or `both`. |

### Routes

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--backend B` | `ZENSHIM_BACKEND` | `auto` | `auto`, `direct` or `cli`. See [Keys, routes and fallback](routing.md). |
| `--opencode PATH` | `ZENSHIM_OPENCODE` | found on `PATH` | The OpenCode CLI to use for the CLI route. |

### Limits

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--rate-limit N` | `ZENSHIM_RATE_LIMIT` | `120` | Requests per minute per remote client. `0` turns it off. |
| `--max-concurrent N` | `ZENSHIM_MAX_CONCURRENT` | `16` | Generations running at once. |
| `--max-queue N` | `ZENSHIM_MAX_QUEUE` | `64` | Requests that may wait for a free slot. |
| `--max-body-mb N` | `ZENSHIM_MAX_BODY_MB` | `32` | Largest request body, in MB. |
| `--request-timeout N` | `ZENSHIM_REQUEST_TIMEOUT` | `600` | Seconds one generation may take. |

### Other environment variables

| Variable | Meaning |
|---|---|
| `ZENSHIM_CONFIG_DIR` | Directory for `config.json` and `state.json`. |
| `ZENSHIM_CLOUDFLARED` | Path to a `cloudflared` binary to use for the tunnel. |
| `ZENSHIM_UPSTREAM_BASE` | Replace Zen's API address. For testing against a stand-in. |

## How the limits behave

- **Rate limit.** Counted per client address, per minute. Clients on the
  same machine are never limited. The count happens before the gateway
  key is checked, so guessing keys is limited too. Over the limit:
  `429` with `Retry-After`.
- **Who is "remote".** A request from another machine is identified by
  its address. A request arriving from this machine with a
  `CF-Connecting-IP` or `X-Forwarded-For` header (the tunnel, or a
  reverse proxy running here) is identified by the forwarded address.
  Forwarded headers from other machines are ignored, so a remote caller
  cannot choose its own identity.
- **Concurrency.** Up to `--max-concurrent` generations run at once. Up
  to `--max-queue` more wait, for at most 30 seconds each. Beyond
  either: `429`.
- **Timeouts.** A generation may take `--request-timeout` seconds in
  total, and upstream may stay silent for at most 3 minutes mid-answer.
  A non-streaming request that runs out gets `504`; a streaming one is
  ended where it stands.
- **Body size.** Larger bodies get `413`. Base64 makes files about a
  third bigger than they are on disk.

## Files

| File | Holds |
|---|---|
| `config.json` | upstream keys, served models, port, gateway key |
| `state.json` | which key routes are resting, and until when |

They live in the first of these that applies:

1. `$ZENSHIM_CONFIG_DIR/`
2. `$XDG_CONFIG_HOME/zenshim/`
3. `~/.config/zenshim/`

Both are written readable only by you (`0600`), and replaced atomically
so an interrupted write cannot leave half a file.

`config.json`:

```json
{
  "keys": ["public"],
  "models": ["muse-spark-1.3-contributor-free", "space-bunny-free"],
  "port": 8480,
  "gateway_key": "sk-zenshim-…"
}
```

It is created by setup, or on the first start when settings come from
flags or the environment alone. After that it changes only when you
change something in the app or run `--setup`.

Other paths zenshim uses:

| Path | Purpose |
|---|---|
| `~/.local/share/zenshim/cloudflared` | the tunnel binary, downloaded on first use |
| `$TMPDIR/zenshim-oc-*` | sandboxes of running CLI backends; removed on exit |

## How a start is decided

1. A saved config is loaded, if there is one.
2. With no config and no `--keys`, `--models` or `--no-wizard`, setup
   runs (the full-screen one in a terminal).
3. With no config but with those flags, a config is built from them:
   keys default to `public`, models to every free model in the live
   catalog, and a gateway key is generated. It is then saved.
4. `--keys`, `--models`, `--all-models`, `--gateway-key` and `--port`
   override the loaded values for this run without rewriting the file.
