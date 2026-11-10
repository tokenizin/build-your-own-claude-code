//! Subagent delegation (the OpenCode-compatible `Task` tool) and tolerant
//! tool-call arguments.
//!
//! NomadWorks agent prompts (product_manager, pma, tech_lead, …) are written for
//! OpenCode, where delegation goes through a `task` tool:
//! `{ subagent_type, description, prompt, task_id? }` → `task_id: …` plus the
//! subagent's answer, and the same `task_id` resumes that subagent later.
//! This module gives the Rust agent the same tool, so those prompts work as-is.

use crate::skills::Skill;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// How deep delegation may nest (PMA → tech_lead → developer).
pub const MAX_TASK_DEPTH: u8 = 2;

/// Fork agents (skills with `context: fork`) a Task call can target.
pub fn task_agents(skills: &[Skill]) -> Vec<&Skill> {
    skills.iter().filter(|s| s.context.as_deref() == Some("fork")).collect()
}

fn short(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

/// The `Task` tool definition, listing the agents it can delegate to.
/// Tells the model that Task calls made in one reply run at the same time. On unless `NOMAD_TASK_FANOUT_HINT=off`
/// (which is how the difference it makes was measured).
fn fan_out_hint() -> &'static str {
    if std::env::var("NOMAD_TASK_FANOUT_HINT").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        ""
    } else {
        "\n\nLaunch several subagents at once whenever their work is independent: make all the Task calls in a single reply and they run at the same time (the way to get three reviews, or a spec and a design, in the time of one). Wait to call a subagent only when it needs another one's answer."
    }
}

pub fn task_tool_spec(agents: &[&Skill]) -> Value {
    let names: Vec<&str> = agents.iter().map(|s| s.name.as_str()).collect();
    let roster = agents
        .iter()
        .map(|s| format!("- {}: {}", s.name, short(&s.description, 110)))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "type": "function",
        "function": {
            "name": "Task",
            "description": format!(
                "Delegate a task to a specialist subagent. The subagent works on its own \
(with tools), then returns its final answer and a task_id. Pass the same task_id \
to continue with that subagent (follow-ups, bounce-backs). Give it everything it \
needs in the prompt: it does not see this conversation.{}\n\nAvailable subagents:\n{roster}",
                fan_out_hint()
            ),
            "parameters": {
                "type": "object",
                "required": ["subagent_type", "description", "prompt"],
                "properties": {
                    "subagent_type": {
                        "type": "string",
                        "enum": names,
                        "description": "Which subagent to use"
                    },
                    "description": {
                        "type": "string",
                        "description": "Short (3-5 words) description of the task"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The full task for the subagent, including file paths and acceptance criteria"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Resume an earlier task with this subagent (from a previous Task result)"
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional: only to run this task on a different model the endpoint serves. Leave it out to use the agent's own model (almost always right)."
                    }
                }
            }
        }
    })
}

/// The endpoint's id for a model named in a Task call: an `ollama/` prefix (OpenCode's provider
/// notation, as in the agent pins) is dropped; anything else is passed through unchanged.
pub fn endpoint_model(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let id = trimmed.strip_prefix("ollama/").unwrap_or(trimmed).trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Tool names models use for the same tools (OpenCode is lowercase), and close misses.
pub fn canonical_tool_name(name: &str) -> &str {
    match name.trim() {
        "task" | "Task" | "delegate" | "Agent" | "agent" => "Task",
        "read" | "Read" | "read_file" => "Read",
        "write" | "Write" | "write_file" => "Write",
        "bash" | "Bash" | "shell" | "sh" => "Bash",
        "skill" | "Skill" => "Skill",
        "handover" | "Handover" | "file_handover" | "submit_handover" => "handover",
        "apply_patch" | "applypatch" | "ApplyPatch" | "patch" => "apply_patch",
        "edit" | "Edit" | "edit_file" | "str_replace" | "str_replace_editor" => "Edit",
        "grep" | "Grep" | "search" | "ripgrep" | "search_files" => "Grep",
        "glob" | "Glob" | "find_files" | "list_files" => "Glob",
        "webfetch" | "WebFetch" | "web_fetch" | "fetch" | "fetch_url" => "WebFetch",
        "todowrite" | "TodoWrite" | "todo_write" | "update_todos" | "write_todos" => "TodoWrite",
        "todoread" | "TodoRead" | "todo_read" | "read_todos" => "TodoRead",
        "nomadflow_validate" | "nomadworks_validate" | "nomad_validate" => "nomadworks_validate",
        other => other,
    }
}

/// Tool-call arguments as an object, whether the endpoint sent a JSON string
/// (OpenAI style) or an object (some Ollama models/versions).
pub fn parse_arguments(raw: &Value) -> Value {
    match raw {
        Value::Object(_) => raw.clone(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return json!({});
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Object(map)) => Value::Object(map),
                // Some models double-encode: "\"{...}\"".
                Ok(Value::String(inner)) => match serde_json::from_str::<Value>(&inner) {
                    Ok(Value::Object(map)) => Value::Object(map),
                    _ => json!({}),
                },
                _ => json!({}),
            }
        }
        _ => json!({}),
    }
}

