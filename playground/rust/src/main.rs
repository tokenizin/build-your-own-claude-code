mod agents;
mod catalog;
mod config;
mod delegation;
mod inputs;
mod nomad_tools;
mod opencode_config;
mod skills;
mod stream;

use async_openai::{Client, config::OpenAIConfig};
use clap::{Parser, ValueEnum};
use config::LlmConfig;
use serde_json::{Value, json};
use skills::{
    Skill, collect_skill_roots, discover_skills, expand_slash_invocation, fork_user_body,
    format_skill_catalog, skill_by_name, substitute_arguments, wrap_skill_body_for_location,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use stream::{Emitter, StreamError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Print only the final answer (default).
    Text,
    /// NDJSON events on stdout (meta, delta, tool, tool_result, final, error).
    StreamJson,
}

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Prompt text (required unless --catalog).
    #[arg(short = 'p', long, required_unless_present = "catalog")]
    prompt: Option<String>,

    /// Print a JSON catalog of models, agents and commands, then exit.
    #[arg(long)]
    catalog: bool,

    /// Model for the main run (wins over a fork agent's own model).
    #[arg(long)]
    model: Option<String>,

    /// Run as this agent (same as prefixing the prompt with `/NAME`).
    #[arg(long)]
    agent: Option<String>,

    /// Attach an image to the user message (repeatable).
    #[arg(long = "image", value_name = "PATH")]
    images: Vec<PathBuf>,

    /// JSON file with prior turns: [{"role":"user"|"assistant","content":"..."}].
    #[arg(long, value_name = "PATH")]
    history: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,
}

/// Top-level stream-json output context; nested runs get `None`.
struct TopLevel<'a> {
    emitter: &'a Emitter,
    base_url: String,
    stream: bool,
}

#[derive(Clone, Copy)]
enum ToolMode {
    /// No tools — smallest prompt (good for slash smoke tests).
    None,
    /// Read, Write, Bash + NomadWorks tools
    Base,
    /// Read, Write, Bash, Skill + NomadWorks tools
    All,
}

struct InitialRun {
    messages: Vec<Value>,
    tool_mode: ToolMode,
    /// Per-fork model override (NomadWorks agent roles).
    model_override: Option<String>,
    /// Fork agent name when the run starts as a single forked agent.
    agent: Option<String>,
}

/// Apply `--agent`: `/NAME <prompt>` unless the prompt already starts with `/`.
/// Returns the effective prompt and an optional stderr warning.
fn apply_agent_flag(prompt: &str, agent: Option<&str>, skills: &[Skill]) -> (String, Option<String>) {
    let Some(agent) = agent.map(str::trim).filter(|a| !a.is_empty()) else {
        return (prompt.to_string(), None);
    };
    if prompt.trim_start().starts_with('/') {
        return (
            prompt.to_string(),
            Some(format!("--agent {agent} ignored: prompt already starts with '/'")),
        );
    }
    match skill_by_name(skills, agent) {
        Some(skill) if catalog::is_agent(skill) => (format!("/{agent} {prompt}"), None),
        _ => (
            prompt.to_string(),
            Some(format!("unknown agent `{agent}` — running without an agent")),
        ),
    }
}

