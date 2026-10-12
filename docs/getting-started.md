# Getting started

## Install

**Linux and macOS**

```bash
curl -fsSL https://raw.githubusercontent.com/rabden/zenshim/main/install.sh | sh
```

The script downloads the latest release for your platform and installs
it to `/usr/local/bin` if that is writable, otherwise `~/.local/bin`.
Pass `--dest <dir>` to choose: `sh install.sh --dest ~/bin`.

**Windows**: download `zenshim-x86_64-pc-windows-msvc.zip` from the
[releases page](https://github.com/rabden/zenshim/releases) and unzip it.

**From source** (Rust 1.85 or newer):

```bash
git clone https://github.com/rabden/zenshim.git
cd zenshim
cargo build --release      # target/release/zenshim
```

**Docker**: see [Deployment](deployment.md#docker).

## First run

```bash
zenshim
```

A three-step setup opens:

1. **Keys.** Paste your Zen API keys, comma-separated, or leave the
   field blank to use the anonymous free tier.
2. **Validate.** Each key is checked against Zen's live model catalog.
   Keys that fail are dropped; if none work you are returned to step 1.
3. **Models.** Pick the models to serve. Every `-free` model is
   preselected. `Space` toggles one, `a` toggles all, `f` switches
   between free models and the whole catalog, `Enter` saves.

Setup writes `~/.config/zenshim/config.json` (see
[Configuration](configuration.md#files)) and generates a **gateway
key**, the key your clients authenticate with. Then the dashboard
opens. Later runs go straight to the dashboard; `zenshim --setup` runs
setup again.

Two values connect a client:

| | Where to get it | Looks like |
|---|---|---|
| Base URL | press `u` in the app | `http://127.0.0.1:8480/v1` |
| Gateway key | press `k` in the app, or run `zenshim --show-key` | `sk-zenshim-…` |

Model ids are listed on the Models tab (`Enter` copies one) and at
`GET /v1/models`.

## Connect a client

### OpenAI SDKs and tools

Anything that accepts an OpenAI base URL and API key works. Most read
two environment variables:

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8480/v1
export OPENAI_API_KEY=sk-zenshim-…
```

Python, Chat Completions:

```python
from openai import OpenAI
client = OpenAI()
reply = client.chat.completions.create(
    model="muse-spark-1.3-contributor-free",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(reply.choices[0].message.content)
```

JavaScript, Responses API with streaming:

```js
import OpenAI from "openai";
const client = new OpenAI();
const stream = await client.responses.create({
  model: "muse-spark-1.3-contributor-free",
  input: "Write a haiku about gates.",
  stream: true,
});
for await (const event of stream) {
  if (event.type === "response.output_text.delta") process.stdout.write(event.delta);
}
```

### Anthropic SDKs and tools

The base URL has no `/v1`; the SDK adds the path itself.

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8480
export ANTHROPIC_API_KEY=sk-zenshim-…
```

```python
from anthropic import Anthropic
client = Anthropic()
message = client.messages.create(
    model="muse-spark-1.3-contributor-free",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}],
)
print(message.content[0].text)
```

### curl

```bash
curl http://127.0.0.1:8480/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "muse-spark-1.3-contributor-free",
       "messages": [{"role": "user", "content": "Hello!"}]}'
```

### Function calling

Send tools the way your dialect normally does; calls come back in that
dialect's standard shape.

```python
reply = client.chat.completions.create(
    model="muse-spark-1.3-contributor-free",
    messages=[{"role": "user", "content": "What is the weather in Paris?"}],
    tools=[{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get current weather for a city",
            "parameters": {"type": "object",
                           "properties": {"city": {"type": "string"}},
                           "required": ["city"]},
        },
    }],
)
call = reply.choices[0].message.tool_calls[0]
print(call.function.name, call.function.arguments)   # get_weather {"city":"Paris"}
```

Send the result back as a `role: "tool"` message with the call's id, as
with OpenAI itself.

### Images and files

```python
reply = client.chat.completions.create(
    model="muse-spark-1.3-contributor-free",
    messages=[{"role": "user", "content": [
        {"type": "text", "text": "What does this PDF say?"},
        {"type": "file", "file": {"filename": "report.pdf",
                                  "file_data": "data:application/pdf;base64,…"}},
    ]}],
)
```

The [API reference](api.md#inputs) lists every input kind per dialect.

## Next steps

- Use it from other machines: [Deployment](deployment.md#public-tunnel).
- Add more keys and understand the fallback: [Keys, routes and fallback](routing.md).
- Run it as a background service: [Deployment](deployment.md).