/// Fills a tool's expected argument names from common alternatives.
pub fn apply_aliases(tool: &str, args: Value) -> Value {
    let Value::Object(mut map) = args else { return json!({}) };
    let aliases: &[(&str, &[&str])] = match tool {
        "Read" => &[("file_path", &["filePath", "path", "file", "filename"])],
        "Write" => &[
            ("file_path", &["filePath", "path", "file", "filename"]),
            ("content", &["contents", "text", "data", "body"]),
        ],
        "Bash" => &[("command", &["cmd", "script", "shell"])],
        "Edit" => &[
            ("file_path", &["filePath", "path", "file", "filename"]),
            ("old_string", &["oldString", "old", "search", "find", "old_text", "oldText"]),
            ("new_string", &["newString", "new", "replace", "replacement", "new_text", "newText"]),
            ("replace_all", &["replaceAll", "all"]),
        ],
        "Grep" => &[("pattern", &["regex", "query", "search", "regexp"]), ("path", &["directory", "dir", "folder"]), ("include", &["glob", "files", "filePattern"])],
        "Glob" => &[("pattern", &["glob", "match", "name"]), ("path", &["directory", "dir", "folder"])],
        "WebFetch" => &[("url", &["uri", "link", "href"])],
        "TodoWrite" => &[("todos", &["items", "list", "tasks", "plan"])],
        "Skill" => &[("name", &["skill", "skill_name"]), ("args", &["arguments", "input"])],
        "Task" => &[
            ("subagent_type", &["subagentType", "agent", "subagent", "agent_type", "role", "subagent_name"]),
            ("prompt", &["instructions", "task", "message", "input", "subagent_prompt", "subagent_instructions"]),
            ("description", &["title", "summary", "subagent_description"]),
            ("task_id", &["taskId", "session_id", "sessionId"]),
        ],
        "nomadflow_run_workflow" => &[
            ("task_path", &["taskPath", "task_file", "taskFile", "path", "file_path", "filePath", "task"]),
            ("instructions", &["prompt", "instruction", "description", "message"]),
            ("session_id", &["sessionId", "workflow_session", "workflowSession"]),
        ],
        "nomadflow_prompt_workflow" => &[
            ("session_id", &["sessionId", "workflow_session", "workflowSession", "id"]),
            ("text", &["message", "prompt", "instructions", "instruction", "follow_up", "followUp"]),
        ],
        _ => &[],
    };
    for (target, alternatives) in aliases {
        let has_target = map.get(*target).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty());
        if has_target {
            continue;
        }
        if let Some(value) = alternatives
            .iter()
            .find_map(|alt| map.get(*alt).filter(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())).cloned())
        {
            map.insert((*target).to_string(), value);
        }
    }
    // A Task with no description still has something to show.
    if tool == "Task" && !map.contains_key("description") {
        if let Some(agent) = map.get("subagent_type").and_then(Value::as_str) {
            map.insert("description".into(), Value::String(format!("Task for {agent}")));
        }
    }
    Value::Object(map)
}

