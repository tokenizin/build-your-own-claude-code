//! `--output-format stream-json`: NDJSON event emitter + SSE chat-completions streaming.

use serde_json::{Value, json};
use std::io::Write;

/// Writes one JSON object per line to stdout, flushed after each line.
pub struct Emitter;

impl Emitter {
    pub fn emit(&self, event: Value) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{event}");
        let _ = out.flush();
    }

    pub fn delta(&self, text: &str) {
        if !text.is_empty() {
            self.emit(json!({ "type": "delta", "text": text }));
        }
    }
}

/// First `max` chars of `s` (char-boundary safe).
pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

pub fn is_error_output(output: &str) -> bool {
    output.starts_with("Error") || output.starts_with("Unknown tool") || output.starts_with("Failed")
}

/// Splits a byte stream into complete lines (handles UTF-8 split across chunks).
#[derive(Default)]
pub struct SseLineBuffer {
    buf: Vec<u8>,
}

impl SseLineBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            lines.push(text.trim_end_matches(['\n', '\r']).to_string());
        }
        lines
    }

    /// Remaining partial line at end of stream.
    pub fn finish(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&self.buf).trim().to_string();
        self.buf.clear();
        (!text.is_empty()).then_some(text)
    }
}

#[derive(Default, Debug, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, PartialEq)]
pub enum SseEvent {
    /// Assistant text increment (may be empty).
    Delta(String),
    Done,
    Ignored,
}

/// Accumulates `chat.completion.chunk` SSE payloads into one assistant message.
#[derive(Default)]
pub struct StreamAccumulator {
    content: String,
    reasoning: String,
    tool_calls: Vec<PartialToolCall>,
    pub done: bool,
}

impl StreamAccumulator {
    /// Feed one SSE line. Returns an error for an in-band `{"error": ...}` payload.
    pub fn feed_line(&mut self, line: &str) -> Result<SseEvent, String> {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(SseEvent::Ignored);
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(SseEvent::Done);
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Ok(SseEvent::Ignored);
        };
        if let Some(err) = chunk.get("error") {
            let msg = err["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            return Err(msg);
        }
        let delta = &chunk["choices"][0]["delta"];
        let text = delta["content"].as_str().unwrap_or("").to_string();
        self.content.push_str(&text);
        for key in ["reasoning", "reasoning_content"] {
            if let Some(r) = delta[key].as_str() {
                self.reasoning.push_str(r);
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for (pos, call) in calls.iter().enumerate() {
                let index = call["index"].as_u64().map(|i| i as usize).unwrap_or(pos);
                if self.tool_calls.len() <= index {
                    self.tool_calls.resize(index + 1, PartialToolCall::default());
                }
                let slot = &mut self.tool_calls[index];
                if let Some(id) = call["id"].as_str() {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    slot.name.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    slot.arguments.push_str(args);
                }
            }
        }
        Ok(SseEvent::Delta(text))
    }

    /// Message in the same shape as a non-streaming `choices[0].message`.
    pub fn into_message(self) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": if self.content.is_empty() { Value::Null } else { Value::String(self.content) },
        });
        if !self.reasoning.is_empty() {
            message["reasoning"] = Value::String(self.reasoning);
        }
        let calls: Vec<Value> = self
            .tool_calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| {
                let id = if c.id.is_empty() { format!("call_{i}") } else { c.id };
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": if c.arguments.is_empty() { "{}".to_string() } else { c.arguments },
                    }
                })
            })
            .collect();
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        message
    }
}

pub enum StreamError {
    /// Failed before any response data arrived — caller may fall back to non-streaming.
    BeforeData(String),
    /// Failed after data arrived (deltas may already be emitted).
    Mid(String),
}

