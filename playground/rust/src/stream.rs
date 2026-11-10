//! `--output-format stream-json`: NDJSON event emitter + SSE chat-completions streaming.

use serde_json::{Value, json};
use std::io::Write;

/// Which delegated task an event belongs to (protocol 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskScope {
    pub task_id: String,
    /// The parent's `Task` tool-call id; ties the task to its card.
    pub parent_call_id: String,
    /// The delegating task, when this one was delegated by another subagent.
    pub parent_task_id: Option<String>,
}

/// Writes one JSON object per line to stdout, flushed after each line.
///
/// A scoped emitter (see [`Emitter::scoped`]) stamps `task_id`, `parent_call_id`
/// and, for nested delegation, `parent_task_id` on every event it emits.
#[derive(Default)]
pub struct Emitter {
    scope: Option<TaskScope>,
    /// Captures events instead of printing them (tests only).
    #[cfg(test)]
    pub sink: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
}

/// `event` with the task scope fields added (unscoped events pass through).
pub fn stamp(mut event: Value, scope: Option<&TaskScope>) -> Value {
    if let (Some(scope), Value::Object(map)) = (scope, &mut event) {
        map.insert("task_id".into(), Value::String(scope.task_id.clone()));
        map.insert("parent_call_id".into(), Value::String(scope.parent_call_id.clone()));
        if let Some(parent) = &scope.parent_task_id {
            map.insert("parent_task_id".into(), Value::String(parent.clone()));
        }
    }
    event
}