/// Finds the agent a Task names: exact, then ignoring case and `-`/`_`.
pub fn find_agent<'a>(agents: &[&'a Skill], name: &str) -> Option<&'a Skill> {
    let wanted = name.trim();
    if let Some(skill) = agents.iter().find(|s| s.name == wanted) {
        return Some(skill);
    }
    let norm = |s: &str| s.to_lowercase().replace(['-', ' '], "_");
    let wanted = norm(wanted);
    agents.iter().find(|s| norm(&s.name) == wanted).copied()
}

/// What to tell the model when it calls a tool that doesn't exist.
pub fn unknown_tool_message(name: &str, tools: Option<&Value>) -> String {
    let names: Vec<String> = tools
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        format!("Unknown tool: {name}. No tools are available in this context.")
    } else {
        format!("Unknown tool: {name}. Use one of: {}.", names.join(", "))
    }
}

// ---------------------------------------------------------------- tool calls written as text

/// Some local models write a tool call into the reply text instead of the
/// `tool_calls` field — always when the call is cut off by the token limit,
/// since the endpoint can't parse an unfinished call. Recognized:
///
/// - XML (Qwen3-Coder / Nemotron): `<function=NAME><parameter=KEY>value</parameter>…</function>`,
///   closing tags optional
/// - Hermes: `<tool_call>{"name": …, "arguments": {…}}</tool_call>`
/// - bare `<parameter=subagent_…>` blocks with no function tag: a Task call
///
/// Returns the text before the first call and the calls in OpenAI shape.
pub fn extract_text_tool_calls(text: &str) -> Option<(String, Vec<Value>)> {
    if text.contains(GEMMA_OPEN) {
        return extract_gemma_tool_calls(text);
    }
    let Some(first) = ["<tool_call>", "<function=", "<parameter="].iter().filter_map(|tag| text.find(tag)).min() else {
        return extract_fenced_task_calls(text);
    };
    let before = text[..first].trim_end().to_string();
    let rest = &text[first..];

    let mut calls = Vec::new();
    // Hermes JSON blocks.
    for chunk in rest.split("<tool_call>").skip(1) {
        let body = chunk.split("</tool_call>").next().unwrap_or("").trim();
        if !body.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            if let Some(name) = v["name"].as_str() {
                let args = if v["arguments"].is_null() { v["parameters"].clone() } else { v["arguments"].clone() };
                calls.push(openai_call(name, parse_arguments(&args), calls.len()));
            }
        }
    }
    if calls.is_empty() {
        // XML blocks, one per <function=…>; parameters before any function tag mean Task.
        let mut pieces: Vec<(Option<String>, &str)> = Vec::new();
        let mut cursor = rest;
        if !cursor.starts_with("<function=") && !cursor.contains("<function=") {
            pieces.push((None, cursor));
        } else {
            while let Some(start) = cursor.find("<function=") {
                let after = &cursor[start + "<function=".len()..];
                let name_end = after.find('>').unwrap_or(after.len());
                let name = after[..name_end].trim().to_string();
                let body_start = (name_end + 1).min(after.len());
                let body = &after[body_start..];
                let body_end = body.find("<function=").unwrap_or(body.len());
                pieces.push((Some(name), &body[..body_end]));
                cursor = &body[body_end..];
            }
        }
        for (name, body) in pieces {
            let params = xml_parameters(body);
            if params.is_empty() {
                continue;
            }
            let name = match name {
                Some(n) if !n.is_empty() => n,
                _ if params.keys().any(|k| k.contains("subagent") || k == "prompt") => "Task".to_string(),
                _ => continue,
            };
            calls.push(openai_call(&name, Value::Object(params), calls.len()));
        }
    }
    if calls.is_empty() { None } else { Some((before, calls)) }
}

const GEMMA_OPEN: &str = "<|tool_call>";
const GEMMA_CLOSE: &str = "<tool_call|>";
const GEMMA_QUOTE: &str = "<|\"|>";