/// POST `{base}/chat/completions` with `stream: true`, emitting content deltas as they arrive.
/// Returns the accumulated assistant message.
pub async fn stream_chat(
    base_url: &str,
    api_key: &str,
    request: &Value,
    emitter: &Emitter,
) -> Result<Value, StreamError> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut body = request.clone();
    body["stream"] = Value::Bool(true);
    let payload = serde_json::to_vec(&body).map_err(|e| StreamError::BeforeData(e.to_string()))?;

    let client = reqwest::Client::new();
    let mut resp = client
        .post(&url)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(payload)
        .send()
        .await
        .map_err(|e| StreamError::BeforeData(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(StreamError::BeforeData(format!(
            "stream request failed: {status} {}",
            truncate_chars(&text, 300)
        )));
    }

    let mut lines = SseLineBuffer::default();
    let mut acc = StreamAccumulator::default();
    let mut got_data = false;
    let handle = |line: &str, acc: &mut StreamAccumulator| -> Result<(), String> {
        if let SseEvent::Delta(text) = acc.feed_line(line)? {
            emitter.delta(&text);
        }
        Ok(())
    };

    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) if !got_data => return Err(StreamError::BeforeData(e.to_string())),
            Err(e) => return Err(StreamError::Mid(e.to_string())),
        };
        got_data = true;
        for line in lines.push(&chunk) {
            handle(&line, &mut acc).map_err(StreamError::Mid)?;
            if acc.done {
                break;
            }
        }
        if acc.done {
            break;
        }
    }
    if let Some(rest) = lines.finish() {
        handle(&rest, &mut acc).map_err(StreamError::Mid)?;
    }
    if !got_data {
        return Err(StreamError::BeforeData("empty stream response".into()));
    }
    Ok(acc.into_message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_buffer_splits_across_chunks() {
        let mut b = SseLineBuffer::default();
        assert!(b.push(b"data: {\"a\"").is_empty());
        let lines = b.push(b":1}\r\n\ndata: [DONE]\n");
        assert_eq!(lines, vec!["data: {\"a\":1}", "", "data: [DONE]"]);
        // UTF-8 split mid-codepoint
        let mut b = SseLineBuffer::default();
        let s = "data: é\n".as_bytes();
        assert!(b.push(&s[..7]).is_empty());
        assert_eq!(b.push(&s[7..]), vec!["data: é"]);
        assert!(b.finish().is_none());
    }

    #[test]
    fn accumulates_text_deltas() {
        let mut acc = StreamAccumulator::default();
        let ev = acc
            .feed_line(r#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#)
            .unwrap();
        assert_eq!(ev, SseEvent::Delta(String::new()));
        assert_eq!(
            acc.feed_line(r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#).unwrap(),
            SseEvent::Delta("Hel".into())
        );
        acc.feed_line(r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#).unwrap();
        assert_eq!(acc.feed_line(": keepalive").unwrap(), SseEvent::Ignored);
        assert_eq!(acc.feed_line("data: [DONE]").unwrap(), SseEvent::Done);
        assert!(acc.done);
        let msg = acc.into_message();
        assert_eq!(msg["content"], "Hello");
        assert!(msg.get("tool_calls").is_none());
    }

    #[test]
    fn accumulates_tool_calls_by_index() {
        let mut acc = StreamAccumulator::default();
        for line in [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"Read","arguments":"{\"file_path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"echo hi\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"a\"}"}}]}}]}"#,
            "data: [DONE]",
        ] {
            acc.feed_line(line).unwrap();
        }
        let msg = acc.into_message();
        assert_eq!(msg["content"], Value::Null);
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"command":"echo hi"}"#);
        assert_eq!(calls[1]["function"]["name"], "Read");
        assert_eq!(calls[1]["function"]["arguments"], r#"{"file_path":"a"}"#);
    }

    #[test]
    fn reasoning_and_errors() {
        let mut acc = StreamAccumulator::default();
        acc.feed_line(r#"data: {"choices":[{"delta":{"reasoning":"think\nanswer"}}]}"#)
            .unwrap();
        assert_eq!(acc.into_message()["reasoning"], "think\nanswer");
        let mut acc = StreamAccumulator::default();
        assert_eq!(
            acc.feed_line(r#"data: {"error":{"message":"bad model"}}"#),
            Err("bad model".to_string())
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("hi", 2000), "hi");
        assert!(is_error_output("Error reading x"));
        assert!(is_error_output("Unknown tool: X"));
        assert!(is_error_output("Failed to run command"));
        assert!(!is_error_output("ok"));
    }
}