/// `{"type":"usage"}` event from an OpenAI-style `usage` object, or None when
/// the endpoint reported nothing. `input` excludes cached prompt tokens, which
/// go in `cacheRead` (OpenCode's convention, so the context meter adds them once).
pub fn usage_event(usage: &Value, model: &str) -> Option<Value> {
    let prompt = usage["prompt_tokens"].as_u64()?;
    let output = usage["completion_tokens"].as_u64().unwrap_or(0);
    let cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(prompt);
    let reasoning = usage["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0);
    Some(json!({
        "type": "usage",
        "model": model,
        "input": prompt - cached,
        "output": output,
        "reasoning": reasoning,
        "cacheRead": cached,
    }))
}

impl Emitter {
    /// An emitter that records events instead of printing them (tests only).
    #[cfg(test)]
    pub fn capturing() -> (Emitter, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (Emitter { scope: None, sink: Some(sink.clone()) }, sink)
    }

    /// An emitter for one delegated task. Nested tasks record their delegator.
    pub fn scoped(&self, task_id: &str, parent_call_id: &str) -> Emitter {
        Emitter {
            scope: Some(TaskScope {
                task_id: task_id.to_string(),
                parent_call_id: parent_call_id.to_string(),
                parent_task_id: self.scope.as_ref().map(|s| s.task_id.clone()),
            }),
            #[cfg(test)]
            sink: self.sink.clone(),
        }
    }

    pub fn emit(&self, event: Value) {
        let event = stamp(event, self.scope.as_ref());
        #[cfg(test)]
        if let Some(sink) = &self.sink {
            sink.lock().expect("event sink").push(event);
            return;
        }
        // One locked write per event keeps lines whole when tasks run concurrently.
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
    /// `usage` from the final chunk (sent when `stream_options.include_usage` is honoured).
    usage: Option<Value>,
    reasoning: String,
    tool_calls: Vec<PartialToolCall>,
    pub done: bool,
    /// Why the model stopped (`stop`, `tool_calls`, `length`), when the endpoint says.
    finish_reason: Option<String>,
    /// The thinking is going round in circles (see `is_runaway`): the caller stops reading.
    pub runaway: bool,
    /// How much thinking had arrived when it was last checked for a loop.
    checked_at: usize,
}

/// `finish_reason` this agent gives a reply it cut off because the thinking was repeating itself.
pub const RUNAWAY: &str = "runaway";

/// How much thinking arrives between two loop checks.
const RUNAWAY_CHECK_EVERY: usize = 1000;

/// Thinking that goes round in circles: its last 240 characters already occur at least four times in its last 6000.
/// A thinking model that degenerates repeats one short block (a list, a sentence) until the reply limit cuts it off,
/// which costs a minute and yields no answer and no tool call; real reasoning does not repeat 240 characters verbatim
/// four times in a row.
pub fn is_runaway(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 2000 {
        return false;
    }
    let tail: String = chars[chars.len() - 240..].iter().collect();
    if tail.trim().is_empty() {
        return false;
    }
    let window: String = chars[chars.len().saturating_sub(6000)..].iter().collect();
    window.matches(tail.as_str()).count() >= 4
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
        if chunk["usage"].is_object() {
            self.usage = Some(chunk["usage"].clone());
        }
        let delta = &chunk["choices"][0]["delta"];
        let text = delta["content"].as_str().unwrap_or("").to_string();
        self.content.push_str(&text);
        for key in ["reasoning", "reasoning_content"] {
            if let Some(r) = delta[key].as_str() {
                self.reasoning.push_str(r);
            }
        }
        if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        if self.reasoning.len() >= self.checked_at + RUNAWAY_CHECK_EVERY {
            self.checked_at = self.reasoning.len();
            // Only while nothing else has come: an answer or a tool call under way is not a loop.
            self.runaway = self.content.trim().is_empty() && self.tool_calls.is_empty() && is_runaway(&self.reasoning);
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
                // Arguments arrive as string fragments (OpenAI) or, from some
                // Ollama models, as one whole JSON object.
                if call["function"]["arguments"].is_object() {
                    slot.arguments = call["function"]["arguments"].to_string();
                } else if let Some(args) = call["function"]["arguments"].as_str() {
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
        if let Some(usage) = self.usage {
            message["usage"] = usage;
        }
        if self.runaway {
            message["finish_reason"] = Value::String(RUNAWAY.into());
        } else if let Some(reason) = self.finish_reason {
            message["finish_reason"] = Value::String(reason);
        }
        message
    }
}

pub enum StreamError {
    /// Failed before any response data arrived — caller may fall back to non-streaming.
    BeforeData(String),
    /// The endpoint refused the request (auth, credits, rate limit): a non-streaming
    /// retry would be refused too, so the caller should stop.
    Rejected(String),
    /// Failed after data arrived (deltas may already be emitted).
    Mid(String),
}

/// Statuses where retrying without streaming can't help (401/402/403/429).
pub fn is_rejection(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 429)
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
    // Ask for token usage in the last chunk. A server that rejects this answers
    // 400 before any data, and the caller retries without streaming.
    body["stream_options"] = json!({ "include_usage": true });
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
        let message = format!("{status} {}", truncate_chars(&text, 300));
        return Err(if is_rejection(status.as_u16()) {
            StreamError::Rejected(message)
        } else {
            StreamError::BeforeData(format!("stream request failed: {message}"))
        });
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
            if acc.done || acc.runaway {
                break;
            }
        }
        if acc.done {
            break;
        }
        if acc.runaway {
            // Stop reading: dropping the response ends the request, so the model stops generating.
            eprintln!("stream: the model's thinking is repeating itself; reply cut off");
            return Ok(acc.into_message());
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
mod usage_tests {
    use super::*;

    #[test]
    fn thinking_that_repeats_itself_is_told_from_long_thinking() {
        let block = "`ls docs/scrs`\n`ls docs/features`\n`ls docs/domains`\n`ls docs/architecture`\n`ls docs/product`\n`ls docs/core`\n`ls docs/setup`\n`ls docs/guides`\n";
        let looping = format!("First I look at the dialog and what it lists.\n{}", block.repeat(40));
        assert!(is_runaway(&looping));
        let long: String = (0..400).map(|i| format!("Step {i}: check file number {i} and compare it with the one before.\n")).collect();
        assert!(!is_runaway(&long), "long but not repeating");
        assert!(!is_runaway(&block.repeat(3)), "too short to call");
        assert!(!is_runaway(&" ".repeat(5000)), "blank padding is not thinking");
    }

    #[test]
    fn a_stream_whose_thinking_loops_is_marked_and_the_finish_reason_is_kept() {
        let mut acc = StreamAccumulator::default();
        let line = serde_json::json!({ "choices": [{ "delta": { "reasoning": "`ls docs/scrs`\n`ls docs/features`\n`ls docs/domains`\n".repeat(8) } }] });
        for _ in 0..12 {
            acc.feed_line(&format!("data: {line}")).unwrap();
        }
        assert!(acc.runaway);
        assert_eq!(acc.into_message()["finish_reason"], RUNAWAY);

        let mut acc = StreamAccumulator::default();
        acc.feed_line(r#"data: {"choices":[{"delta":{"reasoning":"thinking"}}]}"#).unwrap();
        acc.feed_line(r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
        assert_eq!(acc.into_message()["finish_reason"], "length");

        // An answer under way is never cut off, whatever the thinking before it looked like.
        let mut acc = StreamAccumulator::default();
        acc.feed_line(r#"data: {"choices":[{"delta":{"content":"The answer"}}]}"#).unwrap();
        for _ in 0..12 {
            acc.feed_line(&format!("data: {line}")).unwrap();
        }
        assert!(!acc.runaway);
    }

    #[test]
    fn final_chunk_usage_reaches_the_message() {
        let mut acc = StreamAccumulator::default();
        acc.feed_line(r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
        acc.feed_line(r#"data: {"choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":200}}}"#).unwrap();
        let message = acc.into_message();
        assert_eq!(message["content"], "hi");
        let event = usage_event(&message["usage"], "qwen3:8b").unwrap();
        assert_eq!(event["input"], 1000);
        assert_eq!(event["cacheRead"], 200);
        assert_eq!(event["output"], 30);
        assert_eq!(event["model"], "qwen3:8b");
    }

    #[test]
    fn no_usage_no_event() {
        assert!(usage_event(&Value::Null, "m").is_none());
        let acc = StreamAccumulator::default();
        assert!(acc.into_message().get("usage").is_none());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scoped_events_carry_task_ids_and_nesting() {
        let (root, sink) = Emitter::capturing();
        root.emit(json!({ "type": "delta", "text": "hi" }));
        let child = root.scoped("task_a", "call_1");
        child.emit(json!({ "type": "tool", "id": "c" }));
        let grandchild = child.scoped("task_b", "call_9");
        grandchild.emit(json!({ "type": "delta", "text": "x" }));

        let events = sink.lock().unwrap().clone();
        assert!(events[0].get("task_id").is_none(), "root events stay unstamped");
        assert_eq!(events[1]["task_id"], "task_a");
        assert_eq!(events[1]["parent_call_id"], "call_1");
        assert!(events[1].get("parent_task_id").is_none());
        assert_eq!(events[2]["task_id"], "task_b");
        assert_eq!(events[2]["parent_task_id"], "task_a");
        assert_eq!(events[2]["parent_call_id"], "call_9");
        // Non-object events are left alone.
        assert_eq!(stamp(json!("x"), None), json!("x"));
    }


    #[test]
    fn object_arguments_are_kept() {
        let mut acc = StreamAccumulator::default();
        acc.feed_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"Write","arguments":{"filePath":"a.md","content":"x"}}}]}}]}"#,
        )
        .unwrap();
        let message = acc.into_message();
        let args = message["tool_calls"][0]["function"]["arguments"].as_str().unwrap().to_string();
        assert!(args.contains("\"filePath\":\"a.md\""));
    }

    #[test]
    fn rejections_do_not_fall_back() {
        assert!(is_rejection(429));
        assert!(is_rejection(401));
        assert!(!is_rejection(400));
        assert!(!is_rejection(404));
        assert!(!is_rejection(500));
    }

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
