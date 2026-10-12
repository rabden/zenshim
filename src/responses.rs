//! The OpenAI Responses dialect: its requests are converted to the chat
//! shape the rest of the gateway already translates, and finished
//! responses are kept in memory so `previous_response_id` can continue a
//! conversation and `GET /v1/responses/{id}` can read one back.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Responses kept for `previous_response_id` and retrieval.
const MAX_STORED: usize = 500;
const MAX_STORED_BYTES: usize = 128 * 1024 * 1024;
const KEPT_FOR: Duration = Duration::from_secs(3600);

/// A Responses request, ready for the chat translation.
pub struct Prepared {
    /// The equivalent Chat Completions request.
    pub chat: Value,
    /// Request fields the response object repeats back.
    pub echo: Value,
    /// Whether to keep the response for later reference.
    pub store: bool,
    /// The conversation so far as chat messages, without instructions
    /// (those apply to one response only).
    pub conversation: Vec<Value>,
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Responses input (a string or a list of items) as chat messages.
fn input_messages(input: &Value) -> Vec<Value> {
    let items = match input {
        Value::String(text) => return vec![json!({ "role": "user", "content": text })],
        Value::Array(items) => items,
        _ => return Vec::new(),
    };
    let mut messages = Vec::new();
    for item in items {
        let kind = item.get("type").and_then(Value::as_str);
        match kind {
            Some("function_call") => messages.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": item.get("call_id").or(item.get("id")).cloned().unwrap_or(json!("call")),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(json!("?")),
                        "arguments": as_text(item.get("arguments").unwrap_or(&Value::Null)),
                    },
                }],
            })),
            Some("function_call_output") => messages.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(json!("")),
                "content": as_text(item.get("output").unwrap_or(&Value::Null)),
            })),
            // Messages come with or without an explicit type.
            Some("message") | None => {
                let role = match item.get("role").and_then(Value::as_str) {
                    Some("developer") => "system",
                    Some(role) => role,
                    None => continue,
                };
                // Content parts keep their Responses shapes; the chat
                // translation reads both.
                messages.push(json!({ "role": role, "content": item.get("content").cloned().unwrap_or(Value::Null) }));
            }
            // Reasoning items, references and the like carry nothing to replay.
            Some(_) => {}
        }
    }
    messages
}

/// A finished response's output as the chat messages it amounts to.
pub fn output_messages(response: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let output = response.get("output").and_then(Value::as_array).unwrap_or(&empty);
    let text: String = output
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(String::from))
        .collect();
    let calls: Vec<Value> = output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| {
            json!({
                "id": item["call_id"],
                "type": "function",
                "function": { "name": item["name"], "arguments": item["arguments"] },
            })
        })
        .collect();
    if text.is_empty() && calls.is_empty() {
        return Vec::new();
    }
    let mut message = json!({ "role": "assistant", "content": text });
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    vec![message]
}

/// Convert a Responses request, continuing from `previous_response_id`
/// when it names a stored response.
pub fn prepare(store: &ResponseStore, request: &Value) -> Result<Prepared, String> {
    let mut conversation = match request.get("previous_response_id").and_then(Value::as_str) {
        Some(id) => store.conversation(id).ok_or_else(|| {
            format!("previous response '{id}' was not found; stored responses are kept for one hour")
        })?,
        None => Vec::new(),
    };
    let fresh = input_messages(request.get("input").unwrap_or(&Value::Null));
    if fresh.is_empty() && conversation.is_empty() {
        return Err("input must be a string or a non-empty list of input items".to_string());
    }
    conversation.extend(fresh);

    let mut messages = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        messages.push(json!({ "role": "system", "content": instructions }));
    }
    messages.extend(conversation.iter().cloned());

    // Function tools are flat here and nested in chat; hosted tools
    // (web search and so on) have nothing to map to.
    let tools: Vec<Value> = request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool["type"] == "function")
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool["name"],
                    "description": tool.get("description").cloned().unwrap_or(json!("")),
                    "parameters": tool.get("parameters").cloned().unwrap_or(json!({})),
                },
            })
        })
        .collect();

    let mut chat = json!({
        "model": request.get("model").cloned().unwrap_or(Value::Null),
        "stream": request.get("stream").cloned().unwrap_or(json!(false)),
        "messages": messages,
    });
    if !tools.is_empty() {
        chat["tools"] = json!(tools);
    }
    if let Some(max) = request.get("max_output_tokens").and_then(Value::as_u64) {
        chat["max_tokens"] = json!(max);
    }

    let store_it = request.get("store").and_then(Value::as_bool).unwrap_or(true);
    let mut echo = json!({ "store": store_it });
    for field in [
        "instructions", "previous_response_id", "metadata", "tools", "tool_choice",
        "parallel_tool_calls", "max_output_tokens", "temperature", "top_p",
    ] {
        if let Some(value) = request.get(field).filter(|value| !value.is_null()) {
            echo[field] = value.clone();
        }
    }
    Ok(Prepared { chat, echo, store: store_it, conversation })
}