fn tool_specs(mode: ToolMode, skills: &[Skill], depth: u8) -> Option<Value> {
    match mode {
        ToolMode::None => None,
        ToolMode::Base | ToolMode::All => {
            let mut tools = vec![
                json!({
                    "type": "function",
                    "function": {
                        "name": "Read",
                        "description": "Read and return the contents of a file",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "file_path": {
                                    "type": "string",
                                    "description": "The path to the file to read"
                                }
                            },
                            "required": ["file_path"]
                        }
                    }
                }),
                json!({
                    "type": "function",
                    "function": {
                        "name": "Write",
                        "description": "Write content to a file",
                        "parameters": {
                            "type": "object",
                            "required": ["file_path", "content"],
                            "properties": {
                                "file_path": {
                                    "type": "string",
                                    "description": "The path of the file to write to"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "The content to write to the file"
                                }
                            }
                        }
                    }
                }),
                json!({
                    "type": "function",
                    "function": {
                        "name": "Bash",
                        "description": "Execute a shell command",
                        "parameters": {
                            "type": "object",
                            "required": ["command"],
                            "properties": {
                                "command": {
                                    "type": "string",
                                    "description": "The command to execute"
                                }
                            }
                        }
                    }
                }),
            ];

            if matches!(mode, ToolMode::All) {
                tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": "Skill",
                        "description": "Load a skill's instructions into the conversation",
                        "parameters": {
                            "type": "object",
                            "required": ["name"],
                            "properties": {
                                "name": {
                                    "type": "string",
                                    "description": "The name of the skill to use"
                                },
                                "args": {
                                    "type": "string",
                                    "description": "Optional arguments for the skill"
                                }
                            }
                        }
                    }
                }));
            }

            // P3: NomadWorks tools available to base + all (including forked agents).
            tools.extend(nomad_tools::tool_specs());

            // Delegation to NomadWorks subagents (OpenCode's `task` tool).
            let agents = delegation::task_agents(skills);
            if depth < delegation::MAX_TASK_DEPTH && !agents.is_empty() {
                tools.push(delegation::task_tool_spec(&agents));
            }

            Some(Value::Array(tools))
        }
    }
}

fn execute_read(args: &Value) -> String {
    let path = args["file_path"].as_str().unwrap_or("");
    match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => format!("Error reading {path}: {err}"),
    }
}

fn execute_write(args: &Value) -> String {
    let path = args["file_path"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, content) {
        Ok(()) => format!("Successfully wrote to {path}"),
        Err(err) => format!("Error writing {path}: {err}"),
    }
}

fn execute_bash(args: &Value) -> String {
    let command = args["command"].as_str().unwrap_or("");
    let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&cwd)
        .output()
    {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(stderr.trim_end());
            }
            if !output.status.success() && text.is_empty() {
                text = format!(
                    "Command failed with exit code {}",
                    output.status.code().unwrap_or(-1)
                );
            }
            text
        }
        Err(err) => format!("Failed to run command: {err}"),
    }
}

fn assistant_text(message: &Value) -> String {
    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // Some Ollama cloud / thinking models put the answer in `reasoning` with empty content.
    if let Some(reasoning) = message.get("reasoning").and_then(|v| v.as_str()) {
        let last = reasoning
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .last()
            .unwrap_or("")
            .trim_matches('`')
            .trim_matches('"')
            .to_string();
        if !last.is_empty() {
            return last;
        }
    }
    String::new()
}

/// Returns initial messages, tool mode, and optional per-run model override.
/// Slash invokes skip the catalog; tools are omitted unless a skill body references
/// `scripts/` (keeps `/smoke-echo` lean). Forked agents get policies + all tools.
fn build_initial_run(prompt: &str, skills: &[Skill]) -> InitialRun {
    if let Some(invocations) = expand_slash_invocation(prompt, skills) {
        if invocations.len() == 1 && invocations[0].fork {
            let inv = &invocations[0];
            if let Some(skill) = skill_by_name(skills, &inv.skill_name) {
                let messages = agents::build_fork_messages(skill, &inv.body);
                return InitialRun {
                    messages,
                    // Agents (pma, product_manager, …) delegate, so they need Skill + Task.
                    tool_mode: ToolMode::All,
                    model_override: skill.model.clone(),
                    agent: Some(skill.name.clone()),
                };
            }
        }

        let needs_tools = invocations.iter().any(|inv| inv.body.contains("scripts/"));
        let messages = invocations
            .into_iter()
            .map(|inv| json!({ "role": "user", "content": inv.body }))
            .collect();
        let tool_mode = if needs_tools {
            ToolMode::Base
        } else {
            ToolMode::None
        };
        return InitialRun {
            messages,
            tool_mode,
            model_override: None,
            agent: None,
        };
    }

    let mut messages = Vec::new();
    let catalog = format_skill_catalog(skills);
    if !catalog.is_empty() {
        messages.push(json!({ "role": "system", "content": catalog }));
    }
    messages.push(json!({ "role": "user", "content": prompt }));
    InitialRun {
        messages,
        tool_mode: ToolMode::All,
        model_override: None,
        agent: None,
    }
}