/// Gemma's own tool-call tokens, which reach the agent as text when the server does not turn them into a call (seen with
/// `gemma4:26b` on Ollama, mid-run): `<|tool_call>call:NAME{key:<|"|>text<|"|>,n:5,list:[…]}<tool_call|>`. Keys are bare,
/// strings sit between `<|"|>` marks. Whatever follows the last call (`<|tool_response>`…) is the model going on by
/// itself and is dropped.
fn extract_gemma_tool_calls(text: &str) -> Option<(String, Vec<Value>)> {
    let first = text.find(GEMMA_OPEN)?;
    let before = text[..first].trim_end().to_string();
    let mut calls = Vec::new();
    for chunk in text[first..].split(GEMMA_OPEN).skip(1) {
        let body = chunk.split(GEMMA_CLOSE).next().unwrap_or("").trim();
        let Some(body) = body.strip_prefix("call:") else { continue };
        let brace = body.find('{').unwrap_or(body.len());
        let name = body[..brace].trim();
        if name.is_empty() {
            continue;
        }
        let chars: Vec<char> = body[brace..].chars().collect();
        let mut at = 0;
        let args = if chars.is_empty() { json!({}) } else { gemma_value(&chars, &mut at) };
        if args.is_object() {
            calls.push(openai_call(name, args, calls.len()));
        }
    }
    if calls.is_empty() { None } else { Some((before, calls)) }
}

/// One value of Gemma's argument notation, from `chars[*at..]`. Lenient: a value cut off by the reply limit ends where
/// the text does.
fn gemma_value(chars: &[char], at: &mut usize) -> Value {
    let starts = |at: usize, pat: &str| pat.chars().enumerate().all(|(i, c)| chars.get(at + i) == Some(&c));
    while chars.get(*at).is_some_and(|c| c.is_whitespace()) {
        *at += 1;
    }
    if starts(*at, GEMMA_QUOTE) {
        *at += GEMMA_QUOTE.chars().count();
        let begin = *at;
        while *at < chars.len() && !starts(*at, GEMMA_QUOTE) {
            *at += 1;
        }
        let text: String = chars[begin..*at].iter().collect();
        *at = (*at + GEMMA_QUOTE.chars().count()).min(chars.len());
        return Value::String(text);
    }
    match chars.get(*at) {
        Some('{') => {
            *at += 1;
            let mut map = serde_json::Map::new();
            loop {
                while chars.get(*at).is_some_and(|c| c.is_whitespace() || *c == ',') {
                    *at += 1;
                }
                match chars.get(*at) {
                    None => break,
                    Some('}') => {
                        *at += 1;
                        break;
                    }
                    _ => {}
                }
                let begin = *at;
                while chars.get(*at).is_some_and(|c| *c != ':') {
                    *at += 1;
                }
                let key: String = chars[begin..*at].iter().collect::<String>().trim().trim_matches('"').to_string();
                if *at >= chars.len() || key.is_empty() {
                    break;
                }
                *at += 1;
                map.insert(key, gemma_value(chars, at));
            }
            Value::Object(map)
        }
        Some('[') => {
            *at += 1;
            let mut items = Vec::new();
            loop {
                while chars.get(*at).is_some_and(|c| c.is_whitespace() || *c == ',') {
                    *at += 1;
                }
                match chars.get(*at) {
                    None => break,
                    Some(']') => {
                        *at += 1;
                        break;
                    }
                    _ => {
                        let begin = *at;
                        let item = gemma_value(chars, at);
                        if *at == begin {
                            // A stray `}` where an item should be: nothing was read, so step over it or the list never ends.
                            *at += 1;
                        } else {
                            items.push(item);
                        }
                    }
                }
            }
            Value::Array(items)
        }
        Some('"') => {
            *at += 1;
            let begin = *at;
            while chars.get(*at).is_some_and(|c| *c != '"') {
                *at += 1;
            }
            let text: String = chars[begin..*at].iter().collect();
            *at = (*at + 1).min(chars.len());
            Value::String(text)
        }
        _ => {
            let begin = *at;
            while chars.get(*at).is_some_and(|c| !matches!(c, ',' | '}' | ']')) {
                *at += 1;
            }
            let raw: String = chars[begin..*at].iter().collect::<String>().trim().to_string();
            serde_json::from_str::<Value>(&raw).ok().filter(|v| !v.is_object() && !v.is_array()).unwrap_or(Value::String(raw))
        }
    }
}

/// Tools a model may write out as a fenced `{name, arguments}` block and still have them run: they only hand work on.
const COORDINATION_TOOLS: [&str; 4] = ["Task", "nomadflow_run_workflow", "nomadflow_prompt_workflow", "nomadworks_validate"];

