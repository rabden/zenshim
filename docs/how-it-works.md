# How it works

## The problem

OpenCode Zen serves a set of free models, but only to OpenCode itself.
Its API checks that a request looks like one the OpenCode CLI would
send; anything else gets:

```
403 FreeTierError: "OpenCode's free tier can only be used from within OpenCode"
```

Tools that speak the OpenAI or Anthropic API cannot produce that shape,
and their own shape (native tool definitions, multi-message
conversations, their SDK's headers) fails the check. zenshim translates
between the two.

## One request, start to finish

```
client request
   │  1. check: dialect on? rate limit? gateway key? model served? free slot?
   │  2. translate the request to Zen's shape
   │  3. pick a key and a route; send
   ▼
Zen answers as a stream of events
   │  4. translate each event back into the client's dialect
   ▼
client response (streamed, or collected into one object)
```

### 1. Checks

In order: is this dialect switched on, is the caller within the rate
limit, is the gateway key right, is the model one that is served, and is
there a free generation slot (waiting up to 30 seconds for one). Each
failure is answered in the caller's dialect and recorded, so refused
requests appear in the app too.

### 2. Request translation

Whatever the dialect, the request becomes one Zen "Responses" request
made of three parts:

1. **A marker prompt**, verbatim text that OpenCode itself sends. The
   gate looks for it.
2. **A system message** that tells the model to disregard the marker,
   followed by the client's own system prompt and, when tools were
   offered, their descriptions and a protocol for calling them.
3. **One user message** holding the whole conversation as a transcript:

   ```
   [user] What is the weather in Paris?
   [assistant tool call: get_weather {"city":"Paris"}]
   [tool result for call_1: Sunny, 21°C]
   ```

   plus the attachments of the most recent turn that has any.

The request also carries OpenCode's `User-Agent`, its client headers,
and a session id in OpenCode's format that stays the same for a given
conversation.

Anthropic requests are read directly. Responses-API requests are first
converted to the chat shape, with the conversation behind
`previous_response_id` prepended.

### Function calling

Client tools cannot be declared natively on this path without failing
the gate, so they travel as text. The system message lists each tool's name, description and
parameter schema, and tells the model how to call one:

````
```tool
{"name": "get_weather", "arguments": {"city": "Paris"}}
```
````

As the answer streams back, zenshim watches for these fenced blocks,
holds them out of the visible text, parses them, and emits them as real
tool calls in the client's dialect: `tool_calls`, a `function_call`
item, or a `tool_use` block. A partial fence at the end of a chunk is
held back until it is clear whether it starts a call. A block's end is
found by scanning for the first balanced JSON object — so commands
that themselves contain triple backticks (heredocs writing markdown)
survive intact — and a block that does not parse is kept as visible
text so the model can self-correct. Thinking summaries are forwarded
too: Anthropic `thinking` blocks, Responses `reasoning` items, or
`reasoning_content` in the OpenAI chat dialect. Tool results the
client sends later are written into the next transcript.

### 3. Sending

The key scheduler produces an ordered list of attempts (each key over
the direct route, then through the CLI) and the first that answers wins.
[Keys, routes and fallback](routing.md) describes the policy.

**Direct route.** The translated request is posted to the endpoint the
model lives on — `/responses` for muse and the paid Responses catalog,
`/chat/completions` for the chat-native frees (each model's endpoint is
guessed from its id, confirmed on the first request, and remembered).
The reply is a server-sent event stream in either dialect; a protocol
mismatch (the endpoint changed, or a brand-new model) is retried once
on the other endpoint with the request and stream translated.

**CLI route.** zenshim starts `opencode serve` in a sandbox and drives
it over its local HTTP API:

1. create a session with the requested model;
2. attach the client's system prompt as a session instruction, which is
   OpenCode's system channel;
3. send the conversation (a single message is sent as it is; a longer
   one as the transcript) with any attachments;
4. follow the session's event stream for text as it is written, and
   refuse every tool permission request that comes up;
5. when the session goes idle, read the finished messages and reconcile
   them with what was streamed, so nothing is lost or repeated;
6. delete the session.

Its events are converted into the same vocabulary the direct route
produces, so step 4 below is identical for both.

### 4. Response translation

A single translator consumes Zen's events and renders one of three
dialects. For each text delta it emits the dialect's own streaming
event; at the end it emits tool calls, the finish reason and token
usage. For a non-streaming request the same translator runs to
completion and returns one assembled object.

## The pieces

| File | Responsibility |
|---|---|
| `src/main.rs` | command line, start-up, shutdown |
| `src/server.rs` | HTTP routes, checks, dispatch across routes, metrics |
| `src/translate.rs` | client requests → Zen's shape; attachments; the CLI prompt |
| `src/stream.rs` | Zen's events → Chat Completions, Responses or Anthropic output; tool-call parsing |
| `src/responses.rs` | Responses-API request conversion and the in-memory response store |
| `src/keys.rs` | upstream keys, route planning, the 24-hour rest, the gate fallback |
| `src/bridge.rs` | the CLI route: sandbox, backend lifecycle, sessions, events |
| `src/limits.rs` | rate limiter and concurrency slots |
| `src/hub.rs` | request records, totals and log lines shared with the app |
| `src/tui/` | the terminal app and the setup screens |
| `src/tunnel.rs` | the cloudflared quick tunnel |
| `src/config.rs` | the config file |
| `src/wizard.rs` | the model catalog, and the text-mode setup |

## Tests

```bash
cargo test
```

None of the tests contact Zen or start OpenCode. They cover
request translation for all three dialects, stream translation
(including tool calls split across chunks and non-ASCII text), the
key scheduler and its timing, the CLI route's event handling and
sandbox environment, the limits, the response store, and the HTTP layer
against a stand-in upstream that can be told to succeed, fail, or
report exhaustion per key.

## Background

The analysis of Zen's gate (each rule, what fails and why) is in
[pi-opencode-zen/docs/zen-gate-notes.md](https://github.com/rabden/pi-opencode-zen/blob/main/docs/zen-gate-notes.md),
written for the Pi extension built on the same findings.
