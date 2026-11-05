mod agents;
mod catalog;
mod config;
mod delegation;
mod file_lease;
mod inputs;
mod nomad_tools;
mod opencode_config;
mod skills;
mod stream;

use async_openai::{Client, config::OpenAIConfig};
use futures_util::StreamExt;
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
    /// Protocol 2 (`NOMAD_PROTOCOL=2`): delegated tasks emit their own events,
    /// stamped with `task_id`, plus `task.start` / `task.end`. Off by default
    /// because a protocol-1 reader would fold a child's text and tool calls into
    /// the parent's reply.
    protocol2: bool,
}

/// `NOMAD_PROTOCOL=2` opts in to task-scoped events.
fn protocol2_enabled() -> bool {
    env::var("NOMAD_PROTOCOL").map(|v| v.trim() == "2").unwrap_or(false)
}

/// How many Task calls of one assistant turn run at once (`NOMAD_MAX_PARALLEL`, default 3).
/// One local model endpoint is the real bottleneck, so the default stays small.
fn max_parallel() -> usize {
    env::var("NOMAD_MAX_PARALLEL")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(3)
        .clamp(1, 16)
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

/// Saves a task's conversation after each completed step.
type Checkpoint<'a> = &'a dyn Fn(&[Value]);

// One more argument than clippy likes; bundling them is a refactor of every call site.
#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    messages: &mut Vec<Value>,
    skills: &[Skill],
    tool_mode: ToolMode,
    top: Option<&TopLevel<'_>>,
    depth: u8,
    // Called with the conversation after every completed tool step, so a task
    // that is stopped or fails can be resumed from where it got to.
    checkpoint: Option<Checkpoint<'_>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let tools = tool_specs(tool_mode, skills, depth);

    for _ in 0..40 {
        let mut request = json!({
            "model": cfg.model,
            // One system message, first: strict chat templates reject any other layout.
            "messages": inputs::merge_system_messages(messages),
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
                let mut message = response["choices"]
                    .as_array()
                    .and_then(|a| a.first())
                    .ok_or("no choices in response")?["message"]
                    .clone();
                if response["usage"].is_object() {
                    message["usage"] = response["usage"].clone();
                }
                message
            }
        };
        // Token usage for this step, for CodeNomad's context meter.
        if let Some(top) = top {
            if let Some(event) = stream::usage_event(&message["usage"], &cfg.model) {
                top.emitter.emit(event);
            }
        }
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

        // Normalize every call first: models name tools OpenCode-style and send
        // arguments as a string or an object, under varying keys.
        let calls: Vec<PreparedCall> = tool_calls.iter().map(PreparedCall::from_value).collect();
        let mut outputs: Vec<Option<String>> = vec![None; calls.len()];
        let batchable = |c: &PreparedCall| c.name == "Task" && depth < delegation::MAX_TASK_DEPTH;

        // Calls run in the order the model wrote them, except that a run of
        // consecutive Task calls is one concurrent batch. Any other tool is a
        // barrier, so "Write the spec, then Task a reviewer" keeps its order.
        let mut idx = 0;
        while idx < calls.len() {
            if batchable(&calls[idx]) {
                let mut end = idx;
                while end < calls.len() && batchable(&calls[end]) {
                    end += 1;
                }
                run_task_batch(client, cfg, skills, top, &calls[idx..end], &mut outputs[idx..end], depth).await;
                idx = end;
                continue;
            }
            let call = &calls[idx];
            emit_tool(top, call);
            let result =
                Box::pin(execute_call(client, cfg, skills, tools.as_ref(), call, depth)).await?;
            emit_tool_result(top, call, &result);
            outputs[idx] = Some(result);
            idx += 1;
        }

        // Results go into the history in call order, whatever order they finished in.
        for (call, output) in calls.iter().zip(outputs) {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": output.unwrap_or_default(),
            }));
        }
        if let Some(save) = checkpoint {
            save(messages);
        }
    }

    Err("agent loop exceeded maximum iterations".into())
}

/// A tool call with its name and arguments normalized.
struct PreparedCall {
    id: String,
    name: String,
    args: Value,
}

