# Changelog

## Unreleased

### Fixed
- Tool-call blocks whose JSON contains triple backticks (bash heredocs
  writing markdown, echoed code fences) are no longer truncated at the
  first ``` — the block end is now found by scanning for the first
  balanced JSON object with string/escape awareness, then the closing
  fence. Previously such calls were lost and the turn silently ended
  mid-task.
- An unparseable fenced block is now kept as visible text instead of
  being dropped, so the model can see its own malformed call and
  self-correct.
- Text that follows a closed tool block now streams live instead of
  being held back until the end of the response.

### Added
- Reasoning/thinking summaries from the direct route are forwarded to
  clients instead of dropped: Anthropic `thinking` blocks, Responses
  `reasoning` items, and DeepSeek-style `reasoning_content` for the
  OpenAI chat dialect, in streams and final objects alike.

## 0.5.0

### Added
- **CLI route**: an automatic fallback through a sandboxed
  `opencode serve` (OpenCode 2 or newer) when the direct route is
  exhausted or refused. `--backend auto|direct|cli`, `--opencode`.
- **Key scheduling**: an exhausted route rests for 24 hours (or
  upstream's `Retry-After`) and comes back by itself; rests survive
  restarts. `r` on the Keys tab ends a rest.
- **OpenAI Responses API**: `POST /v1/responses` with streaming,
  function tools, `previous_response_id` and `store`;
  `GET` / `DELETE /v1/responses/{id}`.
- **File input**: PDFs and other files, audio and video in all
  dialects; text files are inlined.
- **Limits**: per-client rate limit, concurrency cap with a bounded
  queue, request and idle timeouts, configurable body size.
- **`GET /metrics`** (JSON or Prometheus text) and **`GET /ready`**.
- **Service use**: an environment variable for every setting,
  `--show-key`, clean shutdown on `SIGTERM`, a `Dockerfile`, a Compose
  file, a systemd unit, and a container image published with releases.
- A test suite, and CI running it on Linux, macOS and Windows.
- Documentation under [`docs/`](docs/).

### Changed
- The config and state files are written readable only by their owner,
  and atomically.
- A config built from flags alone is now saved, so the generated
  gateway key no longer changes on every start.
- Errors from upstream are returned in the dialect that was called.
- The Requests tab shows which route served each request; the Keys tab
  shows each key's state per route.

### Fixed
- A request could crash when streamed text ended in a non-ASCII
  character.
- Non-streaming Chat Completions returned tool calls without the
  `function` wrapper.
- Text following a tool call was duplicated in non-streaming answers.
- Characters split across network chunks could be corrupted.
- The table of remembered conversations grew without bound.

## 0.4.0

- Full-screen terminal app with Dashboard, Requests, Models, Keys and
  Logs tabs, replacing the line console.
- Models, upstream keys and the gateway key can be changed while
  running.
- Setup runs inside the app.
- The tunnel can be started and stopped at runtime.

## 0.3.0

- Anthropic Messages dialect (`/v1/messages`); `--format`.

## 0.2.x

- Public tunnel through cloudflared, downloaded automatically.
- Interactive console with clipboard copy.

## 0.1.0

- First release: OpenAI Chat Completions gateway with function calling
  and image input.
