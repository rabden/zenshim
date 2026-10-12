# The app

Started in a terminal, zenshim opens a full-screen interface (built on
[ratatui](https://ratatui.rs)). It needs a window of at least 76×16
characters. Started without a terminal, or with `--no-tui`, it prints a
short summary and log lines instead.

The top line shows the version, the tabs, the listening address, the
number of requests in flight and the uptime. The bottom line shows the
keys that apply to the current tab, and briefly the result of your last
action.

## Keys that work everywhere

| Key | Action |
|---|---|
| `Tab`, `←` `→`, `1`–`5` | switch tabs |
| `k` | copy the gateway key to the clipboard |
| `K` | show or hide the gateway key on screen |
| `u` | copy the OpenAI base URL (`http://127.0.0.1:8480/v1`) |
| `U` | copy the Anthropic base URL (`http://127.0.0.1:8480`) |
| `t` | copy the tunnel URL, when a tunnel is up |
| `T` | start the tunnel, or stop it (asks first) |
| `R` | generate a new gateway key (asks first) |
| `s` / `m` | jump to the Dashboard / the Models tab |
| `?` or `F1` | show all keys |
| `q` or `Ctrl-C` | shut down |

In lists: `↑` `↓` move, `PgUp` `PgDn` page, `g` / `G` jump to the first
or last row.

Copying uses `wl-copy`, `xclip`, `xsel`, `pbcopy` or `clip.exe`,
whichever is installed. The gateway key is never printed unless you
press `K`.

## 1 Dashboard

- **Gateway**: the two base URLs, the tunnel state, the gateway key
  (masked), how many models and keys are in use, and the **Backend**
  line saying which route requests are taking right now.
- **Traffic**: totals for requests, requests in flight and errors,
  tokens in and out, average time of successful requests, and a
  requests-per-second graph of the last few minutes.
- **Recent requests**: the latest requests, newest first. Before the
  first request arrives, this shows a ready-to-paste `curl` example.

## 2 Requests

Every request, including ones refused for a bad key or unknown model.

| Column | Meaning |
|---|---|
| status | HTTP status; `live` while running; `gone` if the client disconnected |
| dialect | `openai`, `responses` or `anthropic` |
| client | `local`, or the caller's address when it came through the tunnel or a proxy |
| via | `direct` or `cli` |
| tokens | input → output, as reported by Zen |
| time / age | how long it took / how long ago it started |

The selected request is shown in full beside the list: the error or
finish reason, message and tool counts, which upstream key and route
served it, and any tool calls.

| Key | Action |
|---|---|
| `f` | filter: all → errors → active |
| `x` | clear finished requests |

The top row follows the newest request until you move the selection.
The last 500 requests are kept.

## 3 Models

The models you serve, with requests and output tokens per model.

| Key | Action |
|---|---|
| `Space` | start or stop serving the selected model |
| `Enter` or `c` | copy its id |
| `a` | copy all served ids, comma-separated |
| `/` | search; `Enter` keeps the filter, `Esc` clears it |
| `f` | switch between free-tier models and the whole catalog |
| `r` | fetch the catalog again |

At least one model always stays served. Changes take effect on the next
request and are saved.

## 4 Keys

Upstream keys, masked, with one column per route showing `ready`, the
number of requests served, or how long the route still rests; then the
count of quota/rate-limit responses and other failures, and the last
error. The gateway key is shown below.

| Key | Action |
|---|---|
| `a` | add a key (or several, comma-separated); each is validated first |
| `d` | remove the selected key (asks first; the last key stays) |
| `v` | validate the selected key against Zen |
| `r` | end the selected key's rest now |
| `R` | generate a new gateway key |

See [Keys, routes and fallback](routing.md) for what the route columns
mean.

## 5 Logs

Start-up messages, tunnel and CLI backend events, failed attempts and
failed requests. `↑` `↓` and `PgUp` `PgDn` scroll; `G` returns to the
newest line. The last 1000 lines are kept.

## What is saved

Changing the served models, adding or removing upstream keys, and
rotating the gateway key all apply immediately and are written to the
config file. A change saves only the field you changed, so a one-off
`--port` or `--gateway-key` on the command line is not written along
with it.
