use serde_json::{json, Value};

/// Which client-facing API shape a stream/request renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    OpenAi,
    Anthropic,
}

/// Core accumulator over a Zen Responses-API SSE stream. Renders either
/// OpenAI chat.completion chunks or Anthropic Messages events, and
/// synthesizes tool calls from fenced ```tool blocks in the text.
pub struct StreamTranslator {
    format: ApiFormat,
    model: String,
    id: String,
    created: u64,
    buffer: String,
    clean_text: String,
    flushed_text: String,
    emitted_tool_calls: Vec<Value>,
    usage: Option<Value>,
    truncated: bool,
    finished: bool,
    tool_call_index: usize,
    anthropic_message_started: bool,
    anthropic_text_block_open: bool,
    anthropic_block_index: usize,
}

const FENCE_OPEN: &str = "```tool";

impl StreamTranslator {
    pub fn new(format: ApiFormat, model: String) -> Self {
        Self {
            format,
            model,
            id: match format {
                ApiFormat::OpenAi => format!(
                    "chatcmpl-zenshim-{:x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0)
                ),
                ApiFormat::Anthropic => format!(
                    "msg_zenshim-{:x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0)
                ),
            },
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            buffer: String::new(),
            clean_text: String::new(),
            flushed_text: String::new(),
            emitted_tool_calls: Vec::new(),
            usage: None,
            truncated: false,
            finished: false,
            tool_call_index: 0,
            anthropic_message_started: false,
            anthropic_text_block_open: false,
            anthropic_block_index: 0,
        }
    }

    // ------------------------------------------------------------------
    // OpenAI rendering
    // ------------------------------------------------------------------