/// How long async-openai may keep retrying 429/5xx responses. Its default is
/// 15 minutes, and Ollama's "credits used up" 429 looks like a rate limit to
/// it, so a run sat silent for 15 minutes before failing.
fn retry_window() -> std::time::Duration {
    let secs = env::var("LLM_RETRY_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(20);
    std::time::Duration::from_secs(secs)
}

fn make_client(base_url: &str, api_key: &str) -> Client<OpenAIConfig> {
    let config = OpenAIConfig::new()
        .with_api_base(base_url)
        .with_api_key(api_key);
    let backoff = backoff::ExponentialBackoffBuilder::new()
        .with_max_elapsed_time(Some(retry_window()))
        .build();
    Client::with_config(config).with_backoff(backoff)
}

async fn probe_base(base_url: &str, api_key: &str) -> bool {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    match client
        .get(&url)
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

async fn fetch_models(base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("GET {url}: {e}"))?;
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}"));
    }
    let body: Value =
        serde_json::from_str(&text).map_err(|e| format!("GET {url}: invalid JSON: {e}"))?;
    catalog::parse_model_list(&body).map_err(|e| format!("GET {url}: {e}"))
}

/// List models from the primary base, then the fallback. Returns (base used, result).
async fn list_models(cfg: &LlmConfig) -> (String, Result<Vec<String>, String>) {
    let primary = fetch_models(&cfg.base_url, &cfg.api_key).await;
    if primary.is_ok() || cfg.fallback_base_url == cfg.base_url {
        return (cfg.base_url.clone(), primary);
    }
    match fetch_models(&cfg.fallback_base_url, &cfg.api_key).await {
        Ok(list) => (cfg.fallback_base_url.clone(), Ok(list)),
        Err(fallback_err) => (
            cfg.base_url.clone(),
            Err(format!("{}; fallback: {fallback_err}", primary.unwrap_err())),
        ),
    }
}

async fn resolve_client(cfg: &LlmConfig) -> (Client<OpenAIConfig>, String) {
    if probe_base(&cfg.base_url, &cfg.api_key).await {
        eprintln!("LLM: {}", cfg.base_url);
        return (make_client(&cfg.base_url, &cfg.api_key), cfg.base_url.clone());
    }
    if cfg.fallback_base_url != cfg.base_url
        && probe_base(&cfg.fallback_base_url, &cfg.api_key).await
    {
        eprintln!(
            "LLM: {} unreachable — using fallback {}",
            cfg.base_url, cfg.fallback_base_url
        );
        return (
            make_client(&cfg.fallback_base_url, &cfg.api_key),
            cfg.fallback_base_url.clone(),
        );
    }
    eprintln!(
        "LLM: could not reach {} (or fallback {}); trying primary anyway",
        cfg.base_url, cfg.fallback_base_url
    );
    (make_client(&cfg.base_url, &cfg.api_key), cfg.base_url.clone())
}

