use serde_json::{json, Value};

/// Translates a Zen Responses-API SSE stream into OpenAI chat-completions
/// chunks, synthesizing tool_calls from fenced ```tool blocks in the text.
pub struct StreamTranslator {
    model: String,
    id: String,
    created: u64,
    buffer: String,
    clean_text: String,
    flushed_text: String,
    emitted_tool_calls: Vec<Value>,
    usage: Option<Value>,
    finished: bool,
    tool_call_index: usize,
}

const FENCE_OPEN: &str = "```tool";

impl StreamTranslator {
    pub fn new(model: String) -> Self {
        Self {
            model,
            id: format!("chatcmpl-zenshim-{:x}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            buffer: String::new(),
            clean_text: String::new(),
            flushed_text: String::new(),
            emitted_tool_calls: Vec::new(),
            usage: None,
            finished: false,
            tool_call_index: 0,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> String {
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

    /// Feed one zen SSE event; returns OpenAI SSE chunks to emit.
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
                        self.flushed_text.push_str(&flushed);
                        out.push(self.chunk(json!({ "content": flushed }), None));
                    }
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(u) = parsed.pointer("/response/usage") {
                    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    self.usage = Some(json!({
                        "prompt_tokens": input,
                        "completion_tokens": output,
                        "total_tokens": input + output,
                    }));
                }
                out.extend(self.finish(if event == "response.completed" { "stop" } else { "stop" }));
            }
            _ => {}
        }
        out
    }

    /// Flush everything: remaining text, parsed tool calls, finish reason.
    fn finish(&mut self, fallback_reason: &str) -> Vec<String> {
        let mut out = Vec::new();
        // Extract fenced tool blocks from whatever remains.
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
                    // Unterminated fence: drop it.
                    rest = "";
                }
            }
        }
        clean_text.push_str(rest);
        let clean_text = clean_text.trim().to_string();
        self.clean_text = clean_text.clone();
        if !clean_text.is_empty() {
            out.push(self.chunk(json!({ "content": clean_text }), None));
        }

        // Emit synthesized tool calls.
        let mut any = false;
        for call in self.emitted_tool_calls.clone() {
            any = true;
            out.push(self.chunk(
                json!({ "tool_calls": [{
                    "index": self.tool_call_index,
                    "id": call["id"],
                    "type": "function",
                    "function": { "name": call["name"], "arguments": call["arguments"] },
                }]}),
                None,
            ));
            self.tool_call_index += 1;
        }

        let reason = if any { "tool_calls" } else { fallback_reason };
        out.push(self.chunk(json!({}), Some(reason)));

        // Final usage chunk (OpenAI streams end with usage then [DONE]).
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
        self.finished = true;
        out
    }

    /// Flush and finish if the stream ended without a completion event.
    pub fn close(&mut self) -> Vec<String> {
        if self.finished {
            Vec::new()
        } else {
            self.finish("stop")
        }
    }

    /// Final assembled message for non-streaming responses.
    pub fn into_completion(mut self) -> Value {
        if !self.finished {
            self.close();
        }
        let content = format!("{}{}", self.flushed_text, self.clean_text).trim().to_string();
        let mut message = json!({ "role": "assistant", "content": if content.is_empty() { Value::Null } else { json!(content) } });
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
                "finish_reason": if self.emitted_tool_calls.is_empty() { "stop" } else { "tool_calls" },
            }],
            "usage": self.usage.clone().unwrap_or(json!({
                "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
            })),
        })
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