impl PreparedCall {
    fn from_value(call: &Value) -> Self {
        let name = delegation::canonical_tool_name(call["function"]["name"].as_str().unwrap_or("")).to_string();
        let args = delegation::apply_aliases(&name, delegation::parse_arguments(&call["function"]["arguments"]));
        PreparedCall { id: call["id"].as_str().unwrap_or("").to_string(), name, args }
    }
}

fn emit_tool(top: Option<&TopLevel<'_>>, call: &PreparedCall) {
    if let Some(top) = top {
        top.emitter.emit(json!({
            "type": "tool",
            "id": call.id,
            "name": call.name,
            "input": call.args,
        }));
    }
}

fn emit_tool_result(top: Option<&TopLevel<'_>>, call: &PreparedCall, result: &str) {
    if let Some(top) = top {
        top.emitter.emit(json!({
            "type": "tool_result",
            "id": call.id,
            "name": call.name,
            "output": stream::truncate_chars(result, 2000),
            "isError": stream::is_error_output(result),
        }));
    }
}

/// Runs one non-Task tool call. (Task calls go through `run_task_batch`.)
async fn execute_call(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    tools: Option<&Value>,
    call: &PreparedCall,
    depth: u8,
) -> Result<String, Box<dyn std::error::Error>> {
    let fn_args = &call.args;
    let file_path = fn_args["file_path"].as_str().unwrap_or("");
    Ok(match call.name.as_str() {
        "Read" => file_lease::with_lease(file_path, &call.id, || execute_read(fn_args))
            .unwrap_or_else(|err| format!("Error: {err}")),
        "Write" => file_lease::with_lease(file_path, &call.id, || execute_write(fn_args))
            .unwrap_or_else(|err| format!("Error: {err}")),
        // Off the async thread: a long command must not stall the other tasks of this turn.
        "Bash" => {
            let args = fn_args.clone();
            tokio::task::spawn_blocking(move || execute_bash(&args))
                .await
                .unwrap_or_else(|err| format!("Error: bash did not complete: {err}"))
        }
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
        "nomadflow_run_workflow" => Box::pin(handle_nomadflow_run(client, cfg, skills, fn_args, depth)).await?,
        other => {
            if let Some(out) = nomad_tools::dispatch(other, fn_args) {
                out
            } else {
                delegation::unknown_tool_message(other, tools)
            }
        }
    })
}

/// Which calls of a batch try to resume a `task_id` an earlier call of the same
/// batch already uses. Two concurrent resumes of one transcript would overwrite
/// each other's progress, so the later ones are refused.
fn duplicate_resumes(calls: &[PreparedCall]) -> Vec<bool> {
    let mut seen = std::collections::HashSet::new();
    calls
        .iter()
        .map(|call| match call.args["task_id"].as_str().map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => !seen.insert(id.to_string()),
            None => false,
        })
        .collect()
}

/// Runs a run of consecutive Task calls concurrently (at most `max_parallel()`
/// at a time). Every card is announced up front, so the UI shows the whole
/// dispatch at once; each result is reported the moment its task finishes.
async fn run_task_batch(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    calls: &[PreparedCall],
    outputs: &mut [Option<String>],
    depth: u8,
) {
    for call in calls {
        emit_tool(top, call);
    }
    let duplicate = duplicate_resumes(calls);
    let jobs = calls.iter().enumerate().map(move |(i, call)| {
        let duplicate = duplicate[i];
        async move {
            let output = if duplicate {
                format!(
                    "Error: task_id `{}` is already being continued by another Task call in this \
turn. Resume it in a later turn, after that call has returned.",
                    call.args["task_id"].as_str().unwrap_or("")
                )
            } else {
                handle_task(client, cfg, skills, top, &call.id, &call.args, depth).await
            };
            (i, output)
        }
    });
    let mut running = futures_util::stream::iter(jobs).buffer_unordered(max_parallel());
    while let Some((i, output)) = running.next().await {
        emit_tool_result(top, &calls[i], &output);
        outputs[i] = Some(output);
    }
}