async fn run_agent_loop(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    messages: &mut Vec<Value>,
    skills: &[Skill],
    tool_mode: ToolMode,
    top: Option<&TopLevel<'_>>,
    depth: u8,
) -> Result<String, Box<dyn std::error::Error>> {
    let tools = tool_specs(tool_mode, skills, depth);

    for _ in 0..40 {
        let mut request = json!({
            "model": cfg.model,
            "messages": messages,
            "max_tokens": cfg.max_tokens,
        });
        if let Some(tools) = &tools {
            request["tools"] = tools.clone();
        }

        // stream-json top level: try SSE streaming first (deltas emitted live).
        let mut streamed: Option<Value> = None;
        if let Some(top) = top.filter(|t| t.stream) {
            match stream::stream_chat(&top.base_url, &cfg.api_key, &request, top.emitter).await {
                Ok(message) => streamed = Some(message),
                Err(StreamError::BeforeData(err)) => {
                    eprintln!("stream: {err} — falling back to non-streaming request");
                }
                Err(StreamError::Rejected(err)) => {
                    return Err(format_api_error(err));
                }
                Err(StreamError::Mid(err)) => {
                    return Err(format!("stream interrupted: {err}").into());
                }
            }
        }

        let was_streamed = streamed.is_some();
        let message = match streamed {
            Some(message) => message,
            None => {
                let response: Value = client
                    .chat()
                    .create_byot(request)
                    .await
                    .map_err(|err| format_api_error(err))?;
                response["choices"]
                    .as_array()
                    .and_then(|a| a.first())
                    .ok_or("no choices in response")?["message"]
                    .clone()
            }
        };
        let message = &message;
        let text = assistant_text(message);

        if let Some(top) = top {
            // Non-streamed step: emit the whole step text. Streamed step: content deltas are
            // already out; emit the reasoning-fallback text only if no content streamed.
            let streamed_content = message["content"]
                .as_str()
                .is_some_and(|c| !c.trim().is_empty());
            if !was_streamed || !streamed_content {
                top.emitter.delta(&text);
            }
        }
        let mut assistant = json!({
            "role": "assistant",
            "content": if text.is_empty() {
                message.get("content").cloned().unwrap_or(Value::Null)
            } else {
                Value::String(text.clone())
            },
        });

        let mut tool_calls = message
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // The model wrote its tool call into the text (common for local models,
        // and always when the call was cut off): run it as a real call.
        if tool_calls.is_empty() && tools.is_some() {
            if let Some((before, calls)) = delegation::extract_text_tool_calls(&text) {
                eprintln!("tools: recovered {} tool call(s) written as text", calls.len());
                assistant["content"] = if before.is_empty() { Value::Null } else { Value::String(before) };
                tool_calls = calls;
            }
        }

        if !tool_calls.is_empty() {
            assistant["tool_calls"] = Value::Array(tool_calls.clone());
        }
        messages.push(assistant);

        if tool_calls.is_empty() {
            if text.is_empty() {
                return Err("assistant returned empty content".into());
            }
            return Ok(text);
        }

        for call in &tool_calls {
            // Models name tools OpenCode-style and send arguments as a string or an
            // object, under varying keys; normalize before running anything.
            let fn_name = delegation::canonical_tool_name(call["function"]["name"].as_str().unwrap_or(""));
            let fn_args = delegation::apply_aliases(fn_name, delegation::parse_arguments(&call["function"]["arguments"]));
            let call_id = call["id"].as_str().unwrap_or("");

            if let Some(top) = top {
                top.emitter.emit(json!({
                    "type": "tool",
                    "id": call_id,
                    "name": fn_name,
                    "input": fn_args,
                }));
            }

            let result = match fn_name {
                "Read" => execute_read(&fn_args),
                "Write" => execute_write(&fn_args),
                "Bash" => execute_bash(&fn_args),
                "Skill" => {
                    Box::pin(handle_skill(
                        client,
                        cfg,
                        skills,
                        fn_args["name"].as_str().unwrap_or(""),
                        fn_args["args"].as_str().unwrap_or(""),
                        depth,
                    ))
                    .await?
                }
                "nomadflow_run_workflow" => {
                    Box::pin(handle_nomadflow_run(client, cfg, skills, &fn_args, depth)).await?
                }
                "Task" if depth < delegation::MAX_TASK_DEPTH => {
                    Box::pin(handle_task(client, cfg, skills, &fn_args, depth)).await
                }
                other => {
                    if let Some(out) = nomad_tools::dispatch(other, &fn_args) {
                        out
                    } else {
                        delegation::unknown_tool_message(other, tools.as_ref())
                    }
                }
            };

            if let Some(top) = top {
                top.emitter.emit(json!({
                    "type": "tool_result",
                    "id": call_id,
                    "name": fn_name,
                    "output": stream::truncate_chars(&result, 2000),
                    "isError": stream::is_error_output(&result),
                }));
            }

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": result,
            }));
        }
    }

    Err("agent loop exceeded maximum iterations".into())
}

