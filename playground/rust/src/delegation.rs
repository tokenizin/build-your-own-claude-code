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
needs in the prompt: it does not see this conversation.\n\nAvailable subagents:\n{roster}"
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
                        "description": "Optional model for this task (e.g. ollama/qwen3.6:latest); the agent's own model by default"
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
    let first = ["<tool_call>", "<function=", "<parameter="]
        .iter()
        .filter_map(|tag| text.find(tag))
        .min()?;
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
