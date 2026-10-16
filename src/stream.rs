use serde_json::{json, Value};

/// Which client-facing API shape a stream/request renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    /// OpenAI Chat Completions.
    OpenAi,
    Anthropic,
    /// OpenAI Responses.
    Responses,
}

impl ApiFormat {
    pub fn label(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Responses => "responses",
        }
    }
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
    flushed_text: String,
    emitted_tool_calls: Vec<Value>,
    usage: Option<Value>,
    truncated: bool,
    finished: bool,
    tool_call_index: usize,
    anthropic_message_started: bool,
    anthropic_text_block_open: bool,
    anthropic_block_index: usize,
    /// Responses events are numbered.
    responses_sequence: u64,
    responses_started: bool,
    responses_text_open: bool,
    /// Output index of the text item (1 when a reasoning item precedes it).
    responses_text_index: usize,
    responses_reasoning_open: bool,
    /// Request fields a Responses object echoes back.
    responses_echo: Value,
    /// Reasoning/thinking summary text forwarded from upstream.
    reasoning_text: String,
    anthropic_thinking_open: bool,
}

const FENCE_OPEN: &str = "```tool";

impl StreamTranslator {
    pub fn new(format: ApiFormat, model: String) -> Self {
        Self {
            format,
            model,
            id: {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                match format {
                    ApiFormat::OpenAi => format!("chatcmpl-zenshim-{stamp:x}"),
                    ApiFormat::Anthropic => format!("msg_zenshim-{stamp:x}"),
                    ApiFormat::Responses => format!("resp_zenshim{stamp:x}"),
                }
            },
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            buffer: String::new(),
            flushed_text: String::new(),
            emitted_tool_calls: Vec::new(),
            usage: None,
            truncated: false,
            finished: false,
            tool_call_index: 0,
            anthropic_message_started: false,
            anthropic_text_block_open: false,
            anthropic_block_index: 0,
            responses_sequence: 0,
            responses_started: false,
            responses_text_open: false,
            responses_text_index: 0,
            responses_reasoning_open: false,
            responses_echo: json!({}),
            reasoning_text: String::new(),
            anthropic_thinking_open: false,
        }
    }

    /// The response id clients will see.
    #[cfg(test)]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Request fields (instructions, tools, …) to echo in a Responses object.
    pub fn echo(&mut self, fields: Value) {
        self.responses_echo = fields;
    }

    // ------------------------------------------------------------------
    // Responses rendering
    // ------------------------------------------------------------------