/// The Task tool: run a NomadWorks subagent in its own context and return its
/// answer with a task_id (OpenCode's format). A known task_id resumes that
/// subagent's conversation. Failures become tool output, so the caller can react.
async fn handle_task(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    args: &Value,
    depth: u8,
) -> String {
    let agents = delegation::task_agents(skills);
    let requested = args["subagent_type"].as_str().unwrap_or("").trim();
    let prompt = args["prompt"].as_str().unwrap_or("").trim();
    if requested.is_empty() || prompt.is_empty() {
        let names: Vec<&str> = agents.iter().map(|s| s.name.as_str()).collect();
        return format!(
            "Error: Task requires subagent_type (one of: {}) and prompt.",
            names.join(", ")
        );
    }
    let Some(skill) = delegation::find_agent(&agents, requested) else {
        let names: Vec<&str> = agents.iter().map(|s| s.name.as_str()).collect();
        return format!("Error: unknown subagent_type `{requested}`. Available: {}", names.join(", "));
    };

    let resumed = args["task_id"]
        .as_str()
        .and_then(delegation::load_task)
        .filter(|record| record.agent == skill.name);
    let (task_id, mut messages) = match resumed {
        Some(record) => {
            let mut messages = record.messages;
            messages.push(json!({ "role": "user", "content": prompt }));
            (args["task_id"].as_str().unwrap_or_default().to_string(), messages)
        }
        // The subagent's own role prompt (its agent .md) plus the delegated task.
        None => {
            let body = wrap_skill_body_for_location(skill, &fork_user_body(skill, prompt));
            (delegation::new_task_id(), agents::build_fork_messages(skill, &body))
        }
    };

    let task_cfg = match &skill.model {
        Some(model) => cfg.with_model(model),
        None => cfg.clone(),
    };
    eprintln!("task `{}` ({task_id}): model {}", skill.name, task_cfg.model);
    let outcome = Box::pin(run_agent_loop(
        client,
        &task_cfg,
        &mut messages,
        skills,
        ToolMode::All,
        None,
        depth + 1,
    ))
    .await;
    match outcome {
        Ok(answer) => {
            delegation::save_task(&task_id, delegation::TaskRecord { agent: skill.name.clone(), messages });
            delegation::task_result(&task_id, &answer)
        }
        Err(err) => format!("Error: subagent {} failed: {err}", skill.name),
    }
}

async fn handle_nomadflow_run(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    args: &Value,
    depth: u8,
) -> Result<String, Box<dyn std::error::Error>> {
    let task_path = args["task_path"].as_str().unwrap_or("").trim();
    let instructions = args["instructions"].as_str().unwrap_or("").trim();
    if task_path.is_empty() || instructions.is_empty() {
        return Ok(
            "Error: nomadflow_run_workflow requires task_path and instructions.".into(),
        );
    }

    let prompt = nomad_tools::workflow_fork_prompt(task_path, instructions);
    let runner = skill_by_name(skills, "workflow_runner")
        .or_else(|| skill_by_name(skills, "runner"));

    let mut fork_messages = if let Some(skill) = runner {
        agents::build_fork_messages(skill, &prompt)
    } else {
        vec![
            json!({
                "role": "system",
                "content": "You are a NomadWorks workflow_runner in a forked playground context."
            }),
            json!({ "role": "user", "content": prompt }),
        ]
    };

    let fork_cfg = runner
        .and_then(|s| s.model.as_ref())
        .map(|m| cfg.with_model(m))
        .unwrap_or_else(|| cfg.clone());
    if fork_cfg.model != cfg.model {
        eprintln!("nomadflow: model {}", fork_cfg.model);
    }

    let session = format!("sync-{}", uuidish());
    // The workflow runner delegates to the team, so it gets Skill + Task too.
    let answer = Box::pin(run_agent_loop(
        client,
        &fork_cfg,
        &mut fork_messages,
        skills,
        ToolMode::All,
        None,
        depth + 1,
    ))
    .await?;

    Ok(format!(
        "SUCCESS: Workflow completed synchronously (playground shim).\n\
Session: {session}\n\
Task: {task_path}\n\
---\n{answer}"
    ))
}

