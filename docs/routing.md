# Keys, routes and fallback

zenshim decides two things for every request: **which upstream key** to
use, and **which route** to take to Zen. This page explains both.

## Upstream keys

An upstream key is a Zen API key. `public` stands for the anonymous free
tier, which needs no account. Give zenshim one key or several:

- in setup, comma-separated;
- with `--keys k1,k2` or `ZENSHIM_KEYS`;
- while running, on the Keys tab (`a` adds, `d` removes).

Keys are used in rotation: each successful request moves on to the next
key, which spreads load across them.

Upstream keys are not what your clients send. Clients send the
**gateway key**; the upstream keys never leave zenshim.

## The two routes

| | direct | cli |
|---|---|---|
| What it is | zenshim's own imitation of the request OpenCode sends | a real, sandboxed `opencode serve` that zenshim drives |
| Speed | fastest | about a second slower once warm; about two more to start |
| Prompt overhead | under 1k tokens | OpenCode's own prompt, about 6.5k tokens per request |
| If Zen changes its gate | breaks until zenshim is updated | keeps working, because it is the official client |
| `max_tokens` | honoured | not enforced |
| Needs | nothing | OpenCode 2 or newer installed |

`--backend` chooses:

- `auto` (default): direct, with the CLI as fallback;
- `direct`: never use the CLI;
- `cli`: always use the CLI.

zenshim looks for `opencode` on your `PATH`, then in `~/.opencode/bin`;
`--opencode <path>` points at a specific one. If none is found, `auto`
simply runs without a fallback, and the dashboard says so.

## What happens to a request

In `auto` mode, routes are tried in this order until one answers:

1. the current key over **direct**;
2. the **same key through the CLI**;
3. the **next key** over direct, then through the CLI; and so on.

Why it moves on depends on what went wrong:

| Zen says | What zenshim does |
|---|---|
| `402` or `429` (quota or rate limit) | That route of that key **rests**, and the next step is tried. |
| `403` (the gate refused the request shape) | Tries the next step. Three in a row on direct trigger the [gate fallback](#when-the-direct-route-is-refused). |
| `401` (the key is not accepted) | Skips that key's other route and moves to the next key. |
| other `4xx` (the request itself is wrong) | Returns the error to the client; another route would say the same. |
| `5xx` or a network failure | Tries the next step. |

So when a key's direct quota runs out, requests for that key go through
the CLI and **stay** there while direct rests. When the CLI is exhausted
too, the key is skipped entirely until one of its routes is back.

## The 24-hour rest

A route that reports quota or rate limiting is left alone for **24
hours**, then used again. Two refinements:

- If Zen sends a `Retry-After` header (up to a day), the rest lasts that
  long instead. A short rate limit therefore costs seconds, not a day.
- Rests are saved in `state.json` beside the config and survive
  restarts. Keys are identified there by a fingerprint, never stored.

The Keys tab shows each route as `ready`, a served count, or
`rests 23h`. Press `r` on a key to end its rest now.

**If everything is resting**, zenshim does not just refuse. The route
due back soonest is tried anyway, at most once a minute per route. If it
works, the rest is lifted; if not, the client gets `429` with
`Retry-After`. This keeps one mistaken "exhausted" from locking the
gateway out for a day.

## When the direct route is refused

A `403` from Zen means its gate did not accept the request shape, which
happens when OpenCode changes and zenshim's imitation falls behind.
After **three consecutive `403`s** on direct, and if a CLI is available:

- every request goes through the CLI;
- direct is tried again after 30 minutes;
- the first direct success switches everything back.

The dashboard's Backend line shows this state and the time until the
next direct attempt. Without a CLI there is nothing to fall back to, so
direct keeps being tried and its `403` reaches the client as `502`.

## The CLI sandbox

The CLI route runs real OpenCode, which is a coding agent able to run
commands and edit files. zenshim keeps it from doing either:

- **Every tool request is refused.** OpenCode is configured to ask
  permission for every tool, and zenshim answers each request with
  "reject". The model is told to answer directly instead.
- **Nothing of yours is loaded.** Its home, config, data and cache
  directories are a fresh temporary directory, readable only by you. It
  does not see your OpenCode settings, agents, plugins, sessions or
  saved credentials.
- **Minimal environment.** It receives `PATH`, locale, proxy and
  certificate variables, and the one upstream key it serves. No other
  environment variable is passed on.
- **Local and locked.** It listens on `127.0.0.1` only, on a port it
  picks itself, behind a random password generated for that run.
- **One per key, on demand.** A backend starts on the first request that
  needs it, stops after 10 idle minutes, and is killed with zenshim.
  Each request runs in its own session, which is deleted afterwards.

This is isolation by OpenCode's own permission system, not an
operating-system sandbox: it relies on OpenCode honouring "ask" for
every tool. For stronger separation, run zenshim in a container or under
the provided systemd unit ([Deployment](deployment.md)).

## Choosing a setup

- **One anonymous key, OpenCode installed**: the default. Direct until
  the quota runs out, then the CLI for the rest of the day.
- **Several keys**: more total quota; rotation and rests are automatic.
- **Direct only** (`--backend direct`): lowest overhead, no OpenCode
  process ever started; a `403` from the gate is not survivable.
- **CLI only** (`--backend cli`): most durable against gate changes, at
  the cost of latency and about 6.5k prompt tokens per request.