    fn openai_chunk(&self, delta: Value, finish_reason: Option<&str>) -> String {
        let mut choice = json!({ "index": 0, "delta": delta });
        if let Some(reason) = finish_reason {
            choice["finish_reason"] = json!(reason);
        }
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [choice],
        });
        format!("data: {chunk}\n\n")
    }

    // ------------------------------------------------------------------
    // Anthropic rendering
    // ------------------------------------------------------------------

    fn anthropic_event(&self, event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    fn anthropic_ensure_started(&mut self, out: &mut Vec<String>) {
        if self.anthropic_message_started {
            return;
        }
        self.anthropic_message_started = true;
        out.push(self.anthropic_event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": self.usage_input(), "output_tokens": 0 },
                },
            }),
        ));
    }

    fn usage_input(&self) -> u64 {
        self.usage
            .as_ref()
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    fn usage_output(&self) -> u64 {
        self.usage
            .as_ref()
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    fn anthropic_open_text_block(&mut self, out: &mut Vec<String>) {
        self.anthropic_ensure_started(out);
        if self.anthropic_text_block_open {
            return;
        }
        self.anthropic_text_block_open = true;
        out.push(self.anthropic_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": self.anthropic_block_index,
                "content_block": { "type": "text", "text": "" },
            }),
        ));
    }

    fn anthropic_close_text_block(&mut self, out: &mut Vec<String>) {
        if !self.anthropic_text_block_open {
            return;
        }
        self.anthropic_text_block_open = false;
        out.push(self.anthropic_event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": self.anthropic_block_index }),
        ));
        self.anthropic_block_index += 1;
    }

    // ------------------------------------------------------------------
    // Fence holdback (format-agnostic)
    // ------------------------------------------------------------------

    /// How much of the buffer is safe to flush (cannot be part of a fence).
    fn safe_len(&self) -> usize {
        // Anything from the last fence opener onward stays buffered until
        // finish(), so complete blocks are parsed (not leaked as content)
        // and incomplete blocks are never partially flushed.
        if let Some(pos) = self.buffer.rfind(FENCE_OPEN) {
            return pos;
        }
        // Partial fence opener at the tail: hold the partial prefix.
        let bytes = self.buffer.as_bytes();
        for hold in (1..=FENCE_OPEN.len()).rev() {
            if bytes.len() >= hold && &self.buffer[bytes.len() - hold..] == &FENCE_OPEN[..hold] {
                return bytes.len() - hold;
            }
        }
        self.buffer.len()
    }

    /// Emit one flushed text delta in the active format.
    fn emit_text(&mut self, text: &str, out: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        self.flushed_text.push_str(text);
        match self.format {
            ApiFormat::OpenAi => out.push(self.openai_chunk(json!({ "content": text }), None)),
            ApiFormat::Anthropic => {
                self.anthropic_open_text_block(out);
                out.push(self.anthropic_event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": self.anthropic_block_index,
                        "delta": { "type": "text_delta", "text": text },
                    }),
                ));
            }
        }
    }

    /// Feed one zen SSE event; returns rendered SSE strings to emit.
    pub fn feed(&mut self, event: &str, data: &str) -> Vec<String> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return out,
        };
        match event {
            "response.output_text.delta" => {
                if let Some(delta) = parsed.get("delta").and_then(Value::as_str) {
                    self.buffer.push_str(delta);
                    let safe = self.safe_len();
                    if safe > 0 {
                        let flushed: String = self.buffer.drain(..safe).collect();
                        self.emit_text(&flushed, &mut out);
                    }
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if event != "response.completed" {
                    self.truncated = true;
                }
                if let Some(u) = parsed.pointer("/response/usage") {
                    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    self.usage = Some(json!({
                        "prompt_tokens": input,
                        "completion_tokens": output,
                        "total_tokens": input + output,
                    }));
                }
                out.extend(self.finish());
            }
            _ => {}
        }
        out
    }

    /// Flush everything: remaining text, parsed tool calls, finish reason.
    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let text = self.buffer.clone();
        let mut rest = text.as_str();
        let mut clean_text = String::new();
        while let Some(start) = rest.find(FENCE_OPEN) {
            clean_text.push_str(&rest[..start]);
            let after_open = &rest[start + FENCE_OPEN.len()..];
            match after_open.find("```") {
                Some(end) => {
                    let payload = &after_open[..end];
                    if let Some(call) = parse_tool_block(payload) {
                        self.emitted_tool_calls.push(call);
                    }
                    rest = &after_open[end + 3..];
                }
                None => {
                    rest = "";
                }
            }
        }
        clean_text.push_str(rest);
        let clean_text = clean_text.trim().to_string();
        self.clean_text = clean_text.clone();
        self.emit_text(&clean_text, &mut out);
        if self.format == ApiFormat::Anthropic {
            self.anthropic_close_text_block(&mut out);
        }

        // Emit synthesized tool calls.
        let mut any = false;
        for call in self.emitted_tool_calls.clone() {
            any = true;
            match self.format {
                ApiFormat::OpenAi => {
                    out.push(self.openai_chunk(
                        json!({ "tool_calls": [{
                            "index": self.tool_call_index,
                            "id": call["id"],
                            "type": "function",
                            "function": { "name": call["name"], "arguments": call["arguments"] },
                        }]}),
                        None,
                    ));
                }
                ApiFormat::Anthropic => {
                    self.anthropic_ensure_started(&mut out);
                    let index = self.anthropic_block_index;
                    let id = call["id"].as_str().unwrap_or("call").to_string();
                    let name = call["name"].as_str().unwrap_or("?").to_string();
                    out.push(self.anthropic_event(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
                        }),
                    ));
                    out.push(self.anthropic_event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": { "type": "input_json_delta", "partial_json": call["arguments"] },
                        }),
                    ));
                    out.push(self.anthropic_event(
                        "content_block_stop",
                        json!({ "type": "content_block_stop", "index": index }),
                    ));
                    self.anthropic_block_index += 1;
                }
            }
            self.tool_call_index += 1;
        }

        let reason = if self.truncated {
            "length"
        } else if any {
            "tool_calls"
        } else {
            "stop"
        };
        match self.format {
            ApiFormat::OpenAi => {
                out.push(self.openai_chunk(json!({}), Some(reason)));
                if let Some(usage) = self.usage.clone() {
                    let chunk = json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [],
                        "usage": usage,
                    });
                    out.push(format!("data: {chunk}\n\n"));
                }
                out.push("data: [DONE]\n\n".to_string());
            }
            ApiFormat::Anthropic => {
                self.anthropic_ensure_started(&mut out);
                let stop_reason = if self.truncated {
                    "max_tokens"
                } else if any {
                    "tool_use"
                } else {
                    "end_turn"
                };
                out.push(self.anthropic_event(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                        "usage": { "output_tokens": self.usage_output() },
                    }),
                ));
                out.push(self.anthropic_event("message_stop", json!({ "type": "message_stop" })));
            }
        }
        self.finished = true;
        out
    }

    /// Flush and finish if the stream ended without a completion event.
    pub fn close(&mut self) -> Vec<String> {
        if self.finished {
            Vec::new()
        } else {
            self.finish()
        }
    }

    /// Final assembled message for non-streaming responses.
    pub fn into_completion(mut self) -> Value {
        if !self.finished {
            self.close();
        }
        let content = format!("{}{}", self.flushed_text, self.clean_text).trim().to_string();
        match self.format {
            ApiFormat::OpenAi => {
                let mut message =
                    json!({ "role": "assistant", "content": if content.is_empty() { Value::Null } else { json!(content) } });
                if !self.emitted_tool_calls.is_empty() {
                    message["tool_calls"] = json!(self.emitted_tool_calls);
                }
                json!({
                    "id": self.id,
                    "object": "chat.completion",
                    "created": self.created,
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": if self.truncated { "length" } else if self.emitted_tool_calls.is_empty() { "stop" } else { "tool_calls" },
                    }],
                    "usage": self.usage.clone().unwrap_or(json!({
                        "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
                    })),
                })
            }
            ApiFormat::Anthropic => {
                let mut blocks: Vec<Value> = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": content }));
                }
                for call in &self.emitted_tool_calls {
                    let input: Value = serde_json::from_str(
                        call["arguments"].as_str().unwrap_or("{}"),
                    )
                    .unwrap_or(json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"],
                        "name": call["name"],
                        "input": input,
                    }));
                }
                json!({
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": blocks,
                    "stop_reason": if self.truncated { "max_tokens" } else if self.emitted_tool_calls.is_empty() { "end_turn" } else { "tool_use" },
                    "stop_sequence": Value::Null,
                    "usage": {
                        "input_tokens": self.usage_input(),
                        "output_tokens": self.usage_output(),
                    },
                })
            }
        }
    }
}

fn parse_tool_block(payload: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(payload.trim()).ok()?;
    let name = parsed.get("name")?.as_str()?.to_string();
    let arguments = parsed.get("arguments").cloned().unwrap_or(json!({}));
    let arguments = match arguments {
        Value::String(s) => s,
        other => other.to_string(),
    };
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Some(json!({
        "id": format!("call_zenshim_{n}"),
        "type": "function",
        "name": name,
        "arguments": arguments,
    }))
}
