# Troubleshooting

The **Logs** tab (or the service's output) and the detail pane of the
**Requests** tab usually say what went wrong. The table below maps what
a client sees to its cause.

## Errors a client receives

| Client sees | Cause | What to do |
|---|---|---|
| `401 invalid gateway key` | The key sent is not the gateway key. | Press `k` in the app, or run `zenshim --show-key`, and use that. Upstream Zen keys are not accepted here. |
| `404 model '…' is not served` | The model is not in your served list. | Use an id from `GET /v1/models`, or serve the model on the Models tab. |
| `404 … format not served` | That dialect is switched off. | Start without `--format`, or with the right one. |
| `404` on `/chat/completions` | The base URL is missing `/v1`. | OpenAI clients need `http://127.0.0.1:8480/v1`. |
| `404` on `/v1/v1/messages` | The base URL has `/v1` and the SDK added it again. | Anthropic clients need `http://127.0.0.1:8480`. |
| `413` | The request is larger than the body limit. | Raise `--max-body-mb`, or send a smaller file. |
| `429 rate limit of … reached` | A remote client passed the per-minute limit. | Wait for `Retry-After`, or raise `--rate-limit` (`0` turns it off). |
| `429 the gateway is at capacity` | All generation slots and the queue are full. | Raise `--max-concurrent` / `--max-queue`, or send less at once. |
| `429 all upstream keys are exhausted` | Every key's quota is used up on every route. | Wait, add a key on the Keys tab, or press `r` on a key if you believe it is usable. |
| `502 zen returned 403 …` | Zen's gate refused the direct route and there is no CLI to fall back to. | Install OpenCode 2 or newer, or update zenshim. |
| `502` with another message | Zen failed or was unreachable on every route. | Check your connection and the Logs tab; retry. |
| `504` | The model took longer than `--request-timeout`. | Retry, raise the timeout, or use streaming. |
| `400 previous response '…' was not found` | Stored responses last one hour and do not survive a restart. | Send the conversation again in `input`. |

## Things that look wrong

**The answer is a short title instead of a reply.**
Occasionally the model follows the marker prompt on the direct route.
Retry the request.

**The answer is empty, or cut off with `finish_reason: "length"`.**
Reasoning models use output tokens to think. Raise `max_tokens`.

**The model answered in prose instead of calling my tool.**
Function calling is emulated through the prompt and a model can ignore
it. Retry, or say explicitly in the prompt that the tool must be used.

**The model did not understand my audio or video.**
Files are passed through, but understanding them is up to the model.

**A key shows `rests 23h` though I think it is fine.**
Zen answered `402` or `429` for it. Press `r` on the Keys tab to use it
again now.

**The dashboard says "direct wire refused — using the OpenCode CLI".**
Zen changed what its gate accepts. Requests are going through the CLI
route and direct is retried every 30 minutes. Nothing to do; check for a
zenshim update.

**Requests became slower and report about 6.5k more input tokens.**
They are going through the CLI route (the `via` column shows `cli`),
because direct is resting or refused.

**Everyone using my Docker container gets rate-limited together.**
They all arrive from Docker's bridge address. Set
`ZENSHIM_RATE_LIMIT=0` if only you can reach the port.

## Start-up problems

| Message | Cause | What to do |
|---|---|---|
| `cannot listen on 127.0.0.1:8480: Address already in use` | Another program, or another zenshim, has the port. | Stop it, or pass `--port`. |
| `no models available; pass --models or run interactively` | The catalog could not be fetched and no models were given. | Check your connection, or pass `--models`. |
| `the OpenCode CLI was not found` | `--backend cli` or `--opencode` was given but no CLI exists there. | Install OpenCode, or correct the path. |
| `no config at … yet` | `--show-key` was run before the first start. | Start zenshim once. |
| "zenshim needs at least 76×16" | The terminal window is too small for the app. | Enlarge it, or use `--no-tui`. |
| Setup: "No usable keys" | Zen rejected every key entered. | Check the keys, or leave the field blank for the anonymous tier. |

## The CLI route

| Log line | Meaning |
|---|---|
| `cli: unsupported: this OpenCode has no v2 server API` | The installed OpenCode is older than 2. The CLI route is switched off for this run; upgrade OpenCode. |
| `cli: OpenCode did not start listening` | The CLI did not come up within a minute. Run `opencode serve` by hand to see why. |
| `cli: the OpenCode backend exited; it restarts on the next request` | The backend process died. It is started again when next needed. |
| `cli: could not refuse an OpenCode tool request` | A refusal could not be delivered. The tool is still not approved; the request will time out rather than run it. |

## The tunnel

| Dashboard shows | What to do |
|---|---|
| `cloudflared unavailable` | The download failed, or you are on an Apple-silicon Mac. Install `cloudflared` yourself. |
| `no URL received within 20s` | Cloudflare did not issue an address. Press `T` to try again. |
| `cloudflared exited` | The tunnel process stopped. Press `T` to start a new one; the address will be different. |

## Copying does nothing

Copy keys need one of `wl-copy`, `xclip`, `xsel`, `pbcopy` or `clip.exe`.
Without one, the app tells you; press `K` to show the key and copy it by
hand.