struct Stored {
    response: Value,
    /// Everything said up to and including this response.
    conversation: Vec<Value>,
    at: Instant,
    bytes: usize,
}

#[derive(Default)]
struct Shelf {
    entries: HashMap<String, Stored>,
    order: VecDeque<String>,
    bytes: usize,
}

impl Shelf {
    fn drop_entry(&mut self, id: &str) -> bool {
        match self.entries.remove(id) {
            Some(entry) => {
                self.bytes -= entry.bytes;
                true
            }
            None => false,
        }
    }

    /// Forget what has expired or no longer fits.
    fn trim(&mut self, now: Instant) {
        while let Some(oldest) = self.order.front().cloned() {
            let stale = match self.entries.get(&oldest) {
                Some(entry) => {
                    now.duration_since(entry.at) >= KEPT_FOR
                        || self.entries.len() > MAX_STORED
                        || self.bytes > MAX_STORED_BYTES
                }
                // Deleted by hand; only its place in line is left.
                None => true,
            };
            if !stale {
                break;
            }
            self.order.pop_front();
            self.drop_entry(&oldest);
        }
    }
}

/// Finished responses, in memory, for an hour.
#[derive(Default)]
pub struct ResponseStore {
    shelf: Mutex<Shelf>,
}

impl ResponseStore {
    pub fn put(&self, response: Value, conversation: Vec<Value>) {
        let Some(id) = response.get("id").and_then(Value::as_str).map(String::from) else { return };
        let bytes = response.to_string().len() + conversation.iter().map(|m| m.to_string().len()).sum::<usize>();
        if bytes > MAX_STORED_BYTES / 4 {
            return;
        }
        let mut shelf = self.shelf.lock().unwrap();
        shelf.drop_entry(&id);
        shelf.bytes += bytes;
        shelf.entries.insert(id.clone(), Stored { response, conversation, at: Instant::now(), bytes });
        shelf.order.push_back(id);
        shelf.trim(Instant::now());
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        let mut shelf = self.shelf.lock().unwrap();
        shelf.trim(Instant::now());
        shelf.entries.get(id).map(|entry| entry.response.clone())
    }

    fn conversation(&self, id: &str) -> Option<Vec<Value>> {
        let mut shelf = self.shelf.lock().unwrap();
        shelf.trim(Instant::now());
        shelf.entries.get(id).map(|entry| entry.conversation.clone())
    }