/// A model that says "I will send these requests" and writes each one as a fenced JSON block, instead of calling the
/// tool: every block that is a Task call (`subagent_type` and `prompt`), alone or in a list, becomes one. Nothing else in
/// a fence is touched. Returns the text before the first block and the calls.
fn extract_fenced_task_calls(text: &str) -> Option<(String, Vec<Value>)> {
    let mut calls = Vec::new();
    let mut first_fence: Option<usize> = None;
    let mut rest = text;
    let mut offset = 0;
    while let Some(open) = rest.find("```") {
        let after_open = &rest[open + 3..];
        let Some(line_end) = after_open.find('\n') else { break };
        let body_start = open + 3 + line_end + 1;
        let Some(close) = rest[body_start..].find("```") else { break };
        let body = rest[body_start..body_start + close].trim();
        let candidates: Vec<Value> = match serde_json::from_str::<Value>(body) {
            Ok(Value::Array(list)) => list,
            Ok(one @ Value::Object(_)) => vec![one],
            _ => Vec::new(),
        };
        for v in candidates {
            if v["subagent_type"].as_str().is_some_and(|t| !t.trim().is_empty()) && v["prompt"].as_str().is_some_and(|p| !p.trim().is_empty()) {
                first_fence.get_or_insert(offset + open);
                calls.push(openai_call("Task", v, calls.len()));
            } else if let Some(name) = v["name"].as_str().filter(|n| COORDINATION_TOOLS.contains(n)) {
                // `{ "name": "nomadflow_prompt_workflow", "arguments": { … } }`: only the tools that hand work on, never one that writes.
                let args = if v["arguments"].is_null() { v["parameters"].clone() } else { v["arguments"].clone() };
                if args.is_object() || args.is_string() {
                    first_fence.get_or_insert(offset + open);
                    calls.push(openai_call(name, parse_arguments(&args), calls.len()));
                }
            }
        }
        let consumed = body_start + close + 3;
        offset += consumed;
        rest = &rest[consumed..];
    }
    let first = first_fence?;
    Some((text[..first].trim_end().to_string(), calls))
}

fn xml_parameters(body: &str) -> Map<String, Value> {
    let mut params = Map::new();
    for chunk in body.split("<parameter=").skip(1) {
        let Some(key_end) = chunk.find('>') else { continue };
        let key = chunk[..key_end].trim();
        if key.is_empty() {
            continue;
        }
        let mut value = &chunk[key_end + 1..];
        for closer in ["</parameter>", "</function>", "</tool_call>"] {
            if let Some(pos) = value.find(closer) {
                value = &value[..pos];
            }
        }
        params.insert(key.to_string(), Value::String(value.trim().to_string()));
    }
    params
}

fn openai_call(name: &str, args: Value, index: usize) -> Value {
    json!({
        "id": format!("text_call_{index}"),
        "type": "function",
        "function": { "name": name, "arguments": args.to_string() }
    })
}

// ---------------------------------------------------------------- task memory

/// A delegated task's conversation, kept so the same task_id can be resumed.
pub struct TaskRecord {
    pub agent: String,
    pub messages: Vec<Value>,
    /// What the kind of task needs to find again on resume (a workflow session: its task path and handover).
    pub meta: Option<Value>,
}

fn memory() -> &'static Mutex<HashMap<String, TaskRecord>> {
    static TASKS: OnceLock<Mutex<HashMap<String, TaskRecord>>> = OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where task conversations are saved between runs (CodeNomad sets this per session).
