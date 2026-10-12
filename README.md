# zenshim

**A local OpenAI-compatible API gateway for OpenCode Zen's free tier.**

Zen's free models (muse-spark, exo, space-bunny, …) reject any client
that isn't the official OpenCode CLI:

```
403 FreeTierError: "OpenCode's free tier can only be used from within OpenCode"
```

zenshim runs as a tiny local server that speaks the OpenAI
Chat Completions API on one side and the gate-passing request shape on
the other. Point any OpenAI-compatible product — SDKs, agents, IDE
plugins, curl — at it and the free models just work, **including
function calling and image input**, transparently translated.

Single static binary, no runtime, no config files required — start it,
use it, Ctrl-C it.

## Install

One-liner (Linux/macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/rabden/zenshim/main/install.sh | sh
```

Installs the latest release binary to `/usr/local/bin` (or `~/.local/bin`)
and prints `zenshim <version>`. Windows: download
`zenshim-x86_64-pc-windows-msvc.zip` from
[releases](https://github.com/rabden/zenshim/releases).

From source:

```bash
cargo build --release
# binary: target/release/zenshim
```

## Use

```bash
./zenshim
```

First run starts an interactive setup:

1. paste your Zen API keys (or leave blank for the anonymous free tier),
2. keys are validated live against the catalog,
3. pick which free models to serve (all `-free` models preselected),
4. done — it prints the endpoint and a generated gateway key:

```
serving 9 model(s) with 1 upstream key(s)
zenshim listening on http://127.0.0.1:8480/v1
gateway key: sk-zenshim-3f9a…
```

Then from anything that speaks OpenAI:

```bash
curl http://127.0.0.1:8480/v1/chat/completions \
  -H "Authorization: Bearer sk-zenshim-…" \
  -H "Content-Type: application/json" \
  -d '{"model": "muse-spark-1.3-contributor-free",
       "messages": [{"role": "user", "content": "hello"}]}'
```

Or in an SDK:

```python
client = OpenAI(base_url="http://127.0.0.1:8480/v1", api_key="sk-zenshim-…")
```

### API surface

zenshim speaks **both** major client dialects, simultaneously by default:

| Route | Dialect | Behavior |
|---|---|---|
| `POST /v1/chat/completions` | OpenAI | streaming + non-streaming; `tools` → standard `tool_calls` (results back as `role:"tool"`); `image_url` parts supported |
| `POST /v1/messages` | Anthropic | streaming + non-streaming; `tools` → `tool_use` blocks (results back as `tool_result`); base64 `image` sources supported; auth via `x-api-key` or `Bearer` |
| `GET /v1/models` | OpenAI | the models selected at setup |
| `GET /health` | — | liveness |

Point Anthropic SDKs at it directly:

```python
client = Anthropic(base_url="http://127.0.0.1:8480", api_key="sk-zenshim-…")
client.messages.create(model="muse-spark-1.3-contributor-free", max_tokens=1024,
                       messages=[{"role": "user", "content": "hi"}])
```

To serve a single dialect: `--format openai` or `--format anthropic`
(the other route then returns 404). Note: reasoning models spend output
tokens on thinking, so the gateway floors `max_tokens` at 1024 and reports
real truncation as `stop_reason: "max_tokens"` / `finish_reason: "length"`.

### Key stacking

Pass multiple upstream keys (comma-separated in the wizard, or
`--keys k1,k2,…`). On `402`/`429` from Zen the gateway rotates to the
next key and retries; exhausted keys are skipped until restart.

### Console

While running, zenshim offers an interactive console (in a terminal):

```
zenshim> k    copy the API key to the clipboard
zenshim> K    reveal the API key on screen
zenshim> m    list served models, pick one to copy its model id ('a' copies all)
zenshim> u    copy the base URL
zenshim> t    copy the tunnel URL (when --tunnel is active)
zenshim> s    reprint the summary
zenshim> q    shut down
```

The key stays off screen by default; values go straight to the clipboard
(xclip/xsel/wl-copy/pbcopy). Non-terminal runs (pipes, services) get a
plain summary instead.

### Going public

```bash
zenshim --tunnel
```

Starts a free cloudflared quick tunnel (no account; `cloudflared` is
auto-downloaded to `~/.local/share/zenshim/` on first use) and prints a public `https://…trycloudflare.com`
URL that routes to your local gateway — use it as the base URL from
anywhere. The gateway key is the only protection on that URL; rotate
it with `--gateway-key`. For your own domain, bind on LAN and put your
reverse proxy in front: `zenshim --host 0.0.0.0 --port 8480`.

### CLI

```
zenshim                 interactive start (wizard on first run)
zenshim --setup         re-run the wizard
zenshim --no-wizard     non-interactive: flags/env only
zenshim --no-tui        disable the interactive console
  --keys k1,k2          upstream keys ("public" = anonymous)
  --models a,b          models to serve (default: all -free)
  --all-models          serve the full live catalog
  --gateway-key KEY     what clients authenticate with
  --port 8480           listen port
  --host 127.0.0.1      bind address (0.0.0.0 = LAN)
  --tunnel              expose publicly via a cloudflared quick tunnel
  --format both         API shapes to serve: openai | anthropic | both
```

## How it works

Every outgoing request is reshaped to what the Zen gateway validates:
official client `User-Agent`, no SDK fingerprint headers, freshly minted
OpenCode SortableID session ids, a verbatim OpenCode prompt as the
leading developer message (defused by a precedence preamble), the whole
conversation consolidated into a single user transcript with `input_image`
blocks carried for the latest image turn, no native `tools`, `stream: true`.

Function calling runs as a text protocol on the wire: the client's tool
definitions are described in the system prompt, the model emits fenced
` ```tool ` blocks, and the gateway parses them out of the stream and
re-emits them as standard OpenAI `tool_calls` (in streaming chunks too,
with `finish_reason: "tool_calls"`). Tool results sent back as `role:"tool"`
messages are folded into the next request's transcript.

The full gate analysis — every rule, what fails, why — lives in
[pi-opencode-zen/docs/zen-gate-notes.md](https://github.com/rabden/pi-opencode-zen/blob/main/docs/zen-gate-notes.md),
the Pi extension built on the same findings.

## Limits

- Marker prompt and UA are pinned to OpenCode CLI 1.19.0; if Zen rotates
  them the gateway 403s until re-captured (see the notes doc).
- Free-tier quota is per IP and enforced by OpenCode; stacking keys helps
  with key-scoped limits, not IP-scoped ones.
- Images from only the most recent turn are carried on the wire.
- Occasionally the model's answer tastes like a title (marker persona
  residue) — retry the request.

## Disclaimer

Unofficial, not affiliated with OpenCode. Free-tier access outside the
official client may conflict with OpenCode's terms — review them before
use. May break at any time as the service evolves.

## License

MIT