    pub fn delete(&self, id: &str) -> bool {
        self.shelf.lock().unwrap().drop_entry(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(id: &str, text: &str) -> Value {
        json!({ "id": id, "output": [{ "type": "message", "content": [{ "type": "output_text", "text": text }] }] })
    }

    #[test]
    fn a_plain_string_input_with_instructions_becomes_two_messages() {
        let request = json!({ "model": "m", "instructions": "Be terse.", "input": "Hello", "max_output_tokens": 99 });
        let prepared = prepare(&ResponseStore::default(), &request).unwrap();
        assert_eq!(
            prepared.chat["messages"],
            json!([{ "role": "system", "content": "Be terse." }, { "role": "user", "content": "Hello" }])
        );
        assert_eq!(prepared.chat["max_tokens"], 99);
        assert!(prepared.store, "responses are stored unless asked otherwise");
        assert_eq!(prepared.echo["instructions"], "Be terse.");
        assert_eq!(prepared.conversation.len(), 1, "instructions are not part of the conversation");
    }

    #[test]
    fn items_tools_and_function_results_map_to_chat_shapes() {
        let request = json!({
            "model": "m",
            "store": false,
            "input": [
                { "role": "developer", "content": "Use tools." },
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "Weather?" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
                ]},
                { "type": "reasoning", "summary": [] },
                { "type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"Paris\"}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "Sunny" },
            ],
            "tools": [
                { "type": "function", "name": "weather", "description": "Look up", "parameters": { "type": "object" } },
                { "type": "web_search" },
            ],
        });
        let prepared = prepare(&ResponseStore::default(), &request).unwrap();
        let messages = prepared.chat["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "weather");
        assert_eq!(messages[3], json!({ "role": "tool", "tool_call_id": "call_1", "content": "Sunny" }));
        assert_eq!(prepared.chat["tools"].as_array().unwrap().len(), 1, "hosted tools are dropped");
        assert_eq!(prepared.chat["tools"][0]["function"]["name"], "weather");
        assert!(!prepared.store);

        // The chat translation understands the Responses content parts.
        let body = crate::translate::build_zen_body(&prepared.chat, "m");
        let transcript = body.pointer("/input/2/content/0/text").and_then(Value::as_str).unwrap();
        assert!(transcript.contains("[user] Weather? (image attached)"));
        assert!(transcript.contains("[tool result for call_1: Sunny]"));
        assert_eq!(body.pointer("/input/2/content/1/image_url").unwrap(), "data:image/png;base64,AAAA");
    }

    #[test]
    fn previous_response_id_continues_the_stored_conversation() {
        let store = ResponseStore::default();
        let first = prepare(&store, &json!({ "model": "m", "instructions": "Old rules.", "input": "My name is Rab." })).unwrap();
        let answer = response("resp_1", "Hello Rab.");
        let mut conversation = first.conversation;
        conversation.extend(output_messages(&answer));
        store.put(answer, conversation);

        let next = prepare(&store, &json!({ "model": "m", "previous_response_id": "resp_1", "input": "What is my name?" })).unwrap();
        assert_eq!(
            next.chat["messages"],
            json!([
                { "role": "user", "content": "My name is Rab." },
                { "role": "assistant", "content": "Hello Rab." },
                { "role": "user", "content": "What is my name?" },
            ]),
            "earlier instructions are not inherited"
        );
        assert_eq!(next.echo["previous_response_id"], "resp_1");
    }

    #[test]
    fn unknown_previous_response_and_empty_input_are_errors() {
        let store = ResponseStore::default();
        let missing = prepare(&store, &json!({ "previous_response_id": "resp_nope", "input": "hi" }));
        assert!(missing.err().unwrap().contains("resp_nope"));
        assert!(prepare(&store, &json!({ "model": "m" })).is_err());
        assert!(prepare(&store, &json!({ "model": "m", "input": [] })).is_err());
    }

    #[test]
    fn function_calls_in_an_answer_are_remembered_as_tool_calls() {
        let answer = json!({ "id": "resp_1", "output": [
            { "type": "message", "content": [{ "type": "output_text", "text": "Checking." }] },
            { "type": "function_call", "call_id": "call_9", "name": "weather", "arguments": "{}" },
        ]});
        let messages = output_messages(&answer);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "Checking.");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_9");
        assert!(output_messages(&json!({ "output": [] })).is_empty());
    }

    #[test]
    fn the_store_reads_back_deletes_and_stays_bounded() {
        let store = ResponseStore::default();
        store.put(response("resp_a", "A"), vec![json!({ "role": "user", "content": "a" })]);
        assert_eq!(store.get("resp_a").unwrap()["id"], "resp_a");
        assert!(store.delete("resp_a"));
        assert!(!store.delete("resp_a"));
        assert!(store.get("resp_a").is_none());

        for n in 0..MAX_STORED + 10 {
            store.put(response(&format!("resp_{n}"), "x"), Vec::new());
        }
        assert!(store.get("resp_0").is_none(), "the oldest make room");
        assert!(store.get(&format!("resp_{}", MAX_STORED + 9)).is_some());
        let shelf = store.shelf.lock().unwrap();
        assert_eq!(shelf.entries.len(), MAX_STORED);
        assert_eq!(shelf.bytes, shelf.entries.values().map(|e| e.bytes).sum::<usize>());
    }
}