/// The longest prefix of `messages` that is a valid conversation: if the last
/// assistant tool call was not fully answered (the run stopped mid-step), drop it.
fn consistent_prefix(messages: &[Value]) -> Vec<Value> {
    let last_calls = messages
        .iter()
        .rposition(|m| m["role"] == "assistant" && m["tool_calls"].as_array().is_some_and(|c| !c.is_empty()));
    let Some(pos) = last_calls else { return messages.to_vec() };
    let answered: std::collections::HashSet<&str> = messages[pos + 1..]
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["tool_call_id"].as_str())
        .collect();
    let complete = messages[pos]["tool_calls"]
        .as_array()
        .is_some_and(|calls| calls.iter().all(|c| answered.contains(c["id"].as_str().unwrap_or(""))));
    if complete { messages.to_vec() } else { messages[..pos].to_vec() }
}

/// The Task tool: run a NomadWorks subagent in its own context and return its
/// answer with a task_id (OpenCode's format). A known task_id resumes that
/// subagent's conversation. Failures become tool output, so the caller can react.
///
/// Progress is checkpointed after every step, and also when the task fails, so
/// the same `task_id` can resume it. With protocol 2 the task also reports its
/// own live events (`task.start`, text, tool calls, `task.end`).
async fn handle_task(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    call_id: &str,
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
    let was_resumed = resumed.is_some();
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

    // Protocol 2: this task's events are stamped with its id and go to the UI live.
    let scoped = top.filter(|t| t.protocol2).map(|t| t.emitter.scoped(&task_id, call_id));
    let child_top = top.zip(scoped.as_ref()).map(|(t, emitter)| TopLevel {
        emitter,
        base_url: t.base_url.clone(),
        stream: t.stream,
        protocol2: true,
    });
    if let Some(emitter) = &scoped {
        emitter.emit(json!({
            "type": "task.start",
            "agent": skill.name,
            "description": args["description"].as_str().unwrap_or(""),
            "model": task_cfg.model,
            "mode": "subagent",
            "resumed": was_resumed,
        }));
    }

    let agent_name = skill.name.clone();
    let save_as = task_id.clone();
    let checkpoint = move |msgs: &[Value]| {
        delegation::save_task(&save_as, delegation::TaskRecord { agent: agent_name.clone(), messages: msgs.to_vec() })
    };
    let outcome = Box::pin(run_agent_loop(
        client,
        &task_cfg,
        &mut messages,
        skills,
        ToolMode::All,
        child_top.as_ref(),
        depth + 1,
        Some(&checkpoint),
    ))
    .await;
    match outcome {
        Ok(answer) => {
            delegation::save_task(&task_id, delegation::TaskRecord { agent: skill.name.clone(), messages });
            if let Some(emitter) = &scoped {
                emitter.emit(json!({
                    "type": "task.end",
                    "status": "completed",
                    "summary": stream::truncate_chars(&answer, 500),
                }));
            }
            delegation::task_result(&task_id, &answer)
        }
        Err(err) => {
            // Keep what the task got to, so the same task_id can pick it up again.
            delegation::save_task(
                &task_id,
                delegation::TaskRecord { agent: skill.name.clone(), messages: consistent_prefix(&messages) },
            );
            if let Some(emitter) = &scoped {
                emitter.emit(json!({ "type": "task.end", "status": "failed", "error": err.to_string() }));
            }
            format!(
                "Error: subagent {} failed: {err}\n\ntask_id: {task_id} (progress was saved; pass this task_id to continue)",
                skill.name
            )
        }
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
        None,
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
            None,
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
    let emitter = Emitter::default();
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
        protocol2: protocol2_enabled(),
    });
    if stream_mode {
        emitter.emit(json!({
            "type": "meta",
            "model": run_cfg.model,
            "agent": initial.agent,
            "baseUrl": active_base,
            "protocol": if protocol2_enabled() { 2 } else { 1 },
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
        None,
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

    // ------------------------------------------------------------ parallel Task execution

    fn call(id: &str, name: &str, args: Value) -> PreparedCall {
        PreparedCall { id: id.into(), name: name.into(), args }
    }

    #[test]
    fn a_task_id_is_resumed_once_per_batch() {
        let calls = vec![
            call("a", "Task", json!({ "task_id": "task_1" })),
            call("b", "Task", json!({ "task_id": "task_2" })),
            call("c", "Task", json!({ "task_id": "task_1" })),
            call("d", "Task", json!({})),
            call("e", "Task", json!({ "task_id": "  " })),
        ];
        assert_eq!(duplicate_resumes(&calls), vec![false, false, true, false, false]);
    }

    #[test]
    fn unanswered_tool_calls_are_dropped_from_a_saved_conversation() {
        let user = json!({ "role": "user", "content": "go" });
        let asked = json!({ "role": "assistant", "content": null,
            "tool_calls": [{ "id": "1", "function": {} }, { "id": "2", "function": {} }] });
        let one = json!({ "role": "tool", "tool_call_id": "1", "content": "ok" });
        let two = json!({ "role": "tool", "tool_call_id": "2", "content": "ok" });

        // Both answered: kept whole.
        let all = vec![user.clone(), asked.clone(), one.clone(), two.clone()];
        assert_eq!(consistent_prefix(&all).len(), 4);
        // One missing: the dangling assistant turn is cut, nothing before it is lost.
        let partial = vec![user.clone(), asked.clone(), one.clone()];
        assert_eq!(consistent_prefix(&partial), vec![user.clone()]);
        // No tool calls at all: untouched.
        let plain = vec![user.clone(), json!({ "role": "assistant", "content": "hi" })];
        assert_eq!(consistent_prefix(&plain).len(), 2);
    }

    /// A scripted OpenAI-compatible endpoint. The parent (a request with no
    /// "do-sub-" text) first asks for three Task calls, then — once tool results
    /// are in — answers. Each subagent waits `delay_ms` before answering, so
    /// overlap shows up in the wall-clock time. `fail` makes subagent 2 return 400.
    async fn mock_model(delay_ms: u64, fail: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let body_start;
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 { return; }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            body_start = pos + 4;
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < body_start + want {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 { break; }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let req: Value = serde_json::from_slice(&buf[body_start..]).unwrap_or(json!({}));
                    let text = req["messages"].to_string();
                    let has_tool_results = req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool"));
                    let (status, message) = if text.contains("do-sub-") && !has_tool_results {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        let n = text.split("do-sub-").nth(1).and_then(|r| r.chars().next()).unwrap_or('?');
                        if fail && n == '2' {
                            ("400 Bad Request", json!({ "error": "boom" }))
                        } else {
                            ("200 OK", json!({ "role": "assistant", "content": format!("answer-{n}") }))
                        }
                    } else if has_tool_results {
                        ("200 OK", json!({ "role": "assistant", "content": "ALL DONE" }))
                    } else {
                        let tasks: Vec<Value> = (1..=3).map(|n| json!({
                            "id": format!("call_{n}"), "type": "function",
                            "function": { "name": "Task", "arguments": json!({
                                "subagent_type": "worker", "description": format!("job {n}"),
                                "prompt": format!("do-sub-{n}") }).to_string() }
                        })).collect();
                        ("200 OK", json!({ "role": "assistant", "content": null, "tool_calls": tasks }))
                    };
                    let payload = if status.starts_with("200") {
                        json!({ "id": "x", "object": "chat.completion", "created": 0, "model": "m",
                            "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }] })
                    } else { message };
                    let body = payload.to_string();
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        base
    }

    struct Run {
        answer: Result<String, String>,
        messages: Vec<Value>,
        events: Vec<Value>,
        elapsed: std::time::Duration,
    }

    async fn run_parent(base: &str, protocol2: bool) -> Run {
        let cfg = LlmConfig {
            base_url: base.into(),
            fallback_base_url: base.into(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: 256,
        };
        let client = make_client(base, "k");
        let skills = vec![skill("worker", true)];
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.into(), stream: false, protocol2 };
        let mut messages = vec![
            json!({ "role": "system", "content": "you orchestrate" }),
            json!({ "role": "user", "content": "plan the work" }),
        ];
        let started = std::time::Instant::now();
        let answer = run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, Some(&top), 0, None)
            .await
            .map_err(|e| e.to_string());
        let elapsed = started.elapsed();
        let events = sink.lock().unwrap().clone();
        Run { answer, messages, events, elapsed }
    }

    fn position(events: &[Value], pred: impl Fn(&Value) -> bool) -> Option<usize> {
        events.iter().position(pred)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn three_task_calls_run_at_the_same_time() {
        let base = mock_model(400, false).await;
        let run = run_parent(&base, false).await;
        assert_eq!(run.answer.as_deref(), Ok("ALL DONE"));
        // Three 400ms subagents: ~1.2s if serial, ~0.4s if overlapped.
        assert!(run.elapsed.as_millis() < 900, "tasks ran serially: {:?}", run.elapsed);

        // All three cards are announced before any result comes back.
        let first_result = position(&run.events, |e| e["type"] == "tool_result").unwrap();
        let tools_before = run.events[..first_result].iter().filter(|e| e["type"] == "tool").count();
        assert_eq!(tools_before, 3);

        // History keeps call order, with each answer under its own call id.
        let tool_msgs: Vec<&Value> = run.messages.iter().filter(|m| m["role"] == "tool").collect();
        let ids: Vec<&str> = tool_msgs.iter().map(|m| m["tool_call_id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["call_1", "call_2", "call_3"]);
        for (n, m) in tool_msgs.iter().enumerate() {
            assert!(m["content"].as_str().unwrap().contains(&format!("answer-{}", n + 1)), "{m}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protocol_one_adds_no_child_events() {
        let base = mock_model(10, false).await;
        let run = run_parent(&base, false).await;
        assert!(run.events.iter().all(|e| e.get("task_id").is_none()), "{:?}", run.events);
        assert!(run.events.iter().all(|e| e["type"] != "task.start" && e["type"] != "task.end"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protocol_two_stamps_each_task() {
        let base = mock_model(10, false).await;
        let run = run_parent(&base, true).await;
        let starts: Vec<&Value> = run.events.iter().filter(|e| e["type"] == "task.start").collect();
        let ends: Vec<&Value> = run.events.iter().filter(|e| e["type"] == "task.end").collect();
        assert_eq!(starts.len(), 3);
        assert_eq!(ends.len(), 3);
        let mut parents: Vec<&str> = starts.iter().map(|e| e["parent_call_id"].as_str().unwrap()).collect();
        parents.sort();
        assert_eq!(parents, ["call_1", "call_2", "call_3"]);
        assert!(ends.iter().all(|e| e["status"] == "completed"));
        // Every child event carries its task id; the parent's own events do not.
        for e in &run.events {
            if e["type"] == "task.start" || e["type"] == "task.end" {
                assert!(e["task_id"].is_string());
            }
        }
        let child_text: Vec<&Value> = run.events.iter().filter(|e| e["type"] == "delta" && e.get("task_id").is_some()).collect();
        assert_eq!(child_text.len(), 3);
        assert!(run.events.iter().any(|e| e["type"] == "delta" && e.get("task_id").is_none()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_failing_task_does_not_sink_the_others_and_can_be_resumed() {
        let base = mock_model(10, true).await;
        let run = run_parent(&base, true).await;
        assert_eq!(run.answer.as_deref(), Ok("ALL DONE"), "the parent carries on");

        let results: Vec<String> = run.messages.iter().filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap().to_string()).collect();
        assert!(results[0].contains("answer-1"));
        assert!(results[1].starts_with("Error: subagent worker failed"), "{}", results[1]);
        assert!(results[2].contains("answer-3"));

        // The failure names a task_id, and that task's conversation was saved.
        let id = results[1].split("task_id: ").nth(1).unwrap().split_whitespace().next().unwrap();
        let saved = delegation::load_task(id).expect("failed task was checkpointed");
        assert_eq!(saved.agent, "worker");
        assert!(saved.messages.iter().any(|m| m["content"].as_str().is_some_and(|c| c.contains("do-sub-2"))));

        let failed = run.events.iter().find(|e| e["type"] == "task.end" && e["status"] == "failed").expect("failed end event");
        assert_eq!(failed["task_id"], id);
    }
}
