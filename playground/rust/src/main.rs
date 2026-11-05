mod agents;
mod catalog;
mod config;
mod delegation;
mod file_lease;
mod handover;
mod inputs;
mod lanes;
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
use std::io::Write as _;
use std::sync::Mutex;
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

#[cfg(test)]
thread_local! {
    static TEST_MAX_STEPS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// `raw` as a step count, or `default` when it is missing or not a positive number.
fn parse_steps(raw: Option<&str>, default: usize) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(default).min(2000)
}

/// Model round-trips one run may take (`NOMAD_MAX_STEPS`, default 40). A workflow session
/// orchestrates a whole lifecycle, so it gets more (`NOMAD_MAX_WORKFLOW_STEPS`, default 120).
fn max_steps(workflow: bool) -> usize {
    #[cfg(test)]
    if let Some(n) = TEST_MAX_STEPS.with(|t| t.get()) {
        return n;
    }
    if workflow {
        parse_steps(env::var("NOMAD_MAX_WORKFLOW_STEPS").ok().as_deref(), 120)
    } else {
        parse_steps(env::var("NOMAD_MAX_STEPS").ok().as_deref(), 40)
    }
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

fn tool_specs(mode: ToolMode, skills: &[Skill], depth: u8, workflow: bool) -> Option<Value> {
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

            // A workflow session reports back through its handover.
            if workflow {
                tools.push(handover::tool_spec());
            }

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

/// A workflow (orchestrator) session: where its handover goes, and the one it filed.
struct WorkflowScope {
    session_id: String,
    task_path: String,
    worktree: PathBuf,
    agent: String,
    filed: Mutex<Option<handover::Filed>>,
}

/// What a delegated run adds to the plain loop.
struct RunScope<'a> {
    /// Called with the conversation after every completed tool step, so a task
    /// that is stopped or fails can be resumed from where it got to.
    checkpoint: Option<Checkpoint<'a>>,
    /// Set for a workflow session: it gets the `handover` tool and must file one.
    workflow: Option<&'a WorkflowScope>,
    /// The lane this run is in when it is one of several Task calls running at once (see lanes.rs).
    lane: Option<&'a str>,
}

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
    scope: Option<&RunScope<'_>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let workflow = scope.and_then(|s| s.workflow);
    let checkpoint = scope.and_then(|s| s.checkpoint);
    let tools = tool_specs(tool_mode, skills, depth, workflow.is_some());

    let step_cap = max_steps(workflow.is_some());
    for _ in 0..step_cap {
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
                run_task_batch(client, cfg, skills, top, &calls[idx..end], &mut outputs[idx..end], depth, scope.and_then(|s| s.lane)).await;
                idx = end;
                continue;
            }
            let call = &calls[idx];
            emit_tool(top, call);
            let result = if matches!(call.name.as_str(), "nomadflow_run_workflow" | "nomadflow_prompt_workflow") {
                // A failing workflow is a result the caller can react to, not the end of this run.
                Box::pin(handle_nomadflow(client, cfg, skills, top, call, depth)).await
            } else {
                Box::pin(execute_call(client, cfg, skills, tools.as_ref(), call, depth, scope)).await?
            };
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

    Err(format!(
        "the agent used all {step_cap} steps without finishing (NOMAD_MAX_STEPS raises the limit); a delegated task keeps its progress, so its id can continue it"
    )
    .into())
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
    scope: Option<&RunScope<'_>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let fn_args = &call.args;
    let workflow = scope.and_then(|s| s.workflow);
    let file_path = fn_args["file_path"].as_str().unwrap_or("");
    Ok(match call.name.as_str() {
        "Read" => file_lease::with_lease(file_path, &call.id, || execute_read(fn_args))
            .unwrap_or_else(|err| format!("Error: {err}")),
        "Write" => {
            // Among Task calls running at the same time, a file belongs to the first lane that writes it.
            if let Some(lane) = scope.and_then(|s| s.lane) {
                if let Err(owner) = lanes::claim(file_path, lane) {
                    return Ok(format!(
                        "Error: {file_path} is being written by another task running in parallel ({owner}). Do not edit it from here: leave it to that task, \
or report back so the work can be done one after the other."
                    ));
                }
            }
            file_lease::with_lease(file_path, &call.id, || execute_write(fn_args)).unwrap_or_else(|err| format!("Error: {err}"))
        }
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
        "handover" => file_handover(workflow, fn_args),
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

// Same shape as run_agent_loop, which it serves; bundling the arguments is a refactor of both.
#[allow(clippy::too_many_arguments)]
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
    parent_lane: Option<&str>,
) {
    for call in calls {
        emit_tool(top, call);
    }
    let duplicate = duplicate_resumes(calls);
    // Several tasks at once each get a lane of their own; a lone one stays in its parent's.
    let parent_lane = parent_lane.map(str::to_string);
    let jobs = calls.iter().enumerate().map(move |(i, call)| {
        let duplicate = duplicate[i];
        let own_lane = (calls.len() > 1).then(|| lanes::child_lane(parent_lane.as_deref(), &call.id));
        let lane = own_lane.clone().or_else(|| parent_lane.clone());
        async move {
            let output = if duplicate {
                format!(
                    "Error: task_id `{}` is already being continued by another Task call in this \
turn. Resume it in a later turn, after that call has returned.",
                    call.args["task_id"].as_str().unwrap_or("")
                )
            } else {
                handle_task(client, cfg, skills, top, call, depth, lane.as_deref()).await
            };
            if let Some(own) = &own_lane {
                lanes::release(own);
            }
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

/// One delegated agent to run: a `Task` subagent, or a workflow's orchestrator.
struct Delegation<'a> {
    skill: &'a Skill,
    task_id: String,
    messages: Vec<Value>,
    description: String,
    /// "subagent" | "orchestrator" (reported to the UI).
    mode: &'static str,
    resumed: bool,
    call_id: &'a str,
    /// Saved with the conversation (a workflow remembers its task path and handover).
    meta: Option<Value>,
    workflow: Option<&'a WorkflowScope>,
    lane: Option<&'a str>,
    /// An explicit model for this run (the Task call's `model`); wins over the agent's own.
    model: Option<String>,
}

/// Runs a delegated agent to its answer, in its own context.
///
/// Progress is saved after every step, and also when the run fails, so the same
/// id can pick it up again. With protocol 2 it reports its own live events
/// (`task.start`, text, tool calls, `task.end`). Returns the answer or the error,
/// and the conversation as it ended.
async fn run_delegated(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    depth: u8,
    d: Delegation<'_>,
) -> (Result<String, String>, Vec<Value>) {
    let Delegation { skill, task_id, mut messages, description, mode, resumed, call_id, meta, workflow, lane, model } = d;
    let task_cfg = match model.as_ref().or(skill.model.as_ref()) {
        Some(model) => cfg.with_model(model),
        None => cfg.clone(),
    };
    eprintln!("{mode} `{}` ({task_id}): model {}", skill.name, task_cfg.model);

    // Protocol 2: this run's events are stamped with its id and go to the UI live.
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
            "description": description,
            "model": task_cfg.model,
            "mode": mode,
            "resumed": resumed,
        }));
    }

    let agent_name = skill.name.clone();
    let save_as = task_id.clone();
    let save_meta = meta.clone();
    let checkpoint = move |msgs: &[Value]| {
        delegation::save_task(
            &save_as,
            delegation::TaskRecord { agent: agent_name.clone(), messages: msgs.to_vec(), meta: save_meta.clone() },
        )
    };
    let scope = RunScope { checkpoint: Some(&checkpoint), workflow, lane };
    let mut outcome = Box::pin(run_agent_loop(
        client,
        &task_cfg,
        &mut messages,
        skills,
        ToolMode::All,
        child_top.as_ref(),
        depth + 1,
        Some(&scope),
    ))
    .await
    .map_err(|e| e.to_string());

    // A workflow is not finished until it has filed its handover: remind it, twice at most.
    if let Some(w) = workflow {
        let mut reminders = 0;
        while outcome.is_ok() && handover_missing(w) {
            if reminders == 2 {
                outcome = Err("the workflow ended without filing a handover, even after two reminders".into());
                break;
            }
            reminders += 1;
            messages.push(json!({ "role": "user", "content": HANDOVER_REMINDER }));
            outcome = Box::pin(run_agent_loop(
                client,
                &task_cfg,
                &mut messages,
                skills,
                ToolMode::All,
                child_top.as_ref(),
                depth + 1,
                Some(&scope),
            ))
            .await
            .map_err(|e| e.to_string());
        }
    }

    match &outcome {
        Ok(answer) => {
            delegation::save_task(&task_id, delegation::TaskRecord { agent: skill.name.clone(), messages: messages.clone(), meta });
            if let Some(emitter) = &scoped {
                // A workflow that ended blocked or failed is reported as failed; partial still ran to its handover.
                let filed = workflow.and_then(|w| w.filed.lock().ok().and_then(|f| f.as_ref().map(|h| (h.outcome, h.display_path.clone(), h.revision))));
                let failed = matches!(filed, Some((handover::Outcome::Blocked | handover::Outcome::Failed, _, _)));
                let mut end = json!({
                    "type": "task.end",
                    "status": if failed { "failed" } else { "completed" },
                    "summary": stream::truncate_chars(answer, 500),
                });
                if let Some((outcome, path, revision)) = filed {
                    end["handover"] = json!({ "outcome": outcome.as_str(), "path": path, "revision": revision });
                    if failed {
                        end["error"] = json!(format!("workflow {}", outcome.as_str()));
                    }
                }
                emitter.emit(end);
            }
        }
        Err(err) => {
            // Keep what the run got to, so the same id can pick it up again.
            delegation::save_task(
                &task_id,
                delegation::TaskRecord { agent: skill.name.clone(), messages: consistent_prefix(&messages), meta },
            );
            if let Some(emitter) = &scoped {
                emitter.emit(json!({ "type": "task.end", "status": "failed", "error": err }));
            }
        }
    }
    (outcome, messages)
}

/// The Task tool: run a NomadWorks subagent in its own context and return its
/// answer with a task_id (OpenCode's format). A known task_id resumes that
/// subagent's conversation. Failures become tool output, so the caller can react.
async fn handle_task(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    call: &PreparedCall,
    depth: u8,
    lane: Option<&str>,
) -> String {
    let (call_id, args) = (call.id.as_str(), &call.args);
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
    let (task_id, messages) = match resumed {
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

    let (outcome, _) = run_delegated(
        client,
        cfg,
        skills,
        top,
        depth,
        Delegation {
            skill,
            task_id: task_id.clone(),
            messages,
            description: args["description"].as_str().unwrap_or("").to_string(),
            mode: "subagent",
            resumed: was_resumed,
            call_id,
            meta: None,
            workflow: None,
            lane,
            model: args["model"].as_str().and_then(delegation::endpoint_model),
        },
    )
    .await;
    match outcome {
        Ok(answer) => delegation::task_result(&task_id, &answer),
        Err(err) => format!(
            "Error: subagent {} failed: {err}\n\ntask_id: {task_id} (progress was saved; pass this task_id to continue)",
            skill.name
        ),
    }
}

// ---------------------------------------------------------------- workflows (orchestrator sessions)

const HANDOVER_REMINDER: &str = "You have not filed your handover. Call the `handover` tool now with the outcome of this \
workflow. If you could not finish, file it anyway with outcome `partial`, `blocked` or `failed` and say why in open_issues. \
Do not answer in plain text instead: the handover is what the Product Manager evaluates.";

fn handover_missing(w: &WorkflowScope) -> bool {
    w.filed.lock().map(|f| f.is_none()).unwrap_or(true)
}

#[cfg(test)]
thread_local! {
    /// Tests run in their own worktree; the process-wide current directory is shared between them.
    static TEST_WORKTREE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn workflow_worktree() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_WORKTREE.with(|t| t.borrow().clone()) {
        return path;
    }
    nomad_tools::resolve_worktree()
}

/// One line per workflow event in `.codenomad/runtime/workflow-events.ndjson`
/// (git-ignored runtime state). Best effort; the stream and the handover file are the record.
fn log_workflow_event(worktree: &Path, event: Value) {
    let dir = worktree.join(".codenomad").join("runtime");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut event = event;
    event["ts"] = json!(handover::now_iso());
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(dir.join("workflow-events.ndjson")) {
        let _ = writeln!(file, "{event}");
    }
}

/// The `handover` tool: validate, then write the file. Problems come back together so they can be fixed in one go.
fn file_handover(workflow: Option<&WorkflowScope>, args: &Value) -> String {
    let Some(w) = workflow else {
        return "Error: the handover tool is only available inside a workflow session (started with nomadflow_run_workflow).".into();
    };
    let args = handover::apply_aliases(args.clone());
    let parsed = match handover::Handover::from_args(&args) {
        Ok(h) => h,
        Err(errors) => {
            let list = errors.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n");
            return format!("Error: handover rejected. Fix these and call handover again:\n{list}");
        }
    };
    match handover::file(&w.worktree, &w.task_path, &w.session_id, &w.agent, &parsed) {
        Err(err) => format!("Error: could not file the handover: {err}"),
        Ok(filed) => {
            let mut note = format!(
                "Handover filed: {} (revision {}, outcome {}, verification {}).",
                filed.display_path,
                filed.revision,
                filed.outcome.as_str(),
                filed.verification
            );
            for warning in &filed.warnings {
                note.push_str(&format!("\nWarning: {warning}"));
            }
            note.push_str("\nYou can now finish with a short closing note; the Product Manager reads the handover.");
            if let Ok(mut slot) = w.filed.lock() {
                *slot = Some(filed);
            }
            note
        }
    }
}

/// `nomadflow_run_workflow` / `nomadflow_prompt_workflow`: run a task lifecycle in a workflow session of its own
/// (a Workflow Runner orchestrating the team), ending in a handover for the Product Manager to evaluate.
///
/// The session has an id (`wf_…`). Passing it back — to either tool — resumes that session with the
/// instructions as a follow-up (a bounce-back), and records the reason in the handover's review log.
/// Runs synchronously: the Product Manager's communication with agents is synchronous, and the agent is one
/// process per message, so a "background" session would end with it.
async fn handle_nomadflow(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    call: &PreparedCall,
    depth: u8,
) -> String {
    let args = &call.args;
    let tool = call.name.as_str();
    let text_of = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let Some(instructions) = text_of("instructions").or_else(|| text_of("text")) else {
        return format!("Error: {tool} requires {}.", if tool == "nomadflow_run_workflow" { "task_path and instructions" } else { "session_id and text" });
    };

    let requested_session = text_of("session_id");
    let record = requested_session.as_deref().and_then(delegation::load_task);
    if tool == "nomadflow_prompt_workflow" && record.is_none() {
        return format!(
            "Error: unknown workflow session `{}`. Start one with nomadflow_run_workflow, or pass the session_id it returned.",
            requested_session.as_deref().unwrap_or("")
        );
    }
    let previous_meta = record.as_ref().and_then(|r| r.meta.clone());
    let meta_str = |key: &str| previous_meta.as_ref().and_then(|m| m[key].as_str()).map(str::to_string);
    let Some(task_path) = text_of("task_path").or_else(|| meta_str("task_path")) else {
        return "Error: nomadflow_run_workflow requires task_path and instructions.".into();
    };

    let fallback;
    let runner = match skill_by_name(skills, "workflow_runner").or_else(|| skill_by_name(skills, "runner")) {
        Some(skill) => skill,
        None => {
            fallback = Skill {
                name: "workflow_runner".into(),
                description: "NomadWorks workflow runner (built in)".into(),
                body: String::new(),
                directory: PathBuf::from("."),
                context: Some("fork".into()),
                policies: Vec::new(),
                model: None,
            };
            &fallback
        }
    };

    let worktree = workflow_worktree();
    let resumed = record.is_some();
    let session_id = match (&requested_session, resumed) {
        (Some(id), _) if delegation::load_task(id).is_some() => id.clone(),
        _ => delegation::new_id("wf"),
    };
    let messages = match record {
        Some(record) => {
            let mut messages = record.messages;
            let revision = meta_str("revision").unwrap_or_default();
            messages.push(json!({ "role": "user", "content": nomad_tools::bounce_prompt(&revision, &instructions) }));
            // The reason it was sent back goes in the handover's own review log.
            if let Some(path) = meta_str("handover_path") {
                if let Err(err) = handover::record_bounce(&worktree.join(&path), &session_id, &instructions) {
                    eprintln!("nomadflow: could not record the review: {err}");
                }
            }
            messages
        }
        None => agents::build_fork_messages(runner, &nomad_tools::workflow_fork_prompt(&worktree, &task_path, &instructions, &session_id)),
    };

    let scope = WorkflowScope {
        session_id: session_id.clone(),
        task_path: task_path.clone(),
        worktree: worktree.clone(),
        agent: runner.name.clone(),
        filed: Mutex::new(None),
    };
    let task_id = handover::task_id_of(&task_path);
    log_workflow_event(
        &worktree,
        json!({ "event": "workflow:started", "session_id": session_id, "task": task_path, "task_id": task_id, "resumed": resumed }),
    );

    let base_meta = json!({
        "task_path": task_path,
        "handover_path": meta_str("handover_path"),
        "revision": meta_str("revision"),
    });
    let (outcome, messages) = run_delegated(
        client,
        cfg,
        skills,
        top,
        depth,
        Delegation {
            skill: runner,
            task_id: session_id.clone(),
            messages,
            description: format!("Workflow {task_id}"),
            mode: "orchestrator",
            resumed,
            call_id: &call.id,
            meta: Some(base_meta),
            workflow: Some(&scope),
            lane: None,
            model: None,
        },
    )
    .await;

    let filed = scope.filed.lock().ok().and_then(|mut f| f.take());
    if let Some(filed) = &filed {
        // The session now knows its handover, for the next bounce.
        delegation::save_task(
            &session_id,
            delegation::TaskRecord {
                agent: runner.name.clone(),
                messages: consistent_prefix(&messages),
                meta: Some(json!({
                    "task_path": task_path,
                    "handover_path": filed.display_path,
                    "revision": filed.revision.to_string(),
                    "outcome": filed.outcome.as_str(),
                })),
            },
        );
        log_workflow_event(
            &worktree,
            json!({
                "event": "handover:filed", "session_id": session_id, "task": task_path,
                "outcome": filed.outcome.as_str(), "revision": filed.revision,
                "path": filed.display_path, "verification": filed.verification, "open_issues": filed.open_issues,
            }),
        );
    }

    let continue_hint = format!("session_id: {session_id} (pass it to nomadflow_prompt_workflow or nomadflow_run_workflow to send this workflow back or continue it)");
    match (outcome, filed) {
        (Ok(answer), Some(filed)) => {
            log_workflow_event(&worktree, json!({ "event": format!("workflow:{}", filed.outcome.as_str()), "session_id": session_id, "task": task_path }));
            let headline = match filed.outcome {
                handover::Outcome::Completed => "SUCCESS: Workflow completed".to_string(),
                handover::Outcome::Partial => "PARTIAL: Workflow ended with work left".to_string(),
                handover::Outcome::Blocked => "BLOCKED: Workflow cannot continue without a decision".to_string(),
                handover::Outcome::Failed => "Error: Workflow failed".to_string(),
            };
            let warnings = if filed.warnings.is_empty() {
                String::new()
            } else {
                format!("\n{}", filed.warnings.iter().map(|w| format!("warning: {w}")).collect::<Vec<_>>().join("\n"))
            };
            format!(
                "{headline} — handover filed (revision {}, verification {}, {} open issue(s)).\n{continue_hint}\ntask: {task_path}\nhandover: {}{warnings}\n\n\
<handover>\n{}\n</handover>\n\n<workflow_result>\n{answer}\n</workflow_result>",
                filed.revision,
                filed.verification,
                filed.open_issues,
                filed.display_path,
                filed.markdown.trim_end()
            )
        }
        (Ok(answer), None) => format!("Error: workflow ended without a handover.\n{continue_hint}\n\n{answer}"),
        (Err(err), _) => {
            log_workflow_event(&worktree, json!({ "event": "workflow:errored", "session_id": session_id, "task": task_path, "error": err }));
            format!("Error: workflow did not finish: {err}\n\n{continue_hint}; progress was saved")
        }
    }
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

    // ------------------------------------------------------------ workflow sessions and handover

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An OpenAI-compatible endpoint whose replies come from `reply(request) -> assistant message`.
    async fn scripted_model(reply: impl Fn(&Value) -> Value + Send + Sync + 'static) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let reply = Arc::new(reply);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16384];
                    let body_start = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 { return; }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") { break pos + 4; }
                    };
                    let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                    let want: usize = head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                    while buf.len() < body_start + want {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 { break; }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let req: Value = serde_json::from_slice(&buf[body_start..]).unwrap_or(json!({}));
                    let message = reply(&req);
                    let body = json!({ "id": "x", "object": "chat.completion", "created": 0, "model": "m",
                        "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }] }).to_string();
                    let out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        base
    }

    fn call_message(id: &str, name: &str, args: Value) -> Value {
        json!({ "role": "assistant", "content": null,
            "tool_calls": [{ "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } }] })
    }

    fn text_message(text: &str) -> Value {
        json!({ "role": "assistant", "content": text })
    }

    fn valid_handover(summary: &str) -> Value {
        json!({
            "outcome": "completed", "objective": "Ship the export", "summary": summary,
            "requirements": [{ "id": "R1", "text": "Exports CSV", "status": "met", "evidence": "export.spec" }],
            "plan": [{ "id": "P1", "title": "Build", "owner": "developer", "status": "done", "result": "merged" }],
            "evidence": [{ "kind": "test", "ref": "npm test", "result": "58 passed" }],
            "open_issues": [], "next_actions": [],
            "verification": { "status": "verified", "notes": "green" }
        })
    }

    fn system_text(req: &Value) -> String {
        req["messages"].as_array().map(|m| m.iter().filter(|x| x["role"] == "system").map(|x| x["content"].to_string()).collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }

    fn last_message(req: &Value) -> Value {
        req["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or(json!({}))
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Runner { Happy, RejectedFirst, NeverFiles, Failing }

    fn new_worktree(name: &str) -> PathBuf {
        let wt = std::env::temp_dir().join(format!("wf-{name}-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(wt.join("tasks/todo/TASK-9")).unwrap();
        fs::write(wt.join("tasks/todo/TASK-9/TASK-9.md"), "# TASK-9\n\n# Post Implementation Task Updates\n").unwrap();
        TEST_WORKTREE.with(|t| *t.borrow_mut() = Some(wt.clone()));
        wt
    }

    /// The Product Manager asks for `parent_call` once, then reports "PMA DONE".
    /// The workflow runner behaves as `runner` says. Counts the runner's requests.
    async fn flow(wt: &Path, runner: Runner, parent_call: Value, runner_requests: Arc<AtomicUsize>) -> Run {
        let _ = wt;
        let base = scripted_model(move |req| {
            let system = system_text(req);
            let last = last_message(req);
            let content = last["content"].as_str().unwrap_or("").to_string();
            if system.contains("workflow_runner") {
                runner_requests.fetch_add(1, Ordering::SeqCst);
                return match (last["role"].as_str().unwrap_or(""), runner) {
                    ("tool", _) if content.contains("Handover filed") => text_message("closing note"),
                    ("tool", _) if content.contains("handover rejected") => call_message("h2", "handover", valid_handover("fixed after rejection")),
                    (_, Runner::NeverFiles) => text_message("I am done, no tool needed"),
                    (_, Runner::RejectedFirst) => {
                        let mut bad = valid_handover("x");
                        bad["open_issues"] = json!(["a leftover"]);
                        call_message("h1", "handover", bad)
                    }
                    (_, Runner::Failing) => call_message("h1", "handover", json!({
                        "outcome": "failed", "objective": "Ship the export", "summary": "Build is broken",
                        "requirements": [{ "id": "R1", "text": "Exports CSV", "status": "unmet" }],
                        "plan": [{ "id": "P1", "title": "Build", "owner": "developer", "status": "failed" }],
                        "evidence": [], "open_issues": ["compile error in export.ts"], "next_actions": ["fix the compile error"],
                        "verification": { "status": "failed" }
                    })),
                    (_, Runner::Happy) if content.contains("sent the workflow back") => call_message("h3", "handover", valid_handover("fixed the header")),
                    (_, Runner::Happy) => call_message("h1", "handover", valid_handover("export shipped")),
                };
            }
            // the Product Manager
            let answered = req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool"));
            if answered { text_message("PMA DONE") } else {
                let name = parent_call["name"].as_str().unwrap_or("nomadflow_run_workflow");
                call_message("wf1", name, parent_call["args"].clone())
            }
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("workflow_runner", true)];
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "go" })];
        let started = std::time::Instant::now();
        let answer = run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, Some(&top), 0, None).await.map_err(|e| e.to_string());
        let events = sink.lock().unwrap().clone();
        Run { answer, messages, events, elapsed: started.elapsed() }
    }

    fn run_call(args: Value) -> Value {
        json!({ "name": "nomadflow_run_workflow", "args": args })
    }

    fn workflow_result(run: &Run) -> String {
        run.messages.iter().find(|m| m["role"] == "tool").and_then(|m| m["content"].as_str()).unwrap_or_default().to_string()
    }

    fn session_of(result: &str) -> String {
        result.split("session_id: ").nth(1).unwrap().split_whitespace().next().unwrap().to_string()
    }

    const TASK: &str = "tasks/todo/TASK-9/TASK-9.md";

    #[tokio::test(flavor = "multi_thread")]
    async fn a_workflow_session_ends_in_a_handover_the_manager_can_read() {
        let wt = new_worktree("happy");
        let requests = Arc::new(AtomicUsize::new(0));
        let run = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "do it" })), requests).await;
        assert_eq!(run.answer.as_deref(), Ok("PMA DONE"));

        let result = workflow_result(&run);
        assert!(result.starts_with("SUCCESS: Workflow completed — handover filed (revision 1, verification verified, 0 open issue(s))."), "{result}");
        assert!(result.contains("handover: tasks/todo/TASK-9/HANDOVER.md"));
        assert!(result.contains("<handover>") && result.contains("**Outcome: completed**"));
        assert!(result.contains("export shipped"));
        assert!(!result.contains("warning:"), "the task file has its Post Implementation section");
        let session = session_of(&result);
        assert!(session.starts_with("wf_"));

        // The file is with the task, and the session can be continued.
        let file = fs::read_to_string(wt.join("tasks/todo/TASK-9/HANDOVER.md")).unwrap();
        assert!(file.contains(&format!("session_id: {session}")) && file.contains("revision: 1"));
        let saved = delegation::load_task(&session).expect("the workflow session is kept");
        assert_eq!(saved.meta.as_ref().unwrap()["handover_path"], "tasks/todo/TASK-9/HANDOVER.md");

        // The orchestrator reports as its own task, in orchestrator mode, ending with its handover.
        let start = run.events.iter().find(|e| e["type"] == "task.start").expect("task.start");
        assert_eq!(start["mode"], "orchestrator");
        assert_eq!(start["agent"], "workflow_runner");
        assert_eq!(start["task_id"], session.as_str());
        let end = run.events.iter().find(|e| e["type"] == "task.end").expect("task.end");
        assert_eq!(end["status"], "completed");
        assert_eq!(end["handover"]["outcome"], "completed");
        assert_eq!(end["handover"]["revision"], 1);

        // And the engine's own event log has the workflow's course.
        let log = fs::read_to_string(wt.join(".codenomad/runtime/workflow-events.ndjson")).unwrap();
        let names: Vec<String> = log.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()["event"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["workflow:started", "handover:filed", "workflow:completed"]);
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_handover_that_breaks_the_rules_is_rejected_and_the_runner_fixes_it() {
        let wt = new_worktree("rejected");
        let run = flow(&wt, Runner::RejectedFirst, run_call(json!({ "task_path": TASK, "instructions": "do it" })), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(run.answer.as_deref(), Ok("PMA DONE"));
        let result = workflow_result(&run);
        assert!(result.starts_with("SUCCESS: Workflow completed"), "{result}");
        assert!(result.contains("fixed after rejection"));
        // Only the accepted one was filed.
        let file = fs::read_to_string(wt.join("tasks/todo/TASK-9/HANDOVER.md")).unwrap();
        assert!(file.contains("revision: 1") && !file.contains("a leftover"));
        // The runner was told why, in the tool result it saw.
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_workflow_that_never_files_a_handover_fails_without_ending_the_managers_run() {
        let wt = new_worktree("never");
        let requests = Arc::new(AtomicUsize::new(0));
        let run = flow(&wt, Runner::NeverFiles, run_call(json!({ "task_path": TASK, "instructions": "do it" })), requests.clone()).await;
        // Before Phase 5 a failing workflow's `?` ended the whole run; now the manager gets a result.
        assert_eq!(run.answer.as_deref(), Ok("PMA DONE"));
        let result = workflow_result(&run);
        assert!(result.starts_with("Error: workflow did not finish: the workflow ended without filing a handover, even after two reminders"), "{result}");
        assert!(result.contains("session_id: wf_") && result.contains("progress was saved"));
        assert_eq!(requests.load(Ordering::SeqCst), 3, "the first try and two reminders");
        assert!(stream::is_error_output(&result), "the card is flagged as an error");
        let end = run.events.iter().find(|e| e["type"] == "task.end").unwrap();
        assert_eq!(end["status"], "failed");
        assert!(!wt.join("tasks/todo/TASK-9/HANDOVER.md").exists());
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_workflow_hands_over_its_failure() {
        let wt = new_worktree("failed");
        let run = flow(&wt, Runner::Failing, run_call(json!({ "task_path": TASK, "instructions": "do it" })), Arc::new(AtomicUsize::new(0))).await;
        let result = workflow_result(&run);
        assert!(result.starts_with("Error: Workflow failed — handover filed"), "{result}");
        assert!(result.contains("compile error in export.ts") && result.contains("fix the compile error"));
        assert!(stream::is_error_output(&result));
        let end = run.events.iter().find(|e| e["type"] == "task.end").unwrap();
        assert_eq!(end["status"], "failed");
        assert_eq!(end["handover"]["outcome"], "failed");
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sending_a_workflow_back_resumes_the_same_session_and_logs_the_review() {
        let wt = new_worktree("bounce");
        let first = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "do it" })), Arc::new(AtomicUsize::new(0))).await;
        let session = session_of(&workflow_result(&first));

        // The Product Manager sends it back through nomadflow_prompt_workflow.
        let second = flow(
            &wt,
            Runner::Happy,
            json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": session, "text": "The CSV header is wrong, fix it." } }),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        assert_eq!(second.answer.as_deref(), Ok("PMA DONE"));
        let result = workflow_result(&second);
        assert!(result.starts_with("SUCCESS: Workflow completed — handover filed (revision 2"), "{result}");
        assert_eq!(session_of(&result), session, "the same session");
        assert!(result.contains("fixed the header"));

        let start = second.events.iter().find(|e| e["type"] == "task.start").unwrap();
        assert_eq!(start["resumed"], true);
        assert_eq!(start["task_id"], session.as_str());

        // The review of revision 1 is in the file, and survived the new revision.
        let file = fs::read_to_string(wt.join("tasks/todo/TASK-9/HANDOVER.md")).unwrap();
        assert!(file.contains("revision: 2"));
        assert!(file.contains(&format!("sent back (revision 1, session {session}): The CSV header is wrong, fix it.")), "{file}");

        // It really continued: the saved conversation holds both rounds.
        let saved = delegation::load_task(&session).unwrap();
        let handovers = saved.messages.iter().filter(|m| m["tool_calls"][0]["function"]["name"] == "handover").count();
        assert_eq!(handovers, 2);
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follow_ups_to_unknown_sessions_and_missing_arguments_are_clear_errors() {
        let wt = new_worktree("errors");
        let run = flow(&wt, Runner::Happy, json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": "wf_nope", "text": "hi" } }), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&run).starts_with("Error: unknown workflow session `wf_nope`"), "{}", workflow_result(&run));
        assert_eq!(run.answer.as_deref(), Ok("PMA DONE"));

        let run = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK })), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(workflow_result(&run), "Error: nomadflow_run_workflow requires task_path and instructions.");
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn the_handover_tool_belongs_to_workflow_sessions_only() {
        assert!(file_handover(None, &valid_handover("x")).starts_with("Error: the handover tool is only available inside a workflow session"));
        let with = tool_specs(ToolMode::All, &[], 0, true).unwrap();
        let without = tool_specs(ToolMode::All, &[], 0, false).unwrap();
        let has = |v: &Value| v.as_array().unwrap().iter().any(|t| t["function"]["name"] == "handover");
        assert!(has(&with) && !has(&without));
    }

    // ------------------------------------------------------------ hardening: step cap, lanes, Task model

    #[test]
    fn step_limits_come_from_the_environment_or_default() {
        assert_eq!(parse_steps(None, 40), 40);
        assert_eq!(parse_steps(Some("75"), 40), 75);
        assert_eq!(parse_steps(Some(" 5 "), 40), 5);
        assert_eq!(parse_steps(Some("0"), 40), 40, "zero would stop every run before it starts");
        assert_eq!(parse_steps(Some("lots"), 40), 40);
        assert_eq!(parse_steps(Some("99999"), 40), 2000, "capped");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_that_never_finishes_stops_at_the_step_limit_and_a_task_can_continue() {
        TEST_MAX_STEPS.with(|t| t.set(Some(3)));
        // The subagent asks for a tool every time; the manager calls it once and then reports.
        let base = scripted_model(|req| {
            let system = system_text(req);
            let answered = req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool" && x["content"].as_str().is_some_and(|c| c.contains("task_id"))));
            if system.contains("agent `looper`") {
                call_message(&format!("r{}", req["messages"].as_array().map(|m| m.len()).unwrap_or(0)), "Read", json!({ "file_path": "/etc/hostname" }))
            } else if answered { text_message("PMA DONE") } else {
                call_message("t1", "Task", json!({ "subagent_type": "looper", "description": "loop", "prompt": "never finish" }))
            }
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("looper", true)];
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "go" })];
        let outcome = run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, None, 0, None).await;
        TEST_MAX_STEPS.with(|t| t.set(None));
        assert_eq!(outcome.as_deref().ok(), Some("PMA DONE"));
        let result = messages.iter().rev().find(|m| m["role"] == "tool").and_then(|m| m["content"].as_str()).unwrap().to_string();
        assert!(result.starts_with("Error: subagent looper failed: the agent used all 3 steps without finishing (NOMAD_MAX_STEPS raises the limit)"), "{result}");
        let id = result.split("task_id: ").nth(1).unwrap().split_whitespace().next().unwrap();
        let saved = delegation::load_task(id).expect("its progress was kept");
        assert!(saved.messages.iter().filter(|m| m["role"] == "tool").count() >= 2, "the steps it did are in the saved conversation");
    }

    fn two_writers(shared: String, other: Option<String>) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
        move |req| {
            let system = system_text(req);
            let last = last_message(req);
            if system.contains("agent `writer`") {
                if last["role"] == "tool" { return text_message("writer done"); }
                let which = req["messages"].to_string().contains("WRITER-B");
                let (path, body) = match (&other, which) {
                    (Some(o), true) => (o.clone(), "from B"),
                    (_, true) => (shared.clone(), "from B"),
                    (_, false) => (shared.clone(), "from A"),
                };
                return call_message("w", "Write", json!({ "file_path": path, "content": body }));
            }
            let answered = req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool"));
            if answered { return text_message("PMA DONE"); }
            json!({ "role": "assistant", "content": null, "tool_calls": [
                { "id": "ta", "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "writer", "description": "a", "prompt": "WRITER-A" }).to_string() } },
                { "id": "tb", "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "writer", "description": "b", "prompt": "WRITER-B" }).to_string() } },
            ] })
        }
    }

    async fn run_writers(reply: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Vec<String> {
        let base = scripted_model(reply).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("writer", true)];
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "go" })];
        let outcome = run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, None, 0, None).await;
        assert_eq!(outcome.as_deref().ok(), Some("PMA DONE"));
        messages.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or_default().to_string()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_parallel_tasks_cannot_overwrite_the_same_file() {
        let dir = std::env::temp_dir().join(format!("lanes-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.txt").to_string_lossy().to_string();
        let results = run_writers(two_writers(shared.clone(), None)).await;
        // The refusal goes to the writer that was too late; the manager only sees each writer's final answer.
        let refusals: Vec<String> = results
            .iter()
            .filter_map(|r| r.split("task_id: ").nth(1).and_then(|t| t.split_whitespace().next()).and_then(delegation::load_task))
            .flat_map(|task| task.messages)
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["content"].as_str().map(str::to_string))
            .filter(|c| c.contains("is being written by another task running in parallel"))
            .collect();
        assert_eq!(refusals.len(), 1, "exactly one of the two writers is turned away: {refusals:?}");
        assert!(refusals[0].starts_with(&format!("Error: {shared} is being written by another task running in parallel (t")), "{}", refusals[0]);
        let content = fs::read_to_string(&shared).unwrap();
        assert!(content == "from A" || content == "from B", "one writer's content, whole: {content}");
        // And the file is free again once the batch is over.
        assert_eq!(lanes::claim(&shared, "later"), Ok(()));
        lanes::release("later");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_tasks_writing_different_files_do_not_get_in_each_others_way() {
        let dir = std::env::temp_dir().join(format!("lanes2-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt").to_string_lossy().to_string(), dir.join("b.txt").to_string_lossy().to_string());
        let results = run_writers(two_writers(a.clone(), Some(b.clone()))).await;
        assert!(results.iter().all(|r| !r.contains("Error")), "{results:?}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "from A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "from B");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_task_model_names_the_endpoints_model() {
        assert_eq!(delegation::endpoint_model("ollama/qwen3.6:latest").as_deref(), Some("qwen3.6:latest"));
        assert_eq!(delegation::endpoint_model(" gemma4:26b ").as_deref(), Some("gemma4:26b"));
        assert_eq!(delegation::endpoint_model("ollama/").as_deref(), None);
        assert_eq!(delegation::endpoint_model("   ").as_deref(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_can_name_its_model_and_it_is_used_for_that_task_only() {
        let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let record = seen.clone();
        let base = scripted_model(move |req| {
            let system = system_text(req);
            let model = req["model"].as_str().unwrap_or("").to_string();
            let who = if system.contains("agent `named`") { "named" } else if system.contains("agent `plain`") { "plain" } else { "pma" };
            record.lock().unwrap().push((who.to_string(), model));
            if who != "pma" { return text_message("ok"); }
            let answered = req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool"));
            if answered { return text_message("PMA DONE"); }
            json!({ "role": "assistant", "content": null, "tool_calls": [
                { "id": "t1", "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "named", "description": "n", "prompt": "p", "model": "ollama/qwen3.6:latest" }).to_string() } },
                { "id": "t2", "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "plain", "description": "p", "prompt": "p" }).to_string() } },
            ] })
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "default-model".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("named", true), skill("plain", true)];
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "go" })];
        run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, None, 0, None).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(seen.contains(&("named".into(), "qwen3.6:latest".into())), "{seen:?}");
        assert!(seen.contains(&("plain".into(), "default-model".into())), "{seen:?}");
        assert!(seen.contains(&("pma".into(), "default-model".into())));
    }
}
