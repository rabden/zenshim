# API reference

All routes are served on one port (default `8480`). By default every
dialect is on; `--format openai` serves only the OpenAI routes and
`--format anthropic` only the Anthropic one, the others answering `404`.

## Authentication

Send the gateway key either way:

```
Authorization: Bearer sk-zenshim-…
x-api-key: sk-zenshim-…
```

Both work on every route. `/health` and `/ready` need no key. If the
gateway key is set to an empty string, no key is required at all.

## Routes

| Route | Dialect | Notes |
|---|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions | streaming and non-streaming |
| `POST /v1/responses` | OpenAI Responses | streaming and non-streaming |
| `GET /v1/responses/{id}` | OpenAI Responses | read a stored response |
| `DELETE /v1/responses/{id}` | OpenAI Responses | forget a stored response |
| `POST /v1/messages` | Anthropic Messages | streaming and non-streaming |
| `GET /v1/models` | OpenAI | the models you chose to serve |
| `GET /metrics` | — | counters; see [Deployment](deployment.md#metrics) |
| `GET /health` | — | `ok` |
| `GET /ready` | — | `ready` |

### Models

Use an id from `GET /v1/models`. A leading `opencode/` or
`opencode-zen/` is ignored, and an id that ends with a served model's id
(or the other way round) resolves to it, so prefixed ids from other
tools usually work unchanged. An id that matches nothing gets `404` with
the list of served models.

### Chat Completions

Understood request fields: `model`, `messages`, `stream`, `tools`,
`max_tokens`.

- **Roles**: `system` and `developer` (leading ones become the system
  prompt), `user`, `assistant` (with `tool_calls`), `tool`.
- **Streaming** sends standard `chat.completion.chunk` events, a final
  chunk with `usage`, then `data: [DONE]`.
- **`finish_reason`** is `stop`, `tool_calls` or `length`.

Other fields (`temperature`, `top_p`, `n`, `stop`, `response_format`, …)
are accepted and ignored.

### Responses

Understood request fields: `model`, `input`, `instructions`, `stream`,
`tools`, `max_output_tokens`, `previous_response_id`, `store`. The
response object also echoes `metadata`, `tool_choice`,
`parallel_tool_calls`, `temperature` and `top_p` back.

- **`input`** is a string or a list of items: messages (with or without
  `"type": "message"`), `function_call` and `function_call_output`.
  Other item types, such as `reasoning`, are skipped.
- **Streaming** follows the standard sequence, each event numbered with
  `sequence_number`: `response.created`, `response.in_progress`,
  `response.output_item.added`, `response.content_part.added`,
  `response.output_text.delta` …, `response.output_text.done`,
  `response.content_part.done`, `response.output_item.done`, then for
  each function call `response.output_item.added`,
  `response.function_call_arguments.delta`,
  `response.function_call_arguments.done`, `response.output_item.done`,
  and finally `response.completed` (or `response.incomplete`) carrying
  the whole response object.
- **Stored responses.** Unless `store` is `false`, a finished response
  is kept in memory for one hour (500 at most, oldest dropped first).
  `previous_response_id` continues from a stored response: its
  conversation is replayed, then your new `input` is added. As with
  OpenAI, `instructions` are not inherited. An unknown id gets `400`.
  Stored responses are lost when zenshim restarts.

### Anthropic Messages

Understood request fields: `model`, `messages`, `system` (string or text
blocks), `stream`, `tools`, `max_tokens`.

- **Content blocks**: `text`, `image`, `document`, `tool_use`,
  `tool_result`.
- **Streaming** sends `message_start`, `content_block_start`,
  `content_block_delta`, `content_block_stop`, `message_delta`,
  `message_stop`.
- **`stop_reason`** is `end_turn`, `tool_use` or `max_tokens`.

### Output length

Reasoning models spend output tokens on thinking before they write, so a
small budget can produce an empty answer. zenshim raises any `max_tokens`
below 1024 to 1024, and reports real truncation as
`finish_reason: "length"`, `stop_reason: "max_tokens"` or
`status: "incomplete"`. On the CLI route the limit is not enforced at
all (see [Keys, routes and fallback](routing.md#the-two-routes)).

## Inputs

Each dialect uses its own part shapes:

| Kind | Chat Completions | Responses | Anthropic |
|---|---|---|---|
| Text | `text` | `input_text` | `text` |
| Image | `image_url` | `input_image` | `image` (base64 or URL source) |
| PDF or other file | `file` with `file_data` | `input_file` with `file_data` or `file_url` | `document` (base64, text or URL source) |
| Audio | `input_audio` | `input_file` | — |
| Video | `video_url` | `input_file` | — |

- **Files are data URIs** (`data:<mime>;base64,…`) or `https` URLs.
  zenshim never downloads a URL itself; it passes it on.
- **Text files are inlined.** Anything with a `text/*` type, and JSON,
  XML, YAML, TOML or CSV, is decoded and placed in the conversation, so
  every model can read it.
- **Everything else is sent as a file.** Whether a model can use it is
  up to the model: PDFs and images work with the models tested; audio
  was accepted but not understood.
- **Only the most recent turn's attachments are sent.** Earlier turns
  keep a note that something was attached.
- Request bodies are limited to 32 MB by default (`--max-body-mb`).

## Function calling

Declare tools the usual way for your dialect:

| | Declare | The model's call | Your result |
|---|---|---|---|
| Chat Completions | `tools: [{type: "function", function: {…}}]` | `message.tool_calls[]` | a `role: "tool"` message with `tool_call_id` |
| Responses | `tools: [{type: "function", name, parameters}]` | a `function_call` output item | a `function_call_output` input item with `call_id` |
| Anthropic | `tools: [{name, input_schema}]` | a `tool_use` content block | a `tool_result` block with `tool_use_id` |

A model may make several calls in one turn. Zen does not expose native
function calling on this path, so calls are emulated through the prompt
(see [How it works](how-it-works.md#function-calling)); it is reliable
with the models tested but a model can still answer in prose instead.
`tool_choice` is not enforced.

## Errors

Errors come in the shape of the dialect that was called.

OpenAI routes:

```json
{"error": {"message": "…", "type": "invalid_request_error", "code": null}}
```

Anthropic route:

```json
{"type": "error", "error": {"type": "rate_limit_error", "message": "…"}}
```

| Status | Meaning |
|---|---|
| `400` | The body is not valid JSON, a Responses `input` is empty or `previous_response_id` is unknown, or Zen rejected the request itself (for example, too long for the model). |
| `401` | Missing or wrong gateway key. |
| `404` | The model is not served, the dialect is switched off, or a stored response does not exist. |
| `413` | The body is larger than `--max-body-mb`. |
| `429` | Rate limit reached, the gateway is at capacity, or every upstream key is exhausted. Carries `Retry-After`. |
| `502` | Zen failed or refused, and no other route worked. |
| `504` | The model took longer than `--request-timeout` (non-streaming requests). |

A streaming response that fails after it has started cannot change its
status code; it ends early instead, with the truncation reported in the
final event.

## Not supported

- Hosted tools such as `web_search` (ignored).
- `file_id` references; there is no Files API.
- Embeddings, legacy text completions, audio output, image generation.
- `background` responses and the Conversations API.
- Sampling parameters (`temperature`, `top_p`, …), `stop`, `n`,
  `response_format` and `tool_choice` are accepted but have no effect.