    fn responses_event(&mut self, kind: &str, mut data: Value) -> String {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.responses_sequence);
        self.responses_sequence += 1;
        format!("event: {kind}\ndata: {data}\n\n")
    }

    fn responses_message_id(&self) -> String {
        format!("msg_{}", self.id.trim_start_matches("resp_"))
    }

    fn responses_reasoning_id(&self) -> String {
        format!("rs_{}", self.id.trim_start_matches("resp_"))
    }

    fn responses_reasoning_item(&self, status: &str, text: &str) -> Value {
        json!({
            "id": self.responses_reasoning_id(),
            "type": "reasoning",
            "status": status,
            "summary": if status == "in_progress" { json!([]) } else { json!([{ "type": "summary_text", "text": text }]) },
        })
    }

    fn responses_message(&self, status: &str, text: &str) -> Value {
        let content = match status {
            "in_progress" => json!([]),
            _ => json!([{ "type": "output_text", "text": text, "annotations": [] }]),
        };
        json!({
            "id": self.responses_message_id(),
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": content,
        })
    }

    fn responses_call(call: &Value, status: &str) -> Value {
        let call_id = call["id"].as_str().unwrap_or("call");
        json!({
            "id": format!("fc_{}", call_id.trim_start_matches("call_")),
            "type": "function_call",
            "status": status,
            "call_id": call_id,
            "name": call["name"],
            "arguments": if status == "in_progress" { json!("") } else { call["arguments"].clone() },
        })
    }

    /// The output items of the finished answer: reasoning (if any), its
    /// text, then its calls.
    fn responses_output(&self) -> Vec<Value> {
        let text = self.flushed_text.trim();
        let mut output = Vec::new();
        if !self.reasoning_text.trim().is_empty() {
            output.push(self.responses_reasoning_item("completed", self.reasoning_text.trim()));
        }
        if !text.is_empty() {
            output.push(self.responses_message("completed", text));
        }
        output.extend(self.emitted_tool_calls.iter().map(|call| Self::responses_call(call, "completed")));
        output
    }

    fn responses_object(&self, status: &str, output: Vec<Value>) -> Value {
        let mut object = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "status": status,
            "error": Value::Null,
            "incomplete_details": if status == "incomplete" { json!({ "reason": "max_output_tokens" }) } else { Value::Null },
            "model": self.model,
            "output": output,
            "instructions": Value::Null,
            "previous_response_id": Value::Null,
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "tools": [],
            "metadata": {},
            "store": true,
            "text": { "format": { "type": "text" } },
            "truncation": "disabled",
            "usage": Value::Null,
        });
        if status != "in_progress" {
            let (input, output) = (self.usage_input(), self.usage_output());
            object["usage"] = json!({
                "input_tokens": input,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens": output,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": input + output,
            });
        }
        if let Some(echo) = self.responses_echo.as_object() {
            for (field, value) in echo {
                object[field] = value.clone();
            }
        }
        object
    }

    fn responses_ensure_started(&mut self, out: &mut Vec<String>) {
        if self.responses_started {
            return;
        }
        self.responses_started = true;
        let response = self.responses_object("in_progress", Vec::new());
        out.push(self.responses_event("response.created", json!({ "response": response.clone() })));
        out.push(self.responses_event("response.in_progress", json!({ "response": response })));
    }

    fn responses_text_delta(&mut self, text: &str, out: &mut Vec<String>) {
        self.responses_ensure_started(out);
        let item_id = self.responses_message_id();
        let index = self.responses_text_index;
        if !self.responses_text_open {
            self.responses_text_open = true;
            let item = self.responses_message("in_progress", "");
            out.push(self.responses_event("response.output_item.added", json!({ "output_index": index, "item": item })));
            out.push(self.responses_event(
                "response.content_part.added",
                json!({
                    "item_id": item_id, "output_index": index, "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }),
            ));
        }
        out.push(self.responses_event(
            "response.output_text.delta",
            json!({ "item_id": item_id, "output_index": index, "content_index": 0, "delta": text }),
        ));
    }

    /// Forward a reasoning/thinking delta as a native reasoning item.
    fn responses_reasoning_delta(&mut self, text: &str, out: &mut Vec<String>) {
        self.responses_ensure_started(out);
        let item_id = self.responses_reasoning_id();
        if !self.responses_reasoning_open {
            self.responses_reasoning_open = true;
            self.responses_text_index = 1;
            out.push(self.responses_event(
                "response.output_item.added",
                json!({ "output_index": 0, "item": self.responses_reasoning_item("in_progress", "") }),
            ));
            out.push(self.responses_event(
                "response.reasoning_summary_part.added",
                json!({ "item_id": item_id, "output_index": 0, "content_index": 0, "part": { "type": "summary_text", "text": "" } }),
            ));
        }
        out.push(self.responses_event(
            "response.reasoning_summary_text.delta",
            json!({ "item_id": item_id, "output_index": 0, "content_index": 0, "delta": text }),
        ));
    }

    /// Close the text item, emit each call as its own item, then the
    /// final response object.
    fn responses_finish(&mut self, out: &mut Vec<String>) {
        self.responses_ensure_started(out);
        let mut index = 0;
        if self.responses_reasoning_open {
            let item_id = self.responses_reasoning_id();
            let text = self.reasoning_text.trim().to_string();
            out.push(self.responses_event(
                "response.reasoning_summary_text.done",
                json!({ "item_id": item_id, "output_index": 0, "content_index": 0, "text": text }),
            ));
            out.push(self.responses_event(
                "response.reasoning_summary_part.done",
                json!({
                    "item_id": item_id, "output_index": 0, "content_index": 0,
                    "part": { "type": "summary_text", "text": text },
                }),
            ));
            out.push(self.responses_event(
                "response.output_item.done",
                json!({ "output_index": 0, "item": self.responses_reasoning_item("completed", &text) }),
            ));
            index = 1;
        }
        if self.responses_text_open {
            let item_id = self.responses_message_id();
            let text_index = self.responses_text_index;
            let text = self.flushed_text.trim().to_string();
            out.push(self.responses_event(
                "response.output_text.done",
                json!({ "item_id": item_id, "output_index": text_index, "content_index": 0, "text": text }),
            ));
            out.push(self.responses_event(
                "response.content_part.done",
                json!({
                    "item_id": item_id, "output_index": text_index, "content_index": 0,
                    "part": { "type": "output_text", "text": text, "annotations": [] },
                }),
            ));
            let item = self.responses_message("completed", &text);
            out.push(self.responses_event("response.output_item.done", json!({ "output_index": text_index, "item": item })));
            index = text_index + 1;
        }
        for call in self.emitted_tool_calls.clone() {
            let pending = Self::responses_call(&call, "in_progress");
            let done = Self::responses_call(&call, "completed");
            let item_id = done["id"].clone();
            out.push(self.responses_event("response.output_item.added", json!({ "output_index": index, "item": pending })));
            out.push(self.responses_event(
                "response.function_call_arguments.delta",
                json!({ "item_id": item_id, "output_index": index, "delta": call["arguments"] }),
            ));
            out.push(self.responses_event(
                "response.function_call_arguments.done",
                json!({ "item_id": item_id, "output_index": index, "arguments": call["arguments"] }),
            ));
            out.push(self.responses_event("response.output_item.done", json!({ "output_index": index, "item": done })));
            index += 1;
        }
        let status = if self.truncated { "incomplete" } else { "completed" };
        let response = self.responses_object(status, self.responses_output());
        out.push(self.responses_event(&format!("response.{status}"), json!({ "response": response })));
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
        // Thinking must precede text: close it first if it is open.
        self.anthropic_close_thinking_block(out);
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

    fn anthropic_close_thinking_block(&mut self, out: &mut Vec<String>) {
        if !self.anthropic_thinking_open {
            return;
        }
        self.anthropic_thinking_open = false;
        out.push(self.anthropic_event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": self.anthropic_block_index }),
        ));
        self.anthropic_block_index += 1;
    }

    /// Forward a thinking delta as a native thinking block (no signature:
    /// zenshim's request translation flattens history into a transcript,
    /// so echoed thinking blocks never return to the model).
    fn anthropic_reasoning_delta(&mut self, text: &str, out: &mut Vec<String>) {
        self.anthropic_ensure_started(out);
        if !self.anthropic_thinking_open {
            if self.anthropic_text_block_open {
                // Text already started: thinking cannot precede it anymore.
                return;
            }
            self.anthropic_thinking_open = true;
            out.push(self.anthropic_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": self.anthropic_block_index,
                    "content_block": { "type": "thinking", "thinking": "" },
                }),
            ));
        }
        out.push(self.anthropic_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": self.anthropic_block_index,
                "delta": { "type": "thinking_delta", "thinking": text },
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

    /// How much of the tail is safe to flush when no full fence opener is
    /// present: everything except a partial opener prefix at the end.
    /// Compared as bytes: the tail may end inside a multi-byte character,
    /// where slicing the string itself would panic.
    fn safe_tail(&self) -> usize {
        let bytes = self.buffer.as_bytes();
        let fence = FENCE_OPEN.as_bytes();
        for hold in (1..=fence.len()).rev() {
            if bytes.len() >= hold && bytes[bytes.len() - hold..] == fence[..hold] {
                return bytes.len() - hold;
            }
        }
        bytes.len()
    }

    /// Consume the buffer: flush text that cannot be part of a tool
    /// block, harvest complete closed blocks as they close (so text that
    /// follows them streams live instead of waiting for the end), and
    /// re-emit unparseable blocks as visible text so the model can see
    /// its own malformed call and self-correct. With `final_flush` an
    /// unclosed or malformed remainder is kept as text too.
    fn pump(&mut self, final_flush: bool, out: &mut Vec<String>) {
        loop {
            let Some(open) = self.buffer.find(FENCE_OPEN) else {
                // No fence opener: flush what cannot grow into one.
                let safe = if final_flush { self.buffer.len() } else { self.safe_tail() };
                if safe > 0 {
                    let flushed: String = self.buffer.drain(..safe).collect();
                    self.emit_text(&flushed, out);
                }
                return;
            };
            if open > 0 {
                let pre: String = self.buffer.drain(..open).collect();
                self.emit_text(&pre, out);
            }
            match scan_tool_block(&self.buffer, FENCE_OPEN.len()) {
                Some((payload, end)) => {
                    let consumed: String = self.buffer.drain(..end).collect();
                    match parse_tool_block(&payload) {
                        Some(call) => self.emitted_tool_calls.push(call),
                        // Not a valid call: keep it visible as text.
                        None => self.emit_text(&consumed, out),
                    }
                }
                // Incomplete block or prose: hold everything from the
                // opener; the final flush decides (unclosed/malformed
                // blocks become text for self-correction).
                None => {
                    if final_flush {
                        let rest: String = self.buffer.drain(..).collect();
                        self.emit_text(&rest, out);
                    }
                    return;
                }
            }
        }
    }

    /// Emit one flushed text delta in the active format.
    fn emit_text(&mut self, text: &str, out: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        self.flushed_text.push_str(text);
        match self.format {
            ApiFormat::OpenAi => out.push(self.openai_chunk(json!({ "content": text }), None)),
            ApiFormat::Responses => self.responses_text_delta(text, out),
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

    /// Forward one reasoning/thinking delta in the active format. The
    /// direct route previously dropped these; reasoning models (muse-spark
    /// & the paid GPT catalog) think visibly.
    fn emit_reasoning(&mut self, text: &str, out: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        self.reasoning_text.push_str(text);
        match self.format {
            // DeepSeek-style reasoning_content, the de-facto convention.
            ApiFormat::OpenAi => out.push(self.openai_chunk(json!({ "reasoning_content": text }), None)),
            ApiFormat::Responses => self.responses_reasoning_delta(text, out),
            ApiFormat::Anthropic => self.anthropic_reasoning_delta(text, out),
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
                    self.pump(false, &mut out);
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = parsed.get("delta").and_then(Value::as_str) {
                    self.emit_reasoning(delta, &mut out);
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
        self.pump(true, &mut out);
        if self.format == ApiFormat::Anthropic {
            self.anthropic_close_thinking_block(&mut out);
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
                // Rendered together with the final object, below.
                ApiFormat::Responses => {}
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
            ApiFormat::Responses => self.responses_finish(&mut out),
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

    /// False until upstream sent its completion event (or `close` ran).
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Finish reason, (input, output) token usage and tool-call count,
    /// for the activity log.
    pub fn outcome(&self) -> (&'static str, Option<(u64, u64)>, usize) {
        let reason = if self.truncated {
            "length"
        } else if self.emitted_tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let usage = self
            .usage
            .as_ref()
            .map(|_| (self.usage_input(), self.usage_output()));
        (reason, usage, self.emitted_tool_calls.len())
    }

    /// Final assembled message for non-streaming responses.
    pub fn into_completion(mut self) -> Value {
        self.completion()
    }

    /// The same, leaving the translator usable (to store the response).
    pub fn completion(&mut self) -> Value {
        if !self.finished {
            self.close();
        }
        // finish() already flushed whatever the buffer still held.
        let content = self.flushed_text.trim().to_string();
        let reasoning = self.reasoning_text.trim().to_string();
        match self.format {
            ApiFormat::OpenAi => {
                let mut message =
                    json!({ "role": "assistant", "content": if content.is_empty() { Value::Null } else { json!(content) } });
                if !reasoning.is_empty() {
                    message["reasoning_content"] = json!(reasoning);
                }
                if !self.emitted_tool_calls.is_empty() {
                    let calls: Vec<Value> = self
                        .emitted_tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call["id"],
                                "type": "function",
                                "function": { "name": call["name"], "arguments": call["arguments"] },
                            })
                        })
                        .collect();
                    message["tool_calls"] = json!(calls);
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
            ApiFormat::Responses => {
                let status = if self.truncated { "incomplete" } else { "completed" };
                self.responses_object(status, self.responses_output())
            }
            ApiFormat::Anthropic => {
                let mut blocks: Vec<Value> = Vec::new();
                if !reasoning.is_empty() {
                    blocks.push(json!({ "type": "thinking", "thinking": reasoning }));
                }
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

/// Find the end of a fenced tool block whose opener begins the string
/// (`from` = just past "```tool"): the FIRST balanced JSON object —
/// braces tracked with string/escape awareness — followed by the closing
/// fence.
///
/// This is what makes bash tool calls work: command strings routinely
/// contain triple backticks (heredocs writing markdown, echoed code
/// fences…), so "close at the first ```" truncates the JSON and the call
/// is lost. Brace scanning finds the real end of the JSON first, then
/// expects the closing fence after it. All scanned characters are ASCII,
/// so byte indexing is UTF-8-safe.
///
/// Returns the payload and the index just past the closing fence, or
/// None while the block is still incomplete (streaming) or the text is
/// not a well-formed block (prose mentioning ```tool).
fn scan_tool_block(s: &str, from: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut start = from;
    while start < bytes.len() && bytes[start] != b'{' {
        start += 1;
    }
    if start == bytes.len() {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut j = start;
    while j < bytes.len() {
        let ch = bytes[j];
        if in_string {
            if escape {
                escape = false;
            } else if ch == b'\\' {
                escape = true;
            } else if ch == b'"' {
                in_string = false;
            }
        } else if ch == b'"' {
            in_string = true;
        } else if ch == b'{' {
            depth += 1;
        } else if ch == b'}' {
            depth -= 1;
            if depth == 0 {
                let rest = &s[j + 1..];
                let trimmed = rest.trim_start_matches([' ', '\t', '\r', '\n']);
                return trimmed.strip_prefix("```").map(|after| {
                    let end = s.len() - after.len();
                    (s[start..j + 1].to_string(), end)
                });
            }
        }
        j += 1;
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> String {
        json!({ "delta": text }).to_string()
    }

    const DONE: &str = r#"{"response":{"usage":{"input_tokens":10,"output_tokens":4}}}"#;

    /// Everything the translator renders for these upstream text deltas.
    fn run(format: ApiFormat, deltas: &[&str], last: &str) -> (Vec<String>, StreamTranslator) {
        let mut translator = StreamTranslator::new(format, "m".to_string());
        let mut out = Vec::new();
        for text in deltas {
            out.extend(translator.feed("response.output_text.delta", &delta(text)));
        }
        out.extend(translator.feed(last, DONE));
        (out, translator)
    }

    fn openai_text(out: &[String]) -> String {
        out.iter()
            .filter_map(|chunk| chunk.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str().map(String::from))
            .collect()
    }

    #[test]
    fn text_ending_in_any_character_streams_without_panicking() {
        // Tails of 1-4 byte characters, which once crashed the fence check.
        for text in ["caf\u{e9}", "ok \u{2713}", "hi \u{1f600}", "\u{4f60}\u{597d}", "a`", "x``"] {
            let (out, translator) = run(ApiFormat::OpenAi, &[text], "response.completed");
            assert_eq!(openai_text(&out), text.trim());
            assert!(translator.is_finished());
        }
    }

    #[test]
    fn plain_text_streams_as_it_arrives_with_usage_at_the_end() {
        let (out, translator) = run(ApiFormat::OpenAi, &["Hel", "lo"], "response.completed");
        assert_eq!(openai_text(&out), "Hello");
        assert!(out.len() >= 3, "each delta is its own chunk");
        assert_eq!(translator.outcome(), ("stop", Some((10, 4)), 0));
        let completion = translator.into_completion();
        assert_eq!(completion["choices"][0]["message"]["content"], "Hello");
        assert_eq!(completion["usage"]["total_tokens"], 14);
    }

    #[test]
    fn a_tool_block_split_across_deltas_becomes_a_tool_call_not_text() {
        let deltas = ["Let me check.\n``", "`tool\n{\"name\": \"calc\", \"argu", "ments\": {\"expr\": \"2+2\"}}\n```"];
        let (out, translator) = run(ApiFormat::OpenAi, &deltas, "response.completed");
        assert_eq!(openai_text(&out).trim_end(), "Let me check.");
        assert!(!out.concat().contains("```"), "no fence leaks into the content");
        assert_eq!(translator.outcome().0, "tool_calls");
        let completion = translator.into_completion();
        let call = &completion["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "calc");
        let arguments: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({ "expr": "2+2" }));
        assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn text_held_back_until_the_end_appears_once() {
        // A trailing backtick is held as a possible fence, then flushed at
        // the end; so is text that follows a tool block.
        let (_, translator) = run(ApiFormat::OpenAi, &["Use `x"], "response.completed");
        assert_eq!(translator.into_completion()["choices"][0]["message"]["content"], "Use `x");
        let (_, translator) = run(ApiFormat::OpenAi, &["tick `"], "response.completed");
        assert_eq!(translator.into_completion()["choices"][0]["message"]["content"], "tick `");
        let deltas = ["Before.\n```tool\n{\"name\": \"t\", \"arguments\": {}}\n```\nAfter."];
        let (_, translator) = run(ApiFormat::Anthropic, &deltas, "response.completed");
        let message = translator.into_completion();
        let text = message["content"][0]["text"].as_str().unwrap();
        assert_eq!(text.matches("After.").count(), 1);
        assert_eq!(text.matches("Before.").count(), 1);
    }

    #[test]
    fn anthropic_renders_text_blocks_and_tool_use() {
        let deltas = ["Checking.\n```tool\n{\"name\": \"weather\", \"arguments\": {\"city\": \"Paris\"}}\n```"];
        let (out, translator) = run(ApiFormat::Anthropic, &deltas, "response.completed");
        let wire = out.concat();
        for event in ["message_start", "content_block_start", "content_block_delta", "message_delta", "message_stop"] {
            assert!(wire.contains(&format!("event: {event}")), "missing {event}");
        }
        let message = translator.into_completion();
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["content"][0]["text"], "Checking.");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["name"], "weather");
        assert_eq!(message["content"][1]["input"], json!({ "city": "Paris" }));
        assert_eq!(message["usage"], json!({ "input_tokens": 10, "output_tokens": 4 }));
    }

    /// (event name, data) pairs of a rendered Responses stream.
    fn responses_events(out: &[String]) -> Vec<(String, Value)> {
        out.iter()
            .map(|block| {
                let (event, data) = block.trim_end().split_once("\ndata: ").unwrap();
                (event.trim_start_matches("event: ").to_string(), serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    #[test]
    fn responses_stream_follows_the_standard_event_sequence() {
        let deltas = ["Check", "ing.\n```tool\n{\"name\": \"calc\", \"arguments\": {\"expr\": \"2+2\"}}\n```"];
        let (out, _) = run(ApiFormat::Responses, &deltas, "response.completed");
        let events = responses_events(&out);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        for (index, (name, data)) in events.iter().enumerate() {
            assert_eq!(data["type"], name.as_str());
            assert_eq!(data["sequence_number"], index as u64);
        }
        let said: String = events
            .iter()
            .filter(|(name, _)| name == "response.output_text.delta")
            .map(|(_, data)| data["delta"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(said.trim_end(), "Checking.");
        let response = &events.last().unwrap().1["response"];
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["text"], "Checking.");
        assert_eq!(response["output"][1]["type"], "function_call");
        assert_eq!(response["output"][1]["name"], "calc");
        assert_eq!(response["output"][1]["arguments"], "{\"expr\":\"2+2\"}");
        assert_eq!(response["usage"]["total_tokens"], 14);
    }

    #[test]
    fn responses_object_echoes_request_fields_and_reports_truncation() {
        let mut translator = StreamTranslator::new(ApiFormat::Responses, "m".to_string());
        translator.echo(json!({ "instructions": "Be terse.", "store": false, "previous_response_id": "resp_1" }));
        translator.feed("response.output_text.delta", &delta("Partial"));
        translator.feed("response.incomplete", DONE);
        let id = translator.id().to_string();
        let response = translator.into_completion();
        assert!(id.starts_with("resp_") && response["id"] == id.as_str());
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(response["instructions"], "Be terse.");
        assert_eq!(response["store"], false);
        assert_eq!(response["previous_response_id"], "resp_1");
        assert_eq!(response["output"][0]["content"][0]["text"], "Partial");
    }

    #[test]
    fn an_incomplete_response_reports_truncation_in_both_dialects() {
        let (_, openai) = run(ApiFormat::OpenAi, &["Partial"], "response.incomplete");
        assert_eq!(openai.into_completion()["choices"][0]["finish_reason"], "length");
        let (_, anthropic) = run(ApiFormat::Anthropic, &["Partial"], "response.incomplete");
        assert_eq!(anthropic.into_completion()["stop_reason"], "max_tokens");
    }

    #[test]
    fn a_stream_that_just_stops_still_yields_what_arrived() {
        let mut translator = StreamTranslator::new(ApiFormat::OpenAi, "m".to_string());
        translator.feed("response.output_text.delta", &delta("Half an ans"));
        assert!(!translator.is_finished());
        assert!(!translator.close().is_empty());
        assert_eq!(translator.into_completion()["choices"][0]["message"]["content"], "Half an ans");
    }

    #[test]
    fn a_bash_command_containing_triple_backticks_keeps_its_tool_call() {
        // Heredocs writing markdown etc: closing at the first ``` used to
        // truncate the JSON and lose the call ("model stops midway").
        let command = "cat > README.md <<'EOF'\n# Title\n```bash\nls\n```\nEOF";
        let json = json!({ "name": "bash", "arguments": { "command": command } }).to_string();
        let deltas = [format!("Writing.\n```tool\n{json}\n```\nDone.")];
        let refs: Vec<&str> = deltas.iter().map(String::as_str).collect();
        let (out, translator) = run(ApiFormat::OpenAi, &refs, "response.completed");
        assert!(translator.outcome().0 == "tool_calls");
        let completion = translator.into_completion();
        let call = &completion["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "bash");
        let arguments: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["command"], command);
        assert!(!out.concat().contains("```tool"), "no fence leak");
    }

    #[test]
    fn an_unparseable_block_is_kept_as_visible_text() {
        let deltas = ["Trying.\n```tool\nnot json at all\n```\nAfter."];
        let (out, translator) = run(ApiFormat::OpenAi, &deltas, "response.completed");
        assert_eq!(translator.outcome().0, "stop");
        let text = openai_text(&out);
        assert!(text.contains("```tool\nnot json at all\n```"), "raw block stays visible");
        assert!(text.contains("After."));
    }

    #[test]
    fn text_after_a_closed_block_streams_live() {
        // The block closes in the first delta; "After." must arrive as its
        // own delta BEFORE the completion event, not only at finish.
        let block = "```tool\n{\"name\": \"t\", \"arguments\": {}}\n```";
        let mut translator = StreamTranslator::new(ApiFormat::OpenAi, "m".to_string());
        let before: Vec<String> = translator.feed("response.output_text.delta", &delta(&format!("Before.\n{block}\n")));
        let after: Vec<String> = translator.feed("response.output_text.delta", &delta("After."));
        assert_eq!(openai_text(&before), "Before.\n\n");
        assert_eq!(openai_text(&after), "After.", "post-fence text streams live");
    }

    #[test]
    fn reasoning_summary_text_is_forwarded_in_every_dialect() {
        for format in [ApiFormat::OpenAi, ApiFormat::Responses, ApiFormat::Anthropic] {
            let mut translator = StreamTranslator::new(format, "m".to_string());
            let mut out = Vec::new();
            out.extend(translator.feed("response.reasoning_summary_text.delta", &delta("Think")));
            out.extend(translator.feed("response.reasoning_summary_text.delta", &delta(" hard")));
            out.extend(translator.feed("response.output_text.delta", &delta("Answer.")));
            out.extend(translator.feed("response.completed", DONE));
            let wire = out.concat();
            assert!(!wire.is_empty());
            let completion = translator.into_completion();
            match format {
                ApiFormat::OpenAi => {
                    let reasoning: String = out
                        .iter()
                        .filter_map(|c| c.strip_prefix("data: "))
                        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                        .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str().map(String::from))
                        .collect();
                    assert_eq!(reasoning, "Think hard");
                    assert_eq!(completion["choices"][0]["message"]["reasoning_content"], "Think hard");
                    assert_eq!(completion["choices"][0]["message"]["content"], "Answer.");
                }
                ApiFormat::Responses => {
                    let events = responses_events(&out);
                    assert_eq!(events[0].0, "response.created");
                    assert_eq!(events[2].0, "response.output_item.added");
                    assert_eq!(events[2].1["item"]["type"], "reasoning");
                    let said: String = events
                        .iter()
                        .filter(|(name, _)| name == "response.reasoning_summary_text.delta")
                        .map(|(_, data)| data["delta"].as_str().unwrap().to_string())
                        .collect();
                    assert_eq!(said, "Think hard");
                    let response = &completion["output"];
                    assert_eq!(response[0]["type"], "reasoning");
                    assert_eq!(response[0]["summary"][0]["text"], "Think hard");
                    assert_eq!(response[1]["content"][0]["text"], "Answer.");
                }
                ApiFormat::Anthropic => {
                    assert!(wire.contains("\"type\":\"thinking\""), "thinking block emitted");
                    assert!(wire.contains("thinking_delta"));
                    let blocks = completion["content"].as_array().unwrap();
                    assert_eq!(blocks[0]["type"], "thinking");
                    assert_eq!(blocks[0]["thinking"], "Think hard");
                    assert_eq!(blocks[1]["type"], "text");
                    assert_eq!(blocks[1]["text"], "Answer.");
                }
            }
        }
    }

    #[test]
    fn events_after_completion_and_unparsable_data_are_ignored() {
        let (_, mut translator) = run(ApiFormat::OpenAi, &["Done"], "response.completed");
        assert!(translator.feed("response.output_text.delta", &delta("extra")).is_empty());
        let mut fresh = StreamTranslator::new(ApiFormat::OpenAi, "m".to_string());
        assert!(fresh.feed("response.output_text.delta", "not json").is_empty());
        assert!(fresh.feed("ping", "{}").is_empty());
    }
}
