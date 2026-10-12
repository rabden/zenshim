# zenshim

**A local API gateway that lets any OpenAI- or Anthropic-compatible
client use OpenCode Zen's free models.**

Zen's free models (muse-spark, space-bunny, …) reject every client that
is not the official OpenCode CLI:

```
403 FreeTierError: "OpenCode's free tier can only be used from within OpenCode"
```

zenshim sits in between. Your tools talk to it in the dialect they
already speak; it reshapes each request into one Zen accepts and
translates the answer back.

```
your client ──(OpenAI or Anthropic API + gateway key)──▶ zenshim ──▶ OpenCode Zen
```

## What you get

- **Three client dialects at once**: OpenAI Chat Completions, OpenAI
  Responses and Anthropic Messages, streaming and non-streaming.
- **Function calling** in each dialect's own shape, and **image, PDF and
  file input**.
- **Several upstream keys**, rotated automatically; an exhausted key
  rests for 24 hours and comes back by itself.
- **Two routes to Zen**: a fast direct one, and an automatic fallback
  through a sandboxed OpenCode CLI when the direct one is exhausted or
  refused.
- **A full-screen terminal app** showing live requests, models, keys and
  logs, where models and keys can be changed without a restart.
- **One-flag public URL** through a cloudflared quick tunnel.
- **Built to be left running**: rate limiting, concurrency caps,
  timeouts, metrics, Docker and systemd packaging.
- One small binary. No runtime, no account.

## Install

Linux and macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/rabden/zenshim/main/install.sh | sh
```

This puts the latest release in `/usr/local/bin` (or `~/.local/bin`).
On Windows, download `zenshim-x86_64-pc-windows-msvc.zip` from
[releases](https://github.com/rabden/zenshim/releases). From source:
`cargo build --release`.

## Quick start

```bash
zenshim
```

The first run asks for your Zen API keys (leave it blank for the
anonymous free tier), checks them, and lets you pick the models to
serve. Then the dashboard opens. Press `k` to copy the gateway key and
`u` to copy the base URL, and point a client at it:

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8480/v1
export OPENAI_API_KEY=sk-zenshim-…        # the gateway key
```

```python
from openai import OpenAI
client = OpenAI()   # reads the two variables above
reply = client.chat.completions.create(
    model="muse-spark-1.3-contributor-free",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(reply.choices[0].message.content)
```

Anthropic clients use the same port without the `/v1`:

```python
from anthropic import Anthropic
client = Anthropic(base_url="http://127.0.0.1:8480", api_key="sk-zenshim-…")
```

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | install, first run, connecting SDKs and tools |
| [The app](docs/app.md) | the terminal interface, tab by tab |
| [API reference](docs/api.md) | routes, inputs, function calling, errors, what is not supported |
| [Keys, routes and fallback](docs/routing.md) | key rotation, the 24-hour rest, the CLI fallback and its sandbox |
| [Configuration](docs/configuration.md) | every flag and environment variable, files, limits |
| [Deployment](docs/deployment.md) | public tunnel, Docker, systemd, reverse proxy, metrics |
| [How it works](docs/how-it-works.md) | the translation, in detail, and a map of the code |
| [Troubleshooting](docs/troubleshooting.md) | symptoms and fixes |
| [Changelog](CHANGELOG.md) | what changed in each release |

## Known limits

- The direct route imitates OpenCode CLI 1.19.0. If Zen changes what its
  gate accepts, direct requests get `403` until the recipe is updated;
  with OpenCode 2 or newer installed, requests move to the CLI route by
  themselves in the meantime.
- The free tier's quota is enforced by OpenCode, partly per IP address.
  More keys help with per-key limits, not per-IP ones.
- Attachments from only the most recent turn that has any are sent.
- Now and then an answer over the direct route reads like a title
  rather than a reply. Retry the request.
- The CLI route cannot cap output length, so `max_tokens` is not
  enforced there.

## Disclaimer

Unofficial and not affiliated with OpenCode. Using the free tier from
outside the official client may conflict with OpenCode's terms; read
them before you rely on this. It can stop working at any time.

## License

MIT