fn store_dir() -> Option<PathBuf> {
    std::env::var("CODENOMAD_TASK_STORE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where the current session keeps its task records (and other per-session state), if CodeNomad set one.
pub fn store_path() -> Option<PathBuf> {
    store_dir()
}

fn safe_id(task_id: &str) -> bool {
    !task_id.is_empty() && task_id.len() <= 100 && task_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn new_task_id() -> String {
    new_id("task")
}

/// A fresh id with the given prefix (`task`, `wf` for workflow sessions).
pub fn new_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{prefix}_{:x}{:x}", nanos, std::process::id())
}

pub fn load_task(task_id: &str) -> Option<TaskRecord> {
    if !safe_id(task_id) {
        return None;
    }
    if let Some(record) = memory().lock().ok()?.get(task_id) {
        return Some(TaskRecord { agent: record.agent.clone(), messages: record.messages.clone(), meta: record.meta.clone() });
    }
    let path = store_dir()?.join(format!("{task_id}.json"));
    let parsed: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    Some(TaskRecord {
        agent: parsed["agent"].as_str()?.to_string(),
        messages: parsed["messages"].as_array()?.clone(),
        meta: parsed.get("meta").filter(|m| !m.is_null()).cloned(),
    })
}

pub fn save_task(task_id: &str, record: TaskRecord) {
    if !safe_id(task_id) {
        return;
    }
    if let Some(dir) = store_dir() {
        if fs::create_dir_all(&dir).is_ok() {
            let mut doc = Map::new();
            doc.insert("agent".into(), Value::String(record.agent.clone()));
            doc.insert("messages".into(), Value::Array(record.messages.clone()));
            if let Some(meta) = &record.meta {
                doc.insert("meta".into(), meta.clone());
            }
            let _ = fs::write(dir.join(format!("{task_id}.json")), Value::Object(doc).to_string());
        }
    }
    if let Ok(mut map) = memory().lock() {
        map.insert(task_id.to_string(), record);
    }
}

/// The Task tool's result, in OpenCode's shape.
pub fn task_result(task_id: &str, answer: &str) -> String {
    format!("task_id: {task_id} (for resuming to continue this task if needed)\n\n<task_result>\n{answer}\n</task_result>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, fork: bool) -> Skill {
        Skill {
            name: name.into(),
            description: format!("{name} agent"),
            body: "body".into(),
            directory: PathBuf::from("/tmp"),
            context: if fork { Some("fork".into()) } else { None },
            policies: vec![],
            model: None,
        }
    }

    #[test]
    fn fenced_json_task_requests_become_tool_calls() {
        let text = "I will send the requests.\n\nTo the BA:\n```json\n{\"subagent_type\":\"business_analyst\",\"description\":\"Review\",\"prompt\":\"Review docs/export.md\"}\n```\nand the lead:\n```\n{\"subagent_type\":\"tech_lead\",\"prompt\":\"Review it too\"}\n```\nDone.";
        let (before, calls) = extract_text_tool_calls(text).expect("two calls");
        assert_eq!(before, "I will send the requests.\n\nTo the BA:");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "Task");
        let args: Value = serde_json::from_str(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!((args["subagent_type"].as_str(), args["prompt"].as_str()), (Some("tech_lead"), Some("Review it too")));
        // A list in one block works; other fenced JSON, code and prose are left alone.
        let list = "```json\n[{\"subagent_type\":\"a\",\"prompt\":\"x\"},{\"subagent_type\":\"b\",\"prompt\":\"y\"}]\n```";
        assert_eq!(extract_text_tool_calls(list).unwrap().1.len(), 2);
        assert!(extract_text_tool_calls("```json\n{\"name\":\"x\",\"value\":1}\n```").is_none());
        assert!(extract_text_tool_calls("```rust\nfn main() {}\n```").is_none());
        assert!(extract_text_tool_calls("```json\n{\"subagent_type\":\"a\"}\n```").is_none(), "no prompt: not a request");
        assert!(extract_text_tool_calls("plain answer").is_none());

        // `{name, arguments}` for a tool that only hands work on; never for one that writes.
        let prompt = "I will send it back.\n```json\n{ \"name\": \"nomadflow_prompt_workflow\", \"arguments\": { \"session_id\": \"ses_lcl_wf_x\", \"text\": \"write the file\" } }\n```";
        let (before, calls) = extract_text_tool_calls(prompt).unwrap();
        assert_eq!((before.as_str(), calls[0]["function"]["name"].as_str()), ("I will send it back.", Some("nomadflow_prompt_workflow")));
        let args: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["session_id"], "ses_lcl_wf_x");
        assert!(extract_text_tool_calls("```json\n{ \"name\": \"Write\", \"arguments\": { \"file_path\": \"/etc/passwd\", \"content\": \"x\" } }\n```").is_none());
        assert!(extract_text_tool_calls("```json\n{ \"name\": \"Bash\", \"arguments\": { \"command\": \"rm -rf /\" } }\n```").is_none());
    }

    #[test]
    fn arguments_as_object_string_or_double_encoded() {
        assert_eq!(parse_arguments(&json!({"a": 1})), json!({"a": 1}));
        assert_eq!(parse_arguments(&json!("{\"a\":1}")), json!({"a": 1}));
        assert_eq!(parse_arguments(&json!("\"{\\\"a\\\":1}\"")), json!({"a": 1}));
        assert_eq!(parse_arguments(&json!("")), json!({}));
        assert_eq!(parse_arguments(&json!("not json")), json!({}));
        assert_eq!(parse_arguments(&Value::Null), json!({}));
    }

    #[test]
    fn aliases_fill_expected_names() {
        let w = apply_aliases("Write", json!({"filePath": "a.md", "content": "x"}));
        assert_eq!(w["file_path"], "a.md");
        let n = apply_aliases("nomadflow_run_workflow", json!({"taskPath": "tasks/1.md", "prompt": "do it"}));
        assert_eq!(n["task_path"], "tasks/1.md");
        assert_eq!(n["instructions"], "do it");
        let t = apply_aliases("Task", json!({"agent": "ba", "instructions": "spec it"}));
        assert_eq!(t["subagent_type"], "ba");
        assert_eq!(t["prompt"], "spec it");
        assert_eq!(t["description"], "Task for ba");
        // An existing value wins over aliases.
        let keep = apply_aliases("Read", json!({"file_path": "x", "path": "y"}));
        assert_eq!(keep["file_path"], "x");
    }

    #[test]
    fn tool_names_and_agents() {
        assert_eq!(canonical_tool_name("task"), "Task");
        assert_eq!(canonical_tool_name("bash"), "Bash");
        assert_eq!(canonical_tool_name("nomadflow_validate"), "nomadworks_validate");
        assert_eq!(canonical_tool_name("nomadflow_run_workflow"), "nomadflow_run_workflow");
        let skills = vec![skill("business_analyst", true), skill("tech-lead", true), skill("plain", false)];
        let agents = task_agents(&skills);
        assert_eq!(agents.len(), 2);
        assert_eq!(find_agent(&agents, "business_analyst").unwrap().name, "business_analyst");
        assert_eq!(find_agent(&agents, "Tech_Lead").unwrap().name, "tech-lead");
        assert!(find_agent(&agents, "plain").is_none());
        let spec = task_tool_spec(&agents);
        assert_eq!(spec["function"]["name"], "Task");
        assert_eq!(spec["function"]["parameters"]["properties"]["subagent_type"]["enum"], json!(["business_analyst", "tech-lead"]));
    }

    #[test]
    fn gemmas_own_tool_call_tokens_written_as_text_are_run() {
        // As gemma4:26b sent it through Ollama, mid-run.
        let seen = "Let's check `usePromptPicker.ts`.<|tool_call>call:Read{file_path:<|\"|>CodeNomad/packages/ui/src/components/prompt-input/usePromptPicker.ts<|\"|>}<tool_call|><|tool_response>";
        let (before, calls) = extract_text_tool_calls(seen).expect("one call");
        assert_eq!(before, "Let's check `usePromptPicker.ts`.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Read");
        let args: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({ "file_path": "CodeNomad/packages/ui/src/components/prompt-input/usePromptPicker.ts" }));

        // Several calls, nested values, numbers and flags, text with commas and braces in it.
        let two = "<|tool_call>call:Task{subagent_type:<|\"|>developer<|\"|>,description:<|\"|>find dialog<|\"|>,prompt:<|\"|>Look at {a, b}: then report.<|\"|>}<tool_call|>\
<|tool_call>call:TodoWrite{todos:[{content:<|\"|>Search<|\"|>,status:<|\"|>pending<|\"|>,n:2,done:false}]}<tool_call|>";
        let (before, calls) = extract_text_tool_calls(two).expect("two calls");
        assert!(before.is_empty());
        assert_eq!(calls.len(), 2);
        let task: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(task["prompt"], "Look at {a, b}: then report.");
        let todo: Value = serde_json::from_str(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(todo, json!({ "todos": [{ "content": "Search", "status": "pending", "n": 2, "done": false }] }));

        // Cut off by the reply limit: what arrived is used. Tokens with no call in them are left alone.
        let cut = "<|tool_call>call:Bash{command:<|\"|>ls docs";
        let (_, calls) = extract_text_tool_calls(cut).expect("cut-off call");
        assert_eq!(serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap()["command"], "ls docs");
        assert!(extract_text_tool_calls("see <|tool_call> in the docs").is_none());

        // A garbled list (a stray `}` where an item should be) still ends: the brace is stepped over.
        let (_, calls) = extract_text_tool_calls("<|tool_call>call:TodoWrite{todos:[}<|\"|>a<|\"|>]}<tool_call|>").expect("garbled call");
        assert_eq!(serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap(), json!({ "todos": ["a"] }));
    }

    #[test]
    fn text_tool_calls_xml_hermes_and_truncated() {
        // Qwen3-Coder / Nemotron XML, complete.
        let xml = "Delegating now.\n<tool_call>\n<function=Task>\n<parameter=subagent_type>\nba\n</parameter>\n<parameter=prompt>\nWrite the spec\n</parameter>\n</function>\n</tool_call>";
        let (before, calls) = extract_text_tool_calls(xml).unwrap();
        assert_eq!(before, "Delegating now.");
        assert_eq!(calls[0]["function"]["name"], "Task");
        let args = parse_arguments(&calls[0]["function"]["arguments"]);
        assert_eq!(args["subagent_type"], "ba");
        assert_eq!(args["prompt"], "Write the spec");

        // What the PMA produced: bare parameters, invented keys, cut off mid-prompt.
        let cut = "Audit module\n<parameter=subagent_description>\nAnalyze the module\n<parameter=subagent_prompt>\nCreate a 5-page report covering:\n\nModule";
        let (_, calls) = extract_text_tool_calls(cut).unwrap();
        assert_eq!(calls[0]["function"]["name"], "Task");
        let args = apply_aliases("Task", parse_arguments(&calls[0]["function"]["arguments"]));
        assert_eq!(args["description"], "Analyze the module");
        assert!(args["prompt"].as_str().unwrap().starts_with("Create a 5-page report"));

        // Hermes JSON.
        let hermes = r#"<tool_call>{"name": "bash", "arguments": {"command": "ls"}}</tool_call>"#;
        let (_, calls) = extract_text_tool_calls(hermes).unwrap();
        assert_eq!(calls[0]["function"]["name"], "bash");

        // Plain text is left alone.
        assert!(extract_text_tool_calls("Here is the summary. No tools needed.").is_none());
        assert!(extract_text_tool_calls("<parameter=x>y").is_none());
    }

    #[test]
    fn unknown_tool_lists_real_tools() {
        let tools = json!([{ "function": { "name": "Read" } }, { "function": { "name": "Task" } }]);
        assert_eq!(unknown_tool_message("nope", Some(&tools)), "Unknown tool: nope. Use one of: Read, Task.");
    }

    #[test]
    fn tasks_resume_from_memory_and_disk() {
        let dir = std::env::temp_dir().join(format!("task-store-{}", new_task_id()));
        // SAFETY: tests in this module don't run concurrently with other env readers of this key.
        unsafe { std::env::set_var("CODENOMAD_TASK_STORE", &dir) };
        let id = new_task_id();
        save_task(&id, TaskRecord { agent: "ba".into(), messages: vec![json!({"role": "user", "content": "hi"})], meta: Some(json!({"task_path": "t.md"})) });
        memory().lock().unwrap().remove(&id);
        let loaded = load_task(&id).expect("loaded from disk");
        assert_eq!(loaded.agent, "ba");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.meta, Some(json!({"task_path": "t.md"})), "meta survives the disk round trip");
        assert!(load_task("../etc/passwd").is_none());
        unsafe { std::env::remove_var("CODENOMAD_TASK_STORE") };
        let _ = fs::remove_dir_all(dir);
        assert!(task_result("task_1", "done").starts_with("task_id: task_1"));
    }
}