fn uuidish() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{}", nanos, std::process::id())
}

async fn handle_skill(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    name: &str,
    skill_args: &str,
    depth: u8,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some(skill) = skill_by_name(skills, name) else {
        return Ok(format!("Unknown skill: {name}"));
    };

    if skill.context.as_deref() == Some("fork") {
        let body = fork_user_body(skill, skill_args);
        let body = wrap_skill_body_for_location(skill, &body);
        let mut fork_messages = agents::build_fork_messages(skill, &body);
        let fork_cfg = match &skill.model {
            Some(model) => cfg.with_model(model),
            None => cfg.clone(),
        };
        if fork_cfg.model != cfg.model {
            eprintln!("fork `{}`: model {}", skill.name, fork_cfg.model);
        }
        let answer = Box::pin(run_agent_loop(
            client,
            &fork_cfg,
            &mut fork_messages,
            skills,
            ToolMode::Base,
            None,
            depth + 1,
        ))
        .await?;
        return Ok(format!(
            "Skill {} ran in a separate context and returned: {answer}",
            skill.name
        ));
    }

    let body = substitute_arguments(&skill.body, skill_args);
    let body = wrap_skill_body_for_location(skill, &body);
    Ok(body)
}

/// Credit/quota exhaustion (Ollama cloud: "usage credits … monthly max reached").
fn is_quota_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["usage credits", "monthly max", "insufficient_quota", "quota", "add usage credits", "billing"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn format_api_error(err: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    let text = err.to_string();
    if is_quota_error(&text) {
        return Box::from(format!(
            "The model endpoint has no credits left for this model (cloud models such as \
*-cloud use ollama.com credits). Pick a local model in the model picker, or raise the \
limit at https://ollama.com/settings.\n\n{text}"
        ));
    }
    if text.contains("429") {
        return Box::from(format!("The model endpoint is rate limiting requests; try again shortly.\n\n{text}"));
    }
    if text.contains("403") || text.contains("error sending request") {
        return Box::from(format!(
            "Ollama endpoint rejected the request. If using https://ollama.tokenizin.com, \
ensure the Cloudflare tunnel is up, or set OLLAMA_BASE_URL=http://127.0.0.1:11434/v1 \
for on-machine use.\n\n{text}"
        ));
    }
    Box::from(text)
}

fn stream_enabled() -> bool {
    env::var("LLM_STREAM").map(|v| v.trim() != "0").unwrap_or(true)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    let args = Args::parse();
    let cfg = LlmConfig::from_env();

    let skills = discover_skills();
    let fork_agents = skills.iter().filter(|s| catalog::is_agent(s)).count();
    eprintln!(
        "skills: {} loaded from {} root(s) ({} fork agents, {} NomadWorks root(s))",
        skills.len(),
        collect_skill_roots().len(),
        fork_agents,
        agents::collect_nomadworks_roots().len()
    );

    if args.catalog {
        let (base, listed) = list_models(&cfg).await;
        if let Err(err) = &listed {
            eprintln!("catalog: model listing failed: {err}");
        }
        let catalog = catalog::build_catalog(&cfg.model, &base, &skills, listed);
        println!("{catalog}");
        return ExitCode::SUCCESS;
    }

    let stream_mode = args.output_format == OutputFormat::StreamJson;
    let emitter = Emitter;
    let fail = |code: u8, message: String| {
        eprintln!("Error: {message}");
        if stream_mode {
            emitter.emit(json!({ "type": "error", "message": message }));
        }
        ExitCode::from(code)
    };

    let history = match &args.history {
        None => Vec::new(),
        Some(path) => match fs::read_to_string(path)
            .map_err(|e| format!("cannot read --history {}: {e}", path.display()))
            .and_then(|text| inputs::parse_history(&text))
        {
            Ok(history) => history,
            Err(err) => return fail(2, err),
        },
    };

    let prompt = args.prompt.clone().unwrap_or_default();
    let (prompt, agent_warning) = apply_agent_flag(&prompt, args.agent.as_deref(), &skills);
    if let Some(warning) = agent_warning {
        eprintln!("warning: {warning}");
    }

    let (client, active_base) = resolve_client(&cfg).await;

    let initial = build_initial_run(&prompt, &skills);
    let run_cfg = if let Some(model) = &args.model {
        eprintln!("model: {model} (--model)");
        cfg.with_model(model)
    } else if let Some(model) = &initial.model_override {
        eprintln!("model: {model} (fork override)");
        cfg.with_model(model)
    } else {
        eprintln!("model: {}", cfg.model);
        cfg
    };

    let mut messages = initial.messages;
    if !args.images.is_empty() {
        let loaded = inputs::load_images(&args.images);
        inputs::attach_images_to_messages(&mut messages, &loaded, &mut |w| {
            eprintln!("warning: {w}")
        });
    }
    inputs::insert_history(&mut messages, history);

    let top = stream_mode.then(|| TopLevel {
        emitter: &emitter,
        base_url: active_base.clone(),
        stream: stream_enabled(),
    });
    if stream_mode {
        emitter.emit(json!({
            "type": "meta",
            "model": run_cfg.model,
            "agent": initial.agent,
            "baseUrl": active_base,
        }));
    }

    match run_agent_loop(
        &client,
        &run_cfg,
        &mut messages,
        &skills,
        initial.tool_mode,
        top.as_ref(),
        0,
    )
    .await
    {
        Ok(final_text) => {
            if stream_mode {
                emitter.emit(json!({ "type": "final", "text": final_text, "model": run_cfg.model }));
            } else {
                println!("{final_text}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            if stream_mode {
                fail(1, err.to_string())
            } else {
                // Same stderr shape as the previous `main() -> Result` (Debug of the error).
                eprintln!("Error: {err:?}");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn quota_errors_get_a_plain_message() {
        let raw = r#"429 Too Many Requests {"error":{"message":"usage credits auto reload monthly max reached, increase your monthly max or add usage credits at https://ollama.com/settings","type":"api_error"}}"#;
        assert!(is_quota_error(raw));
        let msg = format_api_error(raw).to_string();
        assert!(msg.starts_with("The model endpoint has no credits left"));
        assert!(!is_quota_error("429 Too Many Requests slow down"));
        assert!(format_api_error("429 Too Many Requests slow down").to_string().contains("rate limiting"));
    }

    use super::*;

    fn skill(name: &str, fork: bool) -> Skill {
        Skill {
            name: name.into(),
            description: String::new(),
            body: "role body".into(),
            directory: PathBuf::from("/tmp"),
            context: fork.then(|| "fork".to_string()),
            policies: Vec::new(),
            model: None,
        }
    }

    #[test]
    fn agent_flag_prefixes_prompt() {
        let skills = vec![skill("pma", true), skill("plain", false)];
        assert_eq!(
            apply_agent_flag("do it", Some("pma"), &skills),
            ("/pma do it".to_string(), None)
        );
        let (p, w) = apply_agent_flag("/plain x", Some("pma"), &skills);
        assert_eq!(p, "/plain x");
        assert!(w.is_some());
        let (p, w) = apply_agent_flag("x", Some("nope"), &skills);
        assert_eq!(p, "x");
        assert!(w.unwrap().contains("unknown agent"));
        let (p, w) = apply_agent_flag("x", Some("plain"), &skills);
        assert_eq!(p, "x");
        assert!(w.is_some());
        assert_eq!(apply_agent_flag("x", None, &skills), ("x".to_string(), None));
    }

    #[test]
    fn args_require_prompt_unless_catalog() {
        assert!(Args::try_parse_from(["prog"]).is_err());
        assert!(Args::try_parse_from(["prog", "--catalog"]).is_ok());
        let a = Args::try_parse_from([
            "prog", "-p", "hi", "--image", "a.png", "--image", "b.jpg", "--output-format",
            "stream-json", "--model", "m", "--agent", "pma",
        ])
        .unwrap();
        assert_eq!(a.images.len(), 2);
        assert_eq!(a.output_format, OutputFormat::StreamJson);
        assert_eq!(a.prompt.as_deref(), Some("hi"));
    }
}
