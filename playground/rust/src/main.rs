mod agents;
mod catalog;
mod config;
mod control;
mod delegation;
mod file_lease;
mod handover;
mod inputs;
mod lanes;
mod nomad_tools;
mod opencode_config;
mod patch;
mod permissions;
mod skills;
mod stream;
mod tools;
mod workflows;

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

fn tool_specs(mode: ToolMode, skills: &[Skill], depth: u8, workflow: bool, model: &str) -> Option<Value> {
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
            // Edit, Grep, Glob, WebFetch, TodoWrite, TodoRead (OpenCode's tool surface).
            tools.extend(tools::tool_specs());

            // OpenCode gives GPT models `apply_patch` in place of Edit and Write.
            if patch::offered_for(model) {
                tools.retain(|t| !matches!(t["function"]["name"].as_str(), Some("Edit" | "Write")));
                tools.push(patch::tool_spec());
            }

            // A workflow session reports back through its handover.
            if workflow {
                tools.push(handover::tool_spec());
            } else if control::can_ask() {
                // Someone can answer (the host passes answers back); an unattended workflow runner must not wait for one.
                tools.push(tools::question_spec());
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

/// Context window of each model, from Ollama's `/api/show` next to the `/v1` base. Models that do not answer
/// within a few seconds (a cloud model, another kind of endpoint) are left out.
async fn fetch_contexts(base_url: &str, api_key: &str, models: &[String]) -> std::collections::HashMap<String, u64> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1").to_string();
    let url = format!("{root}/api/show");
    let client = reqwest::Client::new();
    futures_util::stream::iter(models.iter().take(80).cloned())
        .map(|id| {
            let (client, url) = (client.clone(), url.clone());
            async move {
                let response = client
                    .post(&url)
                    .bearer_auth(api_key)
                    .json(&json!({ "model": id }))
                    .timeout(std::time::Duration::from_secs(4))
                    .send()
                    .await
                    .ok()?;
                let body: Value = response.json().await.ok()?;
                catalog::context_from_show(&body).map(|context| (id, context))
            }
        })
        .buffer_unordered(8)
        .filter_map(|found| async move { found })
        .collect()
        .await
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
    /// What the last `nomadworks_validate` of this run said: the first line, when it failed. A handover may not claim
    /// `verified` over a check that failed.
    last_validation_failure: Mutex<Option<String>>,
    /// Said after a handover that is not `completed` for a reason the runner can fix itself (see `self_correction`).
    nudge: Mutex<Option<String>>,
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
    /// Who owns this run's todo list: the delegated task's id; none for the top-level run.
    owner: Option<&'a str>,
    /// The permission rules of the agent this run is (permissions.rs); none: nothing is checked.
    rules: Option<&'a permissions::Policy>,
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
    let tools = tool_specs(tool_mode, skills, depth, workflow.is_some(), &cfg.model);

    let step_cap = max_steps(workflow.is_some());
    let effort = thinking_effort(env::var("NOMAD_THINKING").ok().as_deref());
    // A stalled step (no answer, no tool call) is tried again, differently, a few times per run.
    let mut stalls = 0u32;
    let mut retry_after: Option<Stall> = None;
    // Whether this run has handed work on, and whether it was already reminded to.
    let mut handed_on = false;
    let mut reminded = false;
    for _ in 0..step_cap {
        let mut outgoing = inputs::merge_system_messages(messages);
        let retrying = retry_after.take();
        // For this request only: the nudge is not part of the conversation.
        if retrying.is_some() {
            add_user_note(&mut outgoing, STALL_NUDGE);
        }
        let mut request = json!({
            "model": cfg.model,
            // One system message, first: strict chat templates reject any other layout.
            "messages": outgoing,
            "max_tokens": cfg.max_tokens,
        });
        if let Some(tools) = &tools {
            request["tools"] = tools.clone();
        }
        // A step lost to thinking is tried again without it; otherwise the user's setting, if any.
        if let Some(effort) = if retrying == Some(Stall::Thinking) { Some("none") } else { effort } {
            request["reasoning_effort"] = json!(effort);
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
                let mut attempt = 0;
                let response: Value = loop {
                    match client.chat().create_byot(request.clone()).await {
                        Ok(response) => break response,
                        // A gateway in front of a slow model answers 502/504 when the model took too long (a cold start, a busy
                        // server): the next try, with the model warm, usually goes through. Anything else is the caller's to see.
                        Err(err) if attempt < gateway_retries() && is_gateway_error(&err.to_string()) => {
                            attempt += 1;
                            eprintln!("model endpoint: {err} — trying again ({attempt}/{})", gateway_retries());
                            tokio::time::sleep(gateway_backoff(attempt)).await;
                        }
                        Err(err) => return Err(format_api_error(err)),
                    }
                };
                let mut message = response["choices"]
                    .as_array()
                    .and_then(|a| a.first())
                    .ok_or("no choices in response")?["message"]
                    .clone();
                if response["usage"].is_object() {
                    message["usage"] = response["usage"].clone();
                }
                if let Some(reason) = response["choices"][0]["finish_reason"].as_str() {
                    message["finish_reason"] = json!(reason);
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

        // Nothing to go on with: the model thought until it was cut off, or sent an empty reply. The thinking is not an
        // answer (showing it as one is how a run "ends" on a page of planning), so the step is tried again.
        if let Some(stall) = stall_of(message) {
            if stalls < stall_retries() {
                stalls += 1;
                eprintln!(
                    "model: {} — trying the step again{} ({stalls}/{})",
                    if stall == Stall::Thinking { "the reply ended while it was still thinking, with no answer and no tool call" } else { "empty reply" },
                    if stall == Stall::Thinking { " with thinking off" } else { "" },
                    stall_retries()
                );
                retry_after = Some(stall);
                continue;
            }
            if stall == Stall::Thinking {
                return Err(format!(
                    "the model ({}) spent its whole reply thinking and gave no answer and no tool call, {} times in a row. \
Choose another model for this agent, or limit its thinking (NOMAD_THINKING=off or low).",
                    cfg.model,
                    stalls + 1
                )
                .into());
            }
        }
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
                // What the endpoint actually sent, so a model that answers in an unexpected shape can be told from one that is silent.
                let raw = stream::truncate_chars(&message.to_string(), 600);
                eprintln!("model answered with no text and no tool call: {raw}");
                return Err("assistant returned empty content".into());
            }
            // An agent that can delegate and stops on "I will now delegate…" has started nothing: say so, once.
            // (A workflow session has its own reminders, for its handover.)
            if !reminded && !handed_on && workflow.is_none() && delegation_nudge_on() && can_hand_on(tools.as_ref()) && announces_unstarted_work(&text) {
                reminded = true;
                eprintln!("agent: the turn ended on a promise of work with no tool call — reminding it once");
                if let Some(top) = top {
                    top.emitter.delta("\n\n");
                }
                messages.push(json!({ "role": "user", "content": DELEGATION_NUDGE }));
                continue;
            }
            return Ok(text);
        }

        // Normalize every call first: models name tools OpenCode-style and send
        // arguments as a string or an object, under varying keys.
        let calls: Vec<PreparedCall> = tool_calls.iter().map(PreparedCall::from_value).collect();
        handed_on |= calls.iter().any(|c| matches!(c.name.as_str(), "Task" | "nomadflow_run_workflow" | "nomadflow_prompt_workflow"));
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
                run_task_batch(client, cfg, skills, top, &calls[idx..end], &mut outputs[idx..end], depth, scope.and_then(|s| s.lane), scope.and_then(|s| s.rules)).await;
                idx = end;
                continue;
            }
            let call = &calls[idx];
            emit_tool(top, call);
            let result = if let Some(blocked) = gate(top, scope, call).await {
                blocked
            } else if call.name == "question" {
                handle_question(top, call).await
            } else if matches!(call.name.as_str(), "nomadflow_run_workflow" | "nomadflow_prompt_workflow") {
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

/// Checks a call against the agent's permission rules (permissions.rs). `None`: go ahead. `Some(text)`: do not run it, and
/// this is the result the model gets (a rule forbids it, the user said no, or nobody is here to ask).
async fn gate(top: Option<&TopLevel<'_>>, scope: Option<&RunScope<'_>>, call: &PreparedCall) -> Option<String> {
    let rules = scope.and_then(|s| s.rules)?;
    gate_with(top, rules, scope.is_some_and(|s| s.workflow.is_some()), call).await
}

async fn gate_with(top: Option<&TopLevel<'_>>, rules: &permissions::Policy, unattended: bool, call: &PreparedCall) -> Option<String> {
    let cwd = env::current_dir().unwrap_or_default();
    let requests = permissions::requests_for(&call.name, &call.args, &cwd);
    if requests.is_empty() {
        return None;
    }
    match permissions::check(rules, control::can_ask() && !unattended, &requests) {
        permissions::Verdict::Allow => None,
        permissions::Verdict::Deny(rule) => Some(permissions::denied_text(&rule)),
        permissions::Verdict::Ask(asked) => {
            // A workflow runner works unattended: it must not wait for an answer.
            let Some(top) = top.filter(|_| control::can_ask() && !unattended) else {
                return Some(format!(
                    "Error: this tool call needs the user's permission ({} {}) and nobody is here to give it. Use another way, or say what you need.",
                    asked[0].permission, asked[0].pattern
                ));
            };
            let id = delegation::new_id("per");
            top.emitter.emit(json!({
                "type": "permission",
                "id": id,
                "call_id": call.id,
                "permission": asked[0].permission,
                "patterns": asked.iter().map(|r| r.pattern.clone()).collect::<Vec<_>>(),
                "always": asked.iter().map(|r| r.always.clone()).collect::<Vec<_>>(),
            }));
            match control::wait_for_reply(&id).await {
                control::Reply::Once => None,
                control::Reply::Always => {
                    permissions::remember(&asked);
                    None
                }
                control::Reply::Reject => Some("The user rejected permission to use this specific tool call. You may try again with different parameters.".into()),
            }
        }
    }
}

/// The `question` tool: shows the user the questions and waits for the answers the host passes back.
/// A task that is stopped while it waits is dropped by its caller (control.rs), so no timeout is needed here.
async fn handle_question(top: Option<&TopLevel<'_>>, call: &PreparedCall) -> String {
    let Some(top) = top.filter(|_| control::can_ask()) else {
        return "Error: nobody is here to answer questions in this run. Make a reasonable assumption, say what you assumed, and carry on.".into();
    };
    let questions = match tools::parse_questions(&call.args) {
        Ok(questions) => questions,
        Err(err) => return err,
    };
    let id = delegation::new_id("que");
    top.emitter.emit(json!({ "type": "question", "id": id, "call_id": call.id, "questions": questions }));
    match control::wait_for_answer(&id).await {
        control::Answer::Answers(answers) => tools::answered_text(&questions, &answers),
        control::Answer::Dismissed => "Error: The user dismissed this question".into(),
    }
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
        let mut args = delegation::apply_aliases(&name, delegation::parse_arguments(&call["function"]["arguments"]));
        if name == "TodoWrite" {
            // The model, and the UI's plan panel, see the list in canonical form.
            args = tools::normalize_todo_args(args);
        }
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

/// How much of a tool's output goes to the UI. An Edit's output is the diff its card draws, so a cut-off hunk would
/// show wrong line counts; it gets room for the largest diff the tool produces (300 lines).
fn ui_output_limit(tool: &str) -> usize {
    if tool == "Edit" { 30_000 } else { 2_000 }
}

fn emit_tool_result(top: Option<&TopLevel<'_>>, call: &PreparedCall, result: &str) {
    if let Some(top) = top {
        top.emitter.emit(json!({
            "type": "tool_result",
            "id": call.id,
            "name": call.name,
            "output": stream::truncate_chars(result, ui_output_limit(&call.name)),
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
        "Write" | "Edit" => {
            // Among Task calls running at the same time, a file belongs to the first lane that writes it.
            if let Some(lane) = scope.and_then(|s| s.lane) {
                if let Err(owner) = lanes::claim(file_path, lane) {
                    return Ok(format!(
                        "Error: {file_path} is being written by another task running in parallel ({owner}). Do not edit it from here: leave it to that task, \
or report back so the work can be done one after the other."
                    ));
                }
            }
            let run = |args: &Value| if call.name == "Edit" { tools::edit(args) } else { execute_write(args) };
            file_lease::with_lease(file_path, &call.id, || run(fn_args)).unwrap_or_else(|err| format!("Error: {err}"))
        }
        // Searches walk the disk: off the async thread, like Bash.
        "Grep" => {
            let args = fn_args.clone();
            tokio::task::spawn_blocking(move || tools::grep(&args)).await.unwrap_or_else(|err| format!("Error: search did not complete: {err}"))
        }
        "Glob" => {
            let args = fn_args.clone();
            tokio::task::spawn_blocking(move || tools::glob(&args)).await.unwrap_or_else(|err| format!("Error: search did not complete: {err}"))
        }
        "apply_patch" => {
            let text = fn_args["patchText"].as_str().unwrap_or("");
            // Among Task calls running at the same time, a file belongs to the first lane that writes it.
            if let Some(lane) = scope.and_then(|s| s.lane) {
                for path in patch::paths(text) {
                    if let Err(owner) = lanes::claim(&path, lane) {
                        return Ok(format!(
                            "Error: {path} is being written by another task running in parallel ({owner}). Do not edit it from here: leave it to that task, \
or report back so the work can be done one after the other."
                        ));
                    }
                }
            }
            let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match patch::apply(text, &base) {
                Ok(done) => done,
                Err(err) => format!("Error: apply_patch failed: {err}"),
            }
        }
        "WebFetch" => tools::webfetch(fn_args).await,
        "TodoWrite" => tools::todowrite(scope.and_then(|s| s.owner).unwrap_or("root"), fn_args),
        "TodoRead" => tools::todoread(scope.and_then(|s| s.owner).unwrap_or("root")),
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
                if other == "nomadworks_validate" {
                    if let Some(w) = workflow {
                        if let Ok(mut last) = w.last_validation_failure.lock() {
                            *last = out.lines().find(|l| l.trim_start().starts_with("FAIL")).map(|l| l.trim().to_string());
                        }
                    }
                }
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
    rules: Option<&permissions::Policy>,
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
            } else if let Some(blocked) = match rules {
                Some(rules) => gate_with(top, rules, false, call).await,
                None => None,
            } {
                blocked
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
    /// This process is the run itself (a workflow runner's own process): its events are the process's
    /// own, not a task's, so there is no `task.start` / `task.end`.
    inline: bool,
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
    let Delegation { skill, task_id, mut messages, description, mode, resumed, call_id, meta, workflow, lane, model, inline } = d;
    let task_cfg = match model.as_ref().or(skill.model.as_ref()) {
        Some(model) => cfg.with_model(model),
        None => cfg.clone(),
    };
    eprintln!("{mode} `{}` ({task_id}): model {}", skill.name, task_cfg.model);

    // Protocol 2: this run's events are stamped with its id and go to the UI live.
    let scoped = top.filter(|t| t.protocol2 && !inline).map(|t| t.emitter.scoped(&task_id, call_id));
    let child_top = match (inline, top) {
        (true, Some(t)) => Some(TopLevel { emitter: t.emitter, base_url: t.base_url.clone(), stream: t.stream, protocol2: t.protocol2 }),
        _ => top.zip(scoped.as_ref()).map(|(t, emitter)| TopLevel {
            emitter,
            base_url: t.base_url.clone(),
            stream: t.stream,
            protocol2: true,
        }),
    };
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
    let policy = permissions::policy_for(&skill.name);
    let scope = RunScope { checkpoint: Some(&checkpoint), workflow, lane, owner: Some(task_id.as_str()), rules: Some(&policy) };
    // A task the user stops is dropped where it stands (control.rs); a workflow runner's own process is stopped as a whole.
    let stoppable = !inline;
    if stoppable {
        control::clear(&task_id);
    }
    let mut outcome = run_stoppable(stoppable, &task_id, async {
        Box::pin(run_agent_loop(client, &task_cfg, &mut messages, skills, ToolMode::All, child_top.as_ref(), depth + 1, Some(&scope)))
            .await
            .map_err(|e| e.to_string())
    })
    .await;

    // A workflow is not finished until it has filed its handover: remind it, twice at most.
    if let Some(w) = workflow {
        let mut reminders = 0;
        let mut nudges = 0;
        while outcome.is_ok() {
            if handover_missing(w) {
                if reminders == 2 {
                    outcome = Err("the workflow ended without filing a handover, even after two reminders".into());
                    break;
                }
                reminders += 1;
                messages.push(json!({ "role": "user", "content": HANDOVER_REMINDER }));
            } else {
                // Filed, but for a reason the runner can mend itself: say so, up to twice, before it goes back to the manager.
                let nudge = if nudges < 2 { w.nudge.lock().ok().and_then(|mut n| n.take()) } else { None };
                let Some(nudge) = nudge else { break };
                nudges += 1;
                eprintln!("workflow {}: sending the runner back to fix itself ({nudges}/2)", w.session_id);
                messages.push(json!({ "role": "user", "content": nudge }));
            }
            outcome = run_stoppable(stoppable, &task_id, async {
                Box::pin(run_agent_loop(client, &task_cfg, &mut messages, skills, ToolMode::All, child_top.as_ref(), depth + 1, Some(&scope)))
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;
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
                if err == control::STOPPED {
                    emitter.emit(json!({ "type": "task.end", "status": "aborted", "summary": "Stopped by the user" }));
                } else {
                    emitter.emit(json!({ "type": "task.end", "status": "failed", "error": err }));
                }
            }
        }
    }
    (outcome, messages)
}

async fn run_stoppable<T>(stoppable: bool, task_id: &str, work: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    if stoppable { control::stoppable(task_id, work).await } else { work.await }
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
            inline: false,
        },
    )
    .await;
    match outcome {
        Ok(answer) => delegation::task_result(&task_id, &answer),
        Err(err) if err == control::STOPPED => format!(
            "The user stopped subagent {} before it finished; what it had done so far was kept.\n\ntask_id: {task_id} (pass this task_id to continue it, or carry on without it)",
            skill.name
        ),
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
    // `verified` over a check that just failed is a false claim: the runner fixes the cause or files `unverified`.
    if parsed.verification == "verified" {
        // The runner's word is not the check: the project's own validator is run here, whether or not the runner ran it.
        let failure = w.last_validation_failure.lock().ok().and_then(|f| f.clone()).or_else(|| nomad_tools::failing_validation(&w.worktree));
        if let Some(failure) = failure {
            return format!(
                "Error: handover rejected. Fix these and call handover again:\n- verification is `verified`, but the project's validation says: {failure}\n  Fix what it names and run nomadworks_validate again, or file the handover with verification `unverified` and say why."
            );
        }
    }
    // Evidence that names a file must name one that is there (a model can describe a change it never made).
    let missing = handover::missing_evidence_files(&w.worktree, &w.task_path, &parsed);
    if !missing.is_empty() {
        let list = missing.iter().map(|m| format!("- evidence file `{m}` does not exist (relative to the project, or to the task's folder). Point at a file that exists, or use kind `note`.")).collect::<Vec<_>>().join("\n");
        return format!("Error: handover rejected. Fix these and call handover again:\n{list}");
    }
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
            if let Ok(mut slot) = w.nudge.lock() {
                *slot = self_correction(&parsed, w.last_validation_failure.lock().ok().and_then(|f| f.clone()));
            }
            note
        }
    }
}

/// What to tell a runner whose handover is not `completed` for a reason it can mend itself, so the fix does not wait for
/// the manager or a person: a validation that failed (the work is not done), or a handover whose requirements are all met
/// and verified but whose plan still lists steps (do them, or skip the ones outside the task).
fn self_correction(handover: &handover::Handover, validation_failure: Option<String>) -> Option<String> {
    use handover::Outcome;
    if handover.outcome == Outcome::Completed {
        return None;
    }
    if let Some(failure) = validation_failure {
        return Some(format!(
            "Your handover is {}, and the last nomadworks_validate said: {failure}\nDo the work that makes it pass (write the real content: describing a change does not make it), run nomadworks_validate again, and then file the handover again. If it cannot be done, say so in the handover.",
            handover.outcome.as_str()
        ));
    }
    let met = handover.requirements.iter().all(|r| r.status == "met" || r.status == "waived");
    let pending: Vec<&str> = handover.plan.iter().filter(|p| p.status == "pending").map(|p| p.id.as_str()).collect();
    if handover.verification == "verified" && met && !pending.is_empty() {
        return Some(format!(
            "Every requirement is met and verified, but your plan still lists these steps as pending: {}. If they are not part of this task, mark them `skipped` and say why in their result; if they are, do them. Then file the handover again, dropping any open issue this settles (a completed handover needs every step done or skipped, and no open issues).",
            pending.join(", ")
        ));
    }
    None
}

/// Whether `nomadflow_*` start workflows in the background (`NOMAD_WORKFLOW_MODE=async`), as OpenCode's
/// NomadWorks plugin does: the call returns at once, the Workflow Runner works in a session of its own
/// (run by the host in a second process, see `run_workflow_session`), and the Product Manager is told
/// when it is done. Off by default: without a host that does this, a background session would end with
/// the process, so the workflow runs inside the call instead.
fn workflow_async() -> bool {
    #[cfg(test)]
    if let Some(forced) = TEST_ASYNC.with(|t| t.get()) {
        return forced;
    }
    env::var("NOMAD_WORKFLOW_MODE").map(|v| v.trim().eq_ignore_ascii_case("async")).unwrap_or(false)
}

#[cfg(test)]
thread_local! {
    static TEST_ASYNC: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// A workflow session ready to run: the runner, its id and the conversation it starts (or resumes) from.
struct Prepared {
    runner: Skill,
    /// The workflow's id (`wf_…`); the host's session id is `ses_lcl_` + this.
    session_id: String,
    task_path: String,
    resumed: bool,
    messages: Vec<Value>,
    previous_meta: Option<Value>,
    worktree: PathBuf,
}

/// Reads a `nomadflow_*` call: what to run, and the conversation to run it with. `forced_id` is the id a
/// new workflow must get (the host and the Product Manager already know it).
fn prepare_workflow(tool: &str, args: &Value, skills: &[Skill], forced_id: Option<&str>) -> Result<Prepared, String> {
    let text_of = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let Some(instructions) = text_of("instructions").or_else(|| text_of("text")) else {
        return Err(format!("Error: {tool} requires {}.", if tool == "nomadflow_run_workflow" { "task_path and instructions" } else { "session_id and text" }));
    };

    let requested_session = text_of("session_id").map(|s| workflows::workflow_id(&s));
    let record = requested_session.as_deref().and_then(delegation::load_task);
    if tool == "nomadflow_prompt_workflow" && record.is_none() {
        return Err(format!(
            "Error: unknown workflow session `{}`. Start one with nomadflow_run_workflow, or pass the session_id it returned.",
            requested_session.as_deref().unwrap_or("")
        ));
    }
    let previous_meta = record.as_ref().and_then(|r| r.meta.clone());
    let meta_str = |key: &str| previous_meta.as_ref().and_then(|m| m[key].as_str()).map(str::to_string);
    let Some(task_path) = text_of("task_path").or_else(|| meta_str("task_path")) else {
        return Err("Error: nomadflow_run_workflow requires task_path and instructions.".into());
    };

    let runner = match skill_by_name(skills, "workflow_runner").or_else(|| skill_by_name(skills, "runner")) {
        Some(skill) => skill.clone(),
        None => Skill {
            name: "workflow_runner".into(),
            description: "NomadWorks workflow runner (built in)".into(),
            body: String::new(),
            directory: PathBuf::from("."),
            context: Some("fork".into()),
            policies: Vec::new(),
            model: None,
        },
    };

    let worktree = workflow_worktree();
    let resumed = record.is_some();
    let session_id = match (&requested_session, resumed) {
        (Some(id), true) => id.clone(),
        _ => forced_id.map(str::to_string).unwrap_or_else(|| delegation::new_id("wf")),
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
        None => agents::build_fork_messages(&runner, &nomad_tools::workflow_fork_prompt(&worktree, &task_path, &instructions, &session_id)),
    };
    Ok(Prepared { runner, session_id, task_path, resumed, messages, previous_meta, worktree })
}

/// How a workflow session ended.
struct Finished {
    outcome: Result<String, String>,
    filed: Option<handover::Filed>,
}

/// Runs a prepared workflow to its handover. `inline`: this process *is* the runner's session, so its events
/// are the process's own (not a task's) — see `run_workflow_session`.
// One argument more than clippy likes; the call sites pass the same run context every time.
#[allow(clippy::too_many_arguments)]
async fn run_prepared(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    depth: u8,
    prepared: &Prepared,
    call_id: &str,
    inline: bool,
) -> Finished {
    let Prepared { runner, session_id, task_path, resumed, previous_meta, worktree, .. } = prepared;
    let meta_str = |key: &str| previous_meta.as_ref().and_then(|m| m[key].as_str()).map(str::to_string);
    let scope = WorkflowScope {
        session_id: session_id.clone(),
        task_path: task_path.clone(),
        worktree: worktree.clone(),
        agent: runner.name.clone(),
        filed: Mutex::new(None),
        last_validation_failure: Mutex::new(None),
        nudge: Mutex::new(None),
    };
    let task_id = handover::task_id_of(task_path);
    log_workflow_event(
        worktree,
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
            messages: prepared.messages.clone(),
            description: format!("Workflow {task_id}"),
            mode: "orchestrator",
            resumed: *resumed,
            call_id,
            meta: Some(base_meta),
            workflow: Some(&scope),
            lane: None,
            model: None,
            inline,
        },
    )
    .await;

    let filed = scope.filed.lock().ok().and_then(|mut f| f.take());
    if let Some(filed) = &filed {
        // The session now knows its handover, for the next bounce.
        delegation::save_task(
            session_id,
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
            worktree,
            json!({
                "event": "handover:filed", "session_id": session_id, "task": task_path,
                "outcome": filed.outcome.as_str(), "revision": filed.revision,
                "path": filed.display_path, "verification": filed.verification, "open_issues": filed.open_issues,
            }),
        );
    }
    match (&outcome, &filed) {
        (Ok(_), Some(filed)) => log_workflow_event(worktree, json!({ "event": format!("workflow:{}", filed.outcome.as_str()), "session_id": session_id, "task": task_path })),
        (Err(err), _) => log_workflow_event(worktree, json!({ "event": "workflow:errored", "session_id": session_id, "task": task_path, "error": err })),
        _ => {}
    }
    Finished { outcome, filed }
}

fn headline_of(outcome: handover::Outcome) -> &'static str {
    match outcome {
        handover::Outcome::Completed => "SUCCESS: Workflow completed",
        handover::Outcome::Partial => "PARTIAL: Workflow ended with work left",
        handover::Outcome::Blocked => "BLOCKED: Workflow cannot continue without a decision",
        handover::Outcome::Failed => "Error: Workflow failed",
    }
}

/// `nomadflow_run_workflow` / `nomadflow_prompt_workflow`: run a task lifecycle in a workflow session of its own
/// (a Workflow Runner orchestrating the team), ending in a handover for the Product Manager to evaluate.
///
/// The session has an id (`wf_…`). Passing it back — to either tool — resumes that session with the
/// instructions as a follow-up (a bounce-back), and records the reason in the handover's review log.
/// By default this runs inside the call (the agent is one process per message, so a "background" session
/// would end with it); with `NOMAD_WORKFLOW_MODE=async` the call only starts the workflow and the host runs it.
async fn handle_nomadflow(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    call: &PreparedCall,
    depth: u8,
) -> String {
    // Background workflows are the Product Manager's: a nested caller (a workflow's own orchestrator) waits for its result.
    if let (true, 0, Some(top)) = (workflow_async(), depth, top) {
        return nomadflow_in_background(top, call);
    }
    let prepared = match prepare_workflow(&call.name, &call.args, skills, None) {
        Ok(prepared) => prepared,
        Err(message) => return message,
    };
    let Finished { outcome, filed } = run_prepared(client, cfg, skills, top, depth, &prepared, &call.id, false).await;

    let session_id = &prepared.session_id;
    let task_path = &prepared.task_path;
    let continue_hint = format!("session_id: {session_id} (pass it to nomadflow_prompt_workflow or nomadflow_run_workflow to send this workflow back or continue it)");
    match (outcome, filed) {
        (Ok(answer), Some(filed)) => {
            let warnings = if filed.warnings.is_empty() {
                String::new()
            } else {
                format!("\n{}", filed.warnings.iter().map(|w| format!("warning: {w}")).collect::<Vec<_>>().join("\n"))
            };
            format!(
                "{} — handover filed (revision {}, verification {}, {} open issue(s)).\n{continue_hint}\ntask: {task_path}\nhandover: {}{warnings}\n\n\
<handover>\n{}\n</handover>\n\n<workflow_result>\n{answer}\n</workflow_result>",
                headline_of(filed.outcome),
                filed.revision,
                filed.verification,
                filed.open_issues,
                filed.display_path,
                filed.markdown.trim_end()
            )
        }
        (Ok(answer), None) => format!("Error: workflow ended without a handover.\n{continue_hint}\n\n{answer}"),
        (Err(err), _) => format!("Error: workflow did not finish: {err}\n\n{continue_hint}; progress was saved"),
    }
}

/// The asynchronous `nomadflow_*`: registers the workflow, tells the host to run it (a `workflow.start` /
/// `workflow.prompt` event) and returns at once, in the words of NomadWorks' plugin.
fn nomadflow_in_background(top: &TopLevel<'_>, call: &PreparedCall) -> String {
    let tool = call.name.as_str();
    let args = &call.args;
    let text_of = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let worktree = workflow_worktree();
    let asking = if tool == "nomadflow_run_workflow" { "run complex workflows" } else { "send workflow runner prompts" };
    if workflows::team_mode_blocks_workflows(&worktree) {
        return format!("FAIL: Workflow Runner is unavailable in the current team configuration. Switch to full team mode to {asking}.");
    }
    let manager = env::var("CODENOMAD_SESSION_KEY").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "this session".into());
    const BUSY: &str = "FAIL: A shared-worktree implementation workflow is already running. You may continue investigation or spec work separately, but only one implementation workflow can own the shared worktree at a time.";

    // `nomadflow_run_workflow` with a session_id continues that session, as in the synchronous mode.
    let follow_up = if tool == "nomadflow_prompt_workflow" || text_of("session_id").is_some() { Some(text_of("session_id")) } else { None };
    if let Some(requested) = follow_up {
        let (Some(requested), Some(text)) = (requested, text_of("text").or_else(|| text_of("instructions"))) else {
            return format!("Error: {tool} requires {}.", if tool == "nomadflow_run_workflow" { "task_path and instructions" } else { "session_id and text" });
        };
        let id = workflows::workflow_id(&requested);
        let Some(entry) = workflows::find(&worktree, &id) else {
            return format!("Error: unknown workflow session `{requested}`. Start one with nomadflow_run_workflow, or pass the session_id it returned.");
        };
        let session = workflows::session_id(&id);
        // Sending a workflow back again and again, with no completed handover, is the manager and runner
        // talking to each other with nobody watching: after the limit the user is asked.
        let limit = workflows::max_send_backs();
        if limit > 0 && entry.send_backs >= limit {
            return format!(
                "FAIL: Workflow '{requested}' has already been sent back {} times since the user last wrote (limit {limit}). Do not send it back again. \
Tell the user where it stands (the latest handover's outcome and open issues) and ask how to proceed: retry with different instructions, change the task, or stop. A message from the user lifts this limit.",
                entry.send_backs
            );
        }
        workflows::set_send_backs(&worktree, &id, entry.send_backs + 1);
        if entry.running() {
            top.emitter.emit(json!({ "type": "workflow.prompt", "workflow_id": id, "session_id": session, "text": text, "active": true, "call_id": call.id }));
            return format!("SUCCESS: Prompt sent to active session '{requested}'. Instructions added to queue. You will be notified on completion in this session ({manager}).");
        }
        if entry.track == "implementation" && workflows::running_implementation(&worktree).is_some() {
            return BUSY.into();
        }
        workflows::set_state(&worktree, &id, "running");
        log_workflow_event(&worktree, json!({ "event": "workflow:resumed", "session_id": id, "task": entry.task_path }));
        top.emitter.emit(json!({ "type": "workflow.prompt", "workflow_id": id, "session_id": session, "text": text, "active": false, "call_id": call.id }));
        return format!("SUCCESS: Session '{requested}' was not tracked. Sent prompt and resumed monitoring. You will be notified on completion in this session ({manager}).");
    }

    let (Some(task_path), Some(instructions)) = (text_of("task_path"), text_of("instructions")) else {
        return "Error: nomadflow_run_workflow requires task_path and instructions.".into();
    };
    let meta = workflows::task_meta(&worktree, &task_path);
    let track = meta.track_or_default();
    if track == "implementation" && workflows::running_implementation(&worktree).is_some() {
        return BUSY.into();
    }
    let id = delegation::new_id("wf");
    let session = workflows::session_id(&id);
    let entry = workflows::Entry { workflow_id: id.clone(), task_path: task_path.clone(), track: track.clone(), state: "running".into(), send_backs: 0 };
    if let Err(err) = workflows::write_entry(&worktree, &entry) {
        return format!("FAIL: Failed to initiate session: {err}");
    }
    let file_name = Path::new(&task_path).file_name().and_then(|n| n.to_str()).unwrap_or(&task_path).to_string();
    log_workflow_event(&worktree, json!({ "event": "workflow:queued", "session_id": id, "task": task_path, "track": track }));
    top.emitter.emit(json!({
        "type": "workflow.start",
        "workflow_id": id,
        "session_id": session,
        "title": format!("Workflow Run: {file_name}"),
        "task_path": task_path,
        "track": track,
        "instructions": instructions,
        "prompt": workflows::display_prompt(&task_path, &meta, &track, &instructions),
        "call_id": call.id,
    }));
    format!("SUCCESS: Workflow Runner session started. ID: {session}\nTrack: {track}\nInstructions sent for {task_path}. You will be notified on completion in this session ({manager}).")
}

/// A message from the user lifts the limit on send-backs (see `workflows::max_send_backs`); the host's own
/// `[NomadFlow Notification]` / `[NomadFlow Error]` messages do not.
fn lift_send_back_limit(prompt: &str, worktree: &Path) {
    if !prompt.trim_start().starts_with("[NomadFlow") {
        workflows::reset_all_send_backs(worktree);
    }
}

/// Runs this process as a workflow's Workflow Runner (`NOMAD_WORKFLOW_RUN=1`; the host starts it after a
/// `workflow.start` / `workflow.prompt` event). `NOMAD_WORKFLOW_SESSION` is the workflow's id: a session
/// this process's task store already knows is resumed with `text` as the follow-up, any other starts new
/// from `NOMAD_WORKFLOW_TASK_PATH` with `text` as the instructions. The process's own events are the
/// runner's session (its specialists are tasks under it), and a `workflow.done` event closes it.
async fn run_workflow_session(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    top: Option<&TopLevel<'_>>,
    text: &str,
) -> Result<String, String> {
    let id = env::var("NOMAD_WORKFLOW_SESSION").ok().map(|v| workflows::workflow_id(&v)).filter(|v| !v.is_empty());
    let Some(id) = id else { return Err("NOMAD_WORKFLOW_RUN needs NOMAD_WORKFLOW_SESSION".into()) };
    let (tool, args) = if delegation::load_task(&id).is_some() {
        ("nomadflow_prompt_workflow", json!({ "session_id": id, "text": text }))
    } else {
        let task_path = env::var("NOMAD_WORKFLOW_TASK_PATH").unwrap_or_default();
        ("nomadflow_run_workflow", json!({ "task_path": task_path, "instructions": text }))
    };
    let prepared = prepare_workflow(tool, &args, skills, Some(&id))?;
    let worktree = prepared.worktree.clone();
    let Finished { outcome, filed } = run_prepared(client, cfg, skills, top, 0, &prepared, "", true).await;
    workflows::set_state(&worktree, &id, "idle");
    // The count is not reset here, not even by a `completed` handover: a manager that re-sends a finished
    // workflow after every completion would otherwise never reach the limit (D62). Only the user's message does.
    if let Some(top) = top {
        let handover = filed.as_ref().map(|f| {
            json!({
                "outcome": f.outcome.as_str(), "path": f.display_path, "revision": f.revision,
                "verification": f.verification, "open_issues": f.open_issues, "markdown": f.markdown,
            })
        });
        top.emitter.emit(json!({
            "type": "workflow.done",
            "workflow_id": id,
            "session_id": workflows::session_id(&id),
            "task_path": prepared.task_path,
            "status": match (&outcome, &filed) {
                (Err(_), _) => "failed",
                (Ok(_), Some(f)) if matches!(f.outcome, handover::Outcome::Blocked | handover::Outcome::Failed) => "failed",
                _ => "completed",
            },
            "handover": handover,
            "error": outcome.as_ref().err(),
        }));
    }
    outcome
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
        let policy = permissions::policy_for(&skill.name);
        let fork_scope = RunScope { checkpoint: None, workflow: None, lane: None, owner: None, rules: Some(&policy) };
        let answer = Box::pin(run_agent_loop(
            client,
            &fork_cfg,
            &mut fork_messages,
            skills,
            ToolMode::Base,
            None,
            depth + 1,
            Some(&fork_scope),
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

/// `NOMAD_THINKING` as the `reasoning_effort` every request carries: `off` / `none`, `low`, `medium`, `high`. Unset (or
/// anything else): nothing is sent and the model thinks as it is set up to.
fn thinking_effort(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("off" | "none" | "false" | "0") => Some("none"),
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        _ => None,
    }
}

/// Extra tries for a step that came back with no answer and no tool call (`NOMAD_STALL_RETRIES`, default 2; `0` = none).
fn stall_retries() -> u32 {
    env::var("NOMAD_STALL_RETRIES").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(2).min(5)
}

/// Why a step produced nothing to go on with: no answer text and no tool call.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stall {
    /// The reply limit (or this agent, on a loop) cut the model off while it was still thinking.
    Thinking,
    /// The endpoint sent an empty reply.
    Empty,
}

/// Whether `message` is a stalled step. A reply that ended by itself with only thinking is not one: some models put
/// their answer there (`assistant_text`).
fn stall_of(message: &Value) -> Option<Stall> {
    let has_text = message["content"].as_str().is_some_and(|c| !c.trim().is_empty());
    let has_calls = message["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
    if has_text || has_calls {
        return None;
    }
    let thought = ["reasoning", "reasoning_content"].iter().any(|k| message[*k].as_str().is_some_and(|r| !r.trim().is_empty()));
    match message["finish_reason"].as_str() {
        Some("length") | Some(stream::RUNAWAY) => Some(Stall::Thinking),
        _ if !thought => Some(Stall::Empty),
        _ => None,
    }
}

/// Adds `note` as the user's last word: its own turn after an assistant or tool message, the end of the user's message
/// when that is already last (strict chat templates refuse two user turns in a row).
fn add_user_note(messages: &mut Vec<Value>, note: &str) {
    match messages.last_mut().filter(|m| m["role"] == "user") {
        Some(last) => match &mut last["content"] {
            Value::String(text) => {
                text.push_str("\n\n");
                text.push_str(note);
            }
            Value::Array(parts) => parts.push(json!({ "type": "text", "text": note })),
            other => *other = Value::String(note.to_string()),
        },
        None => messages.push(json!({ "role": "user", "content": note })),
    }
}

/// Said to the model, for one request only, after a stalled step.
const STALL_NUDGE: &str = "Your last reply was cut off before it contained an answer or a tool call. Do not plan any further. \
Reply now with the next tool call, or with your answer if the work is done.";

/// Said once to an agent that can delegate and ended its turn announcing work it did not start.
const DELEGATION_NUDGE: &str = "You ended your turn by saying what you will do, but you made no tool call, so nothing has been started. \
Do it now, in this reply: call Task to delegate (several Task calls in one reply run at the same time) or \
nomadflow_run_workflow to start the workflow. If you are waiting for the user's decision instead, say exactly what you need \
from them and stop.";

/// Whether an answer ends on a promise of work ("I will now delegate…", "3. Initializing") rather than on a result or a
/// question to the user. Deliberately narrow: a missed promise costs nothing new, a false alarm costs one extra step.
fn announces_unstarted_work(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.ends_with('?') {
        return false;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(700)..].iter().collect::<String>().to_lowercase().replace('\u{2019}', "'");
    const PROMISES: [&str; 18] = [
        "i will now", "i'll now", "i will delegate", "i'll delegate", "i am delegating", "i'm delegating", "delegating to",
        "i will task", "i will dispatch", "i will hand", "i will orchestrate", "i will start by", "i will begin", "i will proceed",
        "i will initiate", "next, i will", "initializing", "prepare it for the",
    ];
    PROMISES.iter().any(|p| tail.contains(p))
}

/// `NOMAD_DELEGATION_NUDGE=off` switches the reminder off.
fn delegation_nudge_on() -> bool {
    !matches!(env::var("NOMAD_DELEGATION_NUDGE").ok().as_deref().map(str::trim), Some("off" | "0" | "false"))
}

/// Whether the tools offered to this run include one that hands work on.
fn can_hand_on(tools: Option<&Value>) -> bool {
    tools.and_then(|t| t.as_array()).is_some_and(|t| {
        t.iter().any(|tool| matches!(tool["function"]["name"].as_str(), Some("Task" | "nomadflow_run_workflow")))
    })
}

/// Extra tries for a step whose request the gateway in front of the model turned into a 502/503/504 (`LLM_GATEWAY_RETRIES`, default 2).
fn gateway_retries() -> u32 {
    #[cfg(test)]
    if let Some(n) = TEST_GATEWAY_RETRIES.with(|t| t.get()) {
        return n;
    }
    env::var("LLM_GATEWAY_RETRIES").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(2).min(6)
}

fn gateway_backoff(attempt: u32) -> std::time::Duration {
    #[cfg(test)]
    return std::time::Duration::from_millis(10 * attempt as u64);
    #[cfg(not(test))]
    std::time::Duration::from_secs(3 * attempt as u64)
}

#[cfg(test)]
thread_local! {
    static TEST_GATEWAY_RETRIES: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// A gateway timeout or outage rather than an answer from the model: 502, 503, 504, "Bad Gateway", "upstream request failed".
fn is_gateway_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["502", "503", "504", "bad gateway", "service unavailable", "gateway time", "upstream request failed"].iter().any(|n| lower.contains(n))
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
        // Context sizes, best effort: an endpoint that is not Ollama simply has none to give.
        let wanted = catalog::merge_models(&cfg.model, listed.as_deref().unwrap_or_default(), &skills);
        let contexts = fetch_contexts(&base, &cfg.api_key, &wanted).await;
        let catalog = catalog::build_catalog(&cfg.model, &base, &skills, listed, &contexts);
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

    // The host started this process to be a workflow's Workflow Runner (see `run_workflow_session`).
    if env::var("NOMAD_WORKFLOW_RUN").map(|v| v.trim() == "1").unwrap_or(false) {
        let top = stream_mode.then(|| TopLevel {
            emitter: &emitter,
            base_url: active_base.clone(),
            stream: stream_enabled(),
            protocol2: protocol2_enabled(),
        });
        if stream_mode {
            emitter.emit(json!({
                "type": "meta",
                "model": cfg.model,
                "agent": "workflow_runner",
                "baseUrl": active_base,
                "protocol": if protocol2_enabled() { 2 } else { 1 },
            }));
        }
        return match run_workflow_session(&client, &cfg, &skills, top.as_ref(), &args.prompt.clone().unwrap_or_default()).await {
            Ok(answer) => {
                if stream_mode {
                    emitter.emit(json!({ "type": "final", "text": answer, "model": cfg.model }));
                } else {
                    println!("{answer}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => fail(1, err),
        };
    }

    // The user wrote this message (a notification from a workflow begins `[NomadFlow`): the limit on
    // send-backs is lifted, since they are in the loop again.
    if workflow_async() {
        lift_send_back_limit(args.prompt.as_deref().unwrap_or(""), &workflow_worktree());
    }

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

    // The rules of the agent this run is (and OpenCode's defaults and the config's).
    let policy = permissions::policy_for(initial.agent.as_deref().unwrap_or(""));
    let top_scope = RunScope { checkpoint: None, workflow: None, lane: None, owner: None, rules: Some(&policy) };
    match run_agent_loop(
        &client,
        &run_cfg,
        &mut messages,
        &skills,
        initial.tool_mode,
        top.as_ref(),
        0,
        Some(&top_scope),
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

    /// `NOMAD_CONTROL_DIR` is process-wide: tests that set it, or that rely on it being unset, take turns.
    static CONTROL_DIR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Runs the agent with a model that asks one question, while a "user" answers it through the control directory.
    async fn ask_one_question(reply: Value) -> (Vec<Value>, String) {
        let _turn = CONTROL_DIR_LOCK.lock().await;
        let control = std::env::temp_dir().join(format!("control-q-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&control).unwrap();
        std::env::set_var("NOMAD_CONTROL_DIR", &control);
        let base = scripted_model(|req| {
            let last = last_message(req);
            if last["role"] == "tool" {
                text_message(&format!("Got: {}", last["content"].as_str().unwrap_or("")))
            } else {
                call_message("q1", "question", json!({ "questions": [
                    { "question": "Which format should the export use?", "header": "Format", "options": [
                        { "label": "CSV (Recommended)", "description": "Plain and portable" }, { "label": "JSON", "description": "Typed" } ] },
                    { "question": "Which columns?", "header": "Columns", "multiple": true, "options": [ { "label": "id", "description": "" }, { "label": "name", "description": "" } ] }
                ] }))
            }
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let mut messages = vec![json!({ "role": "system", "content": "you ask" }), json!({ "role": "user", "content": "export it" })];
        let user = {
            let (sink, dir) = (sink.clone(), control.clone());
            async move {
                let id = loop {
                    let found = sink.lock().unwrap().iter().find(|e| e["type"] == "question").and_then(|e| e["id"].as_str().map(str::to_string));
                    if let Some(id) = found { break id };
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                };
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                fs::write(dir.join(format!("answer-{id}.json")), reply.to_string()).unwrap();
                id
            }
        };
        let (answer, id) = tokio::join!(run_agent_loop(&client, &cfg, &mut messages, &[], ToolMode::All, Some(&top), 0, None), user);
        std::env::remove_var("NOMAD_CONTROL_DIR");
        assert!(!control.join(format!("answer-{id}.json")).exists(), "the answer was taken");
        let events = sink.lock().unwrap().clone();
        let _ = fs::remove_dir_all(control);
        (events, answer.map_err(|e| e.to_string()).unwrap())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_question_waits_for_the_users_answer_and_returns_it_in_opencodes_words() {
        let (events, answer) = ask_one_question(json!({ "answers": [["CSV (Recommended)"], ["id", "name"]] })).await;
        let asked = events.iter().find(|e| e["type"] == "question").unwrap();
        assert_eq!(asked["call_id"], "q1");
        assert_eq!(asked["questions"][0]["header"], "Format");
        assert_eq!(asked["questions"][1]["multiple"], true);
        assert!(asked["id"].as_str().unwrap().starts_with("que_"));
        // The card was announced first, the result came after the answer.
        let at = |kind: &str| events.iter().position(|e| e["type"] == kind).unwrap();
        assert!(at("tool") < at("question") && at("question") < at("tool_result"));
        assert_eq!(
            answer,
            "Got: User has answered your questions: \"Which format should the export use?\"=\"CSV (Recommended)\", \"Which columns?\"=\"id, name\". You can now continue with the user's answers in mind."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dismissed_question_is_an_error_the_model_can_read() {
        let (_, answer) = ask_one_question(json!({ "dismissed": true })).await;
        assert_eq!(answer, "Got: Error: The user dismissed this question");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn questions_are_checked_and_the_tool_needs_someone_to_answer() {
        let _turn = CONTROL_DIR_LOCK.lock().await;
        assert!(tools::parse_questions(&json!({})).unwrap_err().contains("requires `questions`"));
        assert!(tools::parse_questions(&json!({ "questions": [{ "header": "x" }] })).unwrap_err().contains("`question` text"));
        let parsed = tools::parse_questions(&json!({ "questions": [{ "question": "Q?", "header": "a very long header that goes past thirty characters", "options": [{ "label": "A" }, "B"] }] })).unwrap();
        assert_eq!(parsed[0]["header"].as_str().unwrap().chars().count(), 30);
        assert_eq!(parsed[0]["options"][1]["label"], "B");
        assert_eq!(tools::answered_text(&parsed, &[vec![]]), "User has answered your questions: \"Q?\"=\"Unanswered\". You can now continue with the user's answers in mind.");
        // Without a control directory nobody can answer: the tool is not offered, and a call is told so.
        std::env::remove_var("NOMAD_CONTROL_DIR");
        let has = |v: &Value| v.as_array().unwrap().iter().any(|t| t["function"]["name"] == "question");
        assert!(!has(&tool_specs(ToolMode::All, &[], 0, false, "m").unwrap()));
    }

    /// One tool call by a scripted model, under the permission rules `agent_file` declares; returns what the model was told
    /// and the events. `answers`: what the user replies to each permission request, in turn (none: nobody can answer).
    async fn call_under_rules(agent_file: &str, calls: Vec<(&str, Value)>, answers: Vec<&str>) -> (Vec<String>, Vec<Value>) {
        let _turn = CONTROL_DIR_LOCK.lock().await;
        permissions::forget_all();
        let control = std::env::temp_dir().join(format!("control-p-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&control).unwrap();
        // Taken down again even when an assertion fails below: the variable is process-wide.
        struct ControlDirGuard;
        impl Drop for ControlDirGuard {
            fn drop(&mut self) {
                std::env::remove_var("NOMAD_CONTROL_DIR");
            }
        }
        let _guard = ControlDirGuard;
        if !answers.is_empty() {
            std::env::set_var("NOMAD_CONTROL_DIR", &control);
        }
        let plan = std::sync::Arc::new(calls.iter().map(|(name, args)| (name.to_string(), args.clone())).collect::<Vec<_>>());
        let step = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base = {
            let (plan, step) = (plan.clone(), step.clone());
            scripted_model(move |_req| {
                let n = step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match plan.get(n) {
                    Some((name, args)) => call_message(&format!("c{n}"), name, args.clone()),
                    None => text_message("done"),
                }
            })
            .await
        };
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let policy = permissions::Policy::defaults().with(permissions::rules_from_front_matter(agent_file));
        let scope = RunScope { checkpoint: None, workflow: None, lane: None, owner: None, rules: Some(&policy) };
        let mut messages = vec![json!({ "role": "user", "content": "go" })];
        let user = {
            let (sink, dir) = (sink.clone(), control.clone());
            async move {
                for (turn, reply) in answers.into_iter().enumerate() {
                    // The turn-th request; answered after a moment, and the answer taken before the next.
                    let id = loop {
                        let found = sink.lock().unwrap().iter().filter(|e| e["type"] == "permission").nth(turn).and_then(|e| e["id"].as_str().map(str::to_string));
                        if let Some(id) = found { break id };
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let file = dir.join(format!("answer-{id}.json"));
                    fs::write(&file, json!({ "reply": reply }).to_string()).unwrap();
                    while file.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                }
            }
        };
        let both = async { tokio::join!(run_agent_loop(&client, &cfg, &mut messages, &[], ToolMode::All, Some(&top), 0, Some(&scope)), user) };
        tokio::time::timeout(std::time::Duration::from_secs(20), both).await.expect("the run did not finish (a request nobody answered?)");
        std::env::remove_var("NOMAD_CONTROL_DIR");
        permissions::forget_all();
        let told: Vec<String> = messages.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_string()).collect();
        let events = sink.lock().unwrap().clone();
        let _ = fs::remove_dir_all(control);
        (told, events)
    }

    const REVIEWER: &str = "---\nname: reviewer\npermission:\n  bash:\n    \"*\": ask\n    \"echo ok*\": allow\n  edit:\n    \"**/*.key\": deny\n---\n";

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rule_that_denies_a_call_keeps_it_from_running() {
        let wt = new_worktree("perm-deny");
        let secret = wt.join("prod.key");
        let (told, events) = call_under_rules(REVIEWER, vec![("Write", json!({ "file_path": secret.display().to_string(), "content": "x" }))], vec![]).await;
        assert!(told[0].starts_with("The user has specified a rule which prevents you from using this specific tool call."), "{}", told[0]);
        assert!(!secret.exists(), "nothing was written");
        assert!(events.iter().all(|e| e["type"] != "permission"), "a deny asks nobody");
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ask_waits_for_the_user_once_always_or_reject() {
        // Allowed by a pattern: runs at once, no question.
        let (told, events) = call_under_rules(REVIEWER, vec![("Bash", json!({ "command": "echo ok-fine" }))], vec![]).await;
        assert!(told[0].contains("ok-fine") && events.iter().all(|e| e["type"] != "permission"));

        // Asked, allowed once: runs; the same command is asked again.
        let call = ("Bash", json!({ "command": "printf asked-once" }));
        let (told, events) = call_under_rules(REVIEWER, vec![call.clone(), call.clone()], vec!["once", "once"]).await;
        assert_eq!(events.iter().filter(|e| e["type"] == "permission").count(), 2);
        assert!(told.iter().all(|t| t.contains("asked-once")), "{told:?}");
        let asked = events.iter().find(|e| e["type"] == "permission").unwrap();
        assert_eq!((asked["permission"].as_str(), asked["call_id"].as_str()), (Some("bash"), Some("c0")));
        assert_eq!(asked["patterns"], json!(["printf asked-once"]));
        assert_eq!(asked["always"], json!(["printf *"]));
        assert!(asked["id"].as_str().unwrap().starts_with("per_"));

        // Always: asked the first time only, and a different command of the same kind is covered too.
        let (told, events) = call_under_rules(REVIEWER, vec![call.clone(), ("Bash", json!({ "command": "printf second" }))], vec!["always"]).await;
        assert_eq!(events.iter().filter(|e| e["type"] == "permission").count(), 1);
        assert!(told[0].contains("asked-once") && told[1].contains("second"), "{told:?}");

        // Rejected: it does not run, and the model is told so.
        let marker = std::env::temp_dir().join(format!("perm-marker-{}", delegation::new_task_id()));
        let (told, _) = call_under_rules(REVIEWER, vec![("Bash", json!({ "command": format!("touch {}", marker.display()) }))], vec!["reject"]).await;
        assert!(told[0].starts_with("The user rejected permission to use this specific tool call"), "{}", told[0]);
        assert!(!marker.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_nobody_to_ask_an_ask_is_refused() {
        let (told, events) = call_under_rules(REVIEWER, vec![("Bash", json!({ "command": "rm -rf /tmp/never" }))], vec![]).await;
        assert!(told[0].starts_with("Error: this tool call needs the user's permission (bash rm -rf /tmp/never) and nobody is here to give it"), "{}", told[0]);
        assert!(events.iter().all(|e| e["type"] != "permission"));
    }

    #[test]
    fn a_runner_is_told_what_it_can_mend_itself() {
        let mut h = handover::Handover::from_args(&valid_handover("x")).unwrap();
        assert!(self_correction(&h, Some("FAIL: x".into())).is_none(), "completed needs nothing");
        h.outcome = handover::Outcome::Partial;
        h.open_issues = vec!["something".into()];
        // A failed validation: the work is not done.
        let told = self_correction(&h, Some("FAIL: docs/specifications/EXPORT.md has fewer than 40 words".into())).unwrap();
        assert!(told.contains("Your handover is partial") && told.contains("fewer than 40 words") && told.contains("describing a change does not make it"), "{told}");
        // Requirements met and verified, steps left pending.
        h.plan.push(handover::PlanItem { id: "P9".into(), title: "Build".into(), owner: "".into(), status: "pending".into(), result: "".into(), depends_on: vec![] });
        let told = self_correction(&h, None).unwrap();
        assert!(told.contains("still lists these steps as pending: P9") && told.contains("`skipped`"), "{told}");
        // Not this: an unmet requirement, or an unverified result, is real unfinished work for the manager to judge.
        h.requirements[0].status = "unmet".into();
        assert!(self_correction(&h, None).is_none());
        h.requirements[0].status = "met".into();
        h.verification = "unverified".into();
        assert!(self_correction(&h, None).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_whose_only_shortfall_is_pending_steps_is_sent_back_by_the_engine() {
        let wt = new_worktree("self-correct");
        let requests = Arc::new(AtomicUsize::new(0));
        let run = flow(&wt, Runner::PendingPlan, run_call(json!({ "task_path": TASK, "instructions": "do it" })), requests.clone()).await;
        assert_eq!(run.answer.as_deref(), Ok("PMA DONE"));
        // The manager got the handover from after the correction: completed, not the first, partial one.
        let result = workflow_result(&run);
        assert!(result.contains("completed"), "{result}");
        let ends: Vec<&Value> = run.events.iter().filter(|e| e["type"] == "task.end").collect();
        assert_eq!(ends.last().unwrap()["handover"]["outcome"], "completed");
        assert_eq!(ends.last().unwrap()["handover"]["revision"], 2, "two filings, one workflow");
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gpt_models_get_apply_patch_and_it_edits_files() {
        let names = |model: &str| -> Vec<String> {
            tool_specs(ToolMode::All, &[], 0, false, model).unwrap().as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect()
        };
        let (gpt, other) = (names("gpt-5"), names("qwen2.5:14b"));
        assert!(gpt.contains(&"apply_patch".to_string()) && !gpt.contains(&"Edit".to_string()) && !gpt.contains(&"Write".to_string()));
        assert!(!other.contains(&"apply_patch".to_string()) && other.contains(&"Edit".to_string()) && other.contains(&"Write".to_string()));

        // A model that calls it: the file changes and the model is told what was updated.
        let wt = new_worktree("apply-patch");
        let file = wt.join("notes.txt");
        fs::write(&file, "one\ntwo\n").unwrap();
        let patch = format!("*** Begin Patch\n*** Update File: {}\n-two\n+2\n*** Add File: {}\n+new\n*** End Patch", file.display(), wt.join("added.txt").display());
        let base = scripted_model(move |req| {
            let last = last_message(req);
            if last["role"] == "tool" { text_message(last["content"].as_str().unwrap_or("")) } else { call_message("p1", "apply_patch", json!({ "patchText": patch })) }
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "gpt-5".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let mut messages = vec![json!({ "role": "user", "content": "patch it" })];
        let answer = run_agent_loop(&client, &cfg, &mut messages, &[], ToolMode::All, None, 0, None).await.map_err(|e| e.to_string()).unwrap();
        assert!(answer.starts_with("Success. Updated the following files:\nM "), "{answer}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "one\n2\n");
        assert_eq!(fs::read_to_string(wt.join("added.txt")).unwrap(), "new\n");
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_one_task_leaves_the_others_running() {
        let _turn = CONTROL_DIR_LOCK.lock().await;
        let control = std::env::temp_dir().join(format!("control-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&control).unwrap();
        std::env::set_var("NOMAD_CONTROL_DIR", &control);
        let base = mock_model(1500, false).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("worker", true)];
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "plan the work" })];
        let started = std::time::Instant::now();

        // The user presses Stop on the card of call_2 once its task has started.
        let stopper_sink = sink.clone();
        let stopper_dir = control.clone();
        let stopper = async move {
            let task_id = loop {
                let found = stopper_sink.lock().unwrap().iter().find(|e| e["type"] == "task.start" && e["parent_call_id"] == "call_2").and_then(|e| e["task_id"].as_str().map(str::to_string));
                if let Some(id) = found { break id };
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            };
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            fs::write(stopper_dir.join(format!("stop-{task_id}")), "").unwrap();
            task_id
        };
        let (answer, stopped_id) = tokio::join!(run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, Some(&top), 0, None), stopper);
        std::env::remove_var("NOMAD_CONTROL_DIR");
        assert_eq!(answer.map_err(|e| e.to_string()).as_deref(), Ok("ALL DONE"), "the parent carries on");

        let events = sink.lock().unwrap().clone();
        let end_of = |call: &str| {
            let task = events.iter().find(|e| e["type"] == "task.start" && e["parent_call_id"] == call).unwrap()["task_id"].clone();
            events.iter().position(|e| e["type"] == "task.end" && e["task_id"] == task).map(|i| (i, events[i].clone()))
        };
        let (stopped_at, stopped) = end_of("call_2").unwrap();
        assert_eq!(stopped["status"], "aborted");
        assert_eq!(stopped["task_id"], stopped_id.as_str());
        for call in ["call_1", "call_3"] {
            let (at, end) = end_of(call).unwrap();
            assert_eq!(end["status"], "completed", "{call} ran to its answer");
            assert!(stopped_at < at, "the stopped task ended first");
        }
        // The caller is told, and can resume it; the other two answered normally.
        let results: Vec<String> = messages.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap().to_string()).collect();
        assert!(results[0].contains("answer-1") && results[2].contains("answer-3"));
        assert!(results[1].starts_with("The user stopped subagent worker before it finished") && results[1].contains(&format!("task_id: {stopped_id}")), "{}", results[1]);
        // The request was taken (a resumed task must not stop at once), and the saved work can be picked up again.
        assert!(!control.join(format!("stop-{stopped_id}")).exists());
        assert!(delegation::load_task(&stopped_id).is_some(), "progress is kept so the task can be resumed");
        assert!(started.elapsed().as_millis() < 2800, "the stopped task did not hold the batch up: {:?}", started.elapsed());
        let _ = fs::remove_dir_all(control);
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
                    // A scripted reply may say how it ended (`finish_reason` on the message); `stop` otherwise.
                    let finish = message["finish_reason"].as_str().unwrap_or("stop").to_string();
                    let body = json!({ "id": "x", "object": "chat.completion", "created": 0, "model": "m",
                        "choices": [{ "index": 0, "message": message, "finish_reason": finish }] }).to_string();
                    let out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        base
    }

    /// A reply the limit cut off while the model was still thinking: no answer, no tool call.
    fn cut_off_thinking() -> Value {
        json!({ "role": "assistant", "content": null, "reasoning": "First I list the docs.\n`ls docs/scrs`\n`ls docs/features`", "finish_reason": "length" })
    }

    async fn hardening_run(base: &str, skills: &[Skill]) -> (Result<String, String>, Vec<Value>) {
        let cfg = LlmConfig { base_url: base.into(), fallback_base_url: base.into(), api_key: "k".into(), model: "thinker:26b".into(), max_tokens: 256 };
        let client = make_client(base, "k");
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "improve the attachment dialog" })];
        let answer = run_agent_loop(&client, &cfg, &mut messages, skills, ToolMode::All, None, 0, None).await.map_err(|e| e.to_string());
        (answer, messages)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_step_lost_to_thinking_is_tried_again_without_thinking_and_the_run_goes_on() {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let base = scripted_model(move |req| {
            let mut seen = log.lock().unwrap();
            seen.push(req.clone());
            match seen.len() {
                1 => cut_off_thinking(),
                2 => call_message("r1", "Read", json!({ "file_path": "/etc/hostname" })),
                _ => text_message("The dialog is directory-browser-dialog.tsx."),
            }
        }).await;
        let (answer, messages) = hardening_run(&base, &[]).await;
        assert_eq!(answer.as_deref(), Ok("The dialog is directory-browser-dialog.tsx."));
        let seen = seen.lock().unwrap();
        assert!(seen[0].get("reasoning_effort").is_none(), "the model thinks as it is set up to until it stalls");
        assert_eq!(seen[1]["reasoning_effort"], "none", "the retry is sent with thinking off");
        assert_eq!(last_message(&seen[1])["content"], format!("improve the attachment dialog\n\n{STALL_NUDGE}").as_str(), "and is told to act");
        assert!(seen[2].get("reasoning_effort").is_none(), "only the retry: the next step thinks again");
        let said: Vec<String> = messages.iter().map(|m| m["content"].as_str().unwrap_or("").to_string()).collect();
        assert!(said.iter().all(|t| t != STALL_NUDGE && !t.contains("ls docs")), "neither the nudge nor the thinking enters the conversation: {said:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_that_only_ever_thinks_ends_the_run_with_an_error_that_names_it() {
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let base = scripted_model(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            cut_off_thinking()
        }).await;
        let (answer, _) = hardening_run(&base, &[]).await;
        let err = answer.unwrap_err();
        assert!(err.contains("thinker:26b") && err.contains("spent its whole reply thinking") && err.contains("NOMAD_THINKING"), "{err}");
        assert!(!err.contains("ls docs"), "the thinking is not passed off as the answer: {err}");
        assert_eq!(requests.load(Ordering::SeqCst), 3, "the step and two retries");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_reply_is_tried_again_and_thinking_that_ended_by_itself_is_still_an_answer() {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let base = scripted_model(move |req| {
            let mut seen = log.lock().unwrap();
            seen.push(req.clone());
            if seen.len() == 1 { json!({ "role": "assistant", "content": "" }) } else { text_message("DONE") }
        }).await;
        let (answer, _) = hardening_run(&base, &[]).await;
        assert_eq!(answer.as_deref(), Ok("DONE"));
        assert!(seen.lock().unwrap()[1].get("reasoning_effort").is_none(), "nothing to switch off: it was not thinking");

        // Some models put the answer in `reasoning` and stop by themselves: that is kept as it was.
        let base = scripted_model(|_| json!({ "role": "assistant", "content": "", "reasoning": "working it out\n42" })).await;
        let (answer, _) = hardening_run(&base, &[]).await;
        assert_eq!(answer.as_deref(), Ok("42"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_manager_that_stops_on_a_promise_to_delegate_is_reminded_once_and_then_delegates() {
        let nudges = Arc::new(AtomicUsize::new(0));
        let count = nudges.clone();
        let base = scripted_model(move |req| {
            let system = system_text(req);
            let last = last_message(req);
            let content = last["content"].as_str().unwrap_or("").to_string();
            if system.contains("agent `developer`") {
                return text_message("found it");
            }
            if content == DELEGATION_NUDGE {
                count.fetch_add(1, Ordering::SeqCst);
                return call_message("t1", "Task", json!({ "subagent_type": "developer", "description": "find dialog", "prompt": "find the attachment dialog" }));
            }
            if last["role"] == "tool" { text_message("The developer found it. I will now review the result.") } else {
                text_message("1. Task Classification\n2. Execution Strategy: I will delegate a task to a developer.\n3. Initializing")
            }
        }).await;
        let skills = vec![skill("developer", true)];
        let (answer, messages) = hardening_run(&base, &skills).await;
        assert_eq!(answer.as_deref(), Ok("The developer found it. I will now review the result."), "after delegating it is not reminded again");
        assert_eq!(nudges.load(Ordering::SeqCst), 1);
        assert!(messages.iter().any(|m| m["role"] == "tool" && m["content"].as_str().is_some_and(|c| c.contains("found it"))), "the task really ran");

        // A model that ignores the reminder is not asked twice, and an agent with nobody to delegate to is not asked at all.
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let base = scripted_model(move |req| {
            if last_message(req)["content"] == DELEGATION_NUDGE {
                count.fetch_add(1, Ordering::SeqCst);
            }
            text_message("I will now delegate this to the developer.")
        }).await;
        let (answer, _) = hardening_run(&base, &skills).await;
        assert_eq!(answer.as_deref(), Ok("I will now delegate this to the developer."));
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // An agent with no tool that hands work on has nothing to be reminded of.
        assert!(can_hand_on(Some(&json!([{ "function": { "name": "Read" } }, { "function": { "name": "Task" } }]))));
        assert!(!can_hand_on(Some(&json!([{ "function": { "name": "Read" } }]))));
        assert!(!can_hand_on(None));
    }

    #[test]
    fn promises_of_work_are_told_from_results_and_questions() {
        assert!(announces_unstarted_work("3. Initializing"));
        assert!(announces_unstarted_work("I\u{2019}ll delegate the investigation to the developer."));
        assert!(announces_unstarted_work("The task file is written. I will now move it to Active and prepare it for the Technical Architect to review."));
        assert!(!announces_unstarted_work("The dialog is `directory-browser-dialog.tsx`; it sorts by name only."));
        assert!(!announces_unstarted_work("I will delegate this to the developer. Shall I proceed?"), "it is waiting for the user");
        assert!(!announces_unstarted_work(""));
        let long = format!("I will now delegate.\n{}", "The result is in the table above. ".repeat(40));
        assert!(!announces_unstarted_work(&long), "only how the answer ends counts");
    }

    #[test]
    fn the_thinking_setting_and_what_counts_as_a_stall() {
        assert_eq!(thinking_effort(None), None);
        assert_eq!(thinking_effort(Some("off")), Some("none"));
        assert_eq!(thinking_effort(Some(" Low ")), Some("low"));
        assert_eq!(thinking_effort(Some("maximum")), None, "an unknown value sends nothing");
        assert_eq!(stall_of(&cut_off_thinking()), Some(Stall::Thinking));
        assert_eq!(stall_of(&json!({ "content": null, "reasoning": "round and round", "finish_reason": stream::RUNAWAY })), Some(Stall::Thinking));
        assert_eq!(stall_of(&json!({ "content": "", "finish_reason": "stop" })), Some(Stall::Empty));
        assert_eq!(stall_of(&json!({ "content": "", "reasoning": "42", "finish_reason": "stop" })), None);
        assert_eq!(stall_of(&json!({ "content": "half an ans", "finish_reason": "length" })), None, "a cut-off answer is still text");
        assert_eq!(stall_of(&call_message("c", "Read", json!({}))), None);

        // The nudge is its own turn after a tool result, and the end of the user's message when that is last.
        let mut after_tool = vec![json!({ "role": "tool", "content": "x" })];
        add_user_note(&mut after_tool, "N");
        assert_eq!(after_tool[1], json!({ "role": "user", "content": "N" }));
        let mut after_user = vec![json!({ "role": "user", "content": [{ "type": "text", "text": "look" }] })];
        add_user_note(&mut after_user, "N");
        assert_eq!(after_user.len(), 1);
        assert_eq!(after_user[0]["content"][1]["text"], "N");
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
    enum Runner { Happy, RejectedFirst, NeverFiles, Failing, PendingPlan }

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
                    // Files a partial handover whose only shortfall is steps left pending; once told so, files it completed.
                    (_, Runner::PendingPlan) if content.contains("still lists these steps as pending") => call_message("h4", "handover", valid_handover("steps settled")),
                    (_, Runner::PendingPlan) => {
                        let mut early = valid_handover("spec done, build steps not started");
                        early["outcome"] = json!("partial");
                        early["open_issues"] = json!(["P3 not started"]);
                        early["plan"].as_array_mut().unwrap().push(json!({ "id": "P3", "title": "Build the feature", "owner": "developer", "status": "pending" }));
                        call_message("h1", "handover", early)
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

    // ------------------------------------------------------------ a flaky gateway in front of the model

    /// An endpoint that answers `status` (with `body`) to its first `failures` requests, then a plain answer. Counts requests.
    async fn flaky_endpoint(failures: usize, status: &'static str, body: &'static str) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let mut got = 0;
                    loop {
                        let n = sock.read(&mut buf[got..]).await.unwrap_or(0);
                        if n == 0 { return; }
                        got += n;
                        if let Some(pos) = buf[..got].windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            let want: usize = head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                            if got >= pos + 4 + want { break; }
                        }
                    }
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let (code, payload) = if n < failures {
                        (status, body.to_string())
                    } else {
                        ("200 OK", json!({ "id": "x", "object": "chat.completion", "created": 0, "model": "m",
                            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ANSWER" }, "finish_reason": "stop" }] }).to_string())
                    };
                    let out = format!("HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    async fn one_step(base: &str) -> Result<String, String> {
        let cfg = LlmConfig { base_url: base.into(), fallback_base_url: base.into(), api_key: "k".into(), model: "m".into(), max_tokens: 64 };
        // The HTTP library's own retries are off here, so that what is counted is this step's.
        let no_library_retries = backoff::ExponentialBackoffBuilder::new().with_max_elapsed_time(Some(std::time::Duration::ZERO)).build();
        let client = Client::with_config(OpenAIConfig::new().with_api_base(base).with_api_key("k")).with_backoff(no_library_retries);
        let mut messages = vec![json!({ "role": "user", "content": "hi" })];
        run_agent_loop(&client, &cfg, &mut messages, &[], ToolMode::None, None, 0, None).await.map_err(|e| e.to_string())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_gateway_timeout_is_tried_again_and_the_run_goes_on() {
        TEST_GATEWAY_RETRIES.with(|t| t.set(Some(2)));
        let (base, seen) = flaky_endpoint(2, "502 Bad Gateway", "upstream request failed").await;
        let answer = one_step(&base).await;
        TEST_GATEWAY_RETRIES.with(|t| t.set(None));
        assert_eq!(answer.as_deref(), Ok("ANSWER"));
        assert_eq!(seen.load(Ordering::SeqCst), 3, "two gateway errors, then the answer");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_gateway_that_keeps_failing_ends_the_run_after_the_retries() {
        TEST_GATEWAY_RETRIES.with(|t| t.set(Some(2)));
        let (base, seen) = flaky_endpoint(100, "504 Gateway Timeout", "gateway time-out").await;
        let answer = one_step(&base).await;
        TEST_GATEWAY_RETRIES.with(|t| t.set(None));
        assert!(answer.is_err());
        assert_eq!(seen.load(Ordering::SeqCst), 3, "the first try and two more, no more");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_from_the_model_is_not_retried() {
        TEST_GATEWAY_RETRIES.with(|t| t.set(Some(2)));
        let (base, seen) = flaky_endpoint(100, "400 Bad Request", r#"{"error":{"message":"bad request"}}"#).await;
        let answer = one_step(&base).await;
        TEST_GATEWAY_RETRIES.with(|t| t.set(None));
        assert!(answer.is_err());
        assert_eq!(seen.load(Ordering::SeqCst), 1, "an answer from the model, even a refusal, is final");
    }

    #[test]
    fn gateway_errors_are_told_from_the_models_own() {
        assert!(is_gateway_error("502 Bad Gateway upstream request failed"));
        assert!(is_gateway_error("HTTP 504 Gateway Timeout"));
        assert!(is_gateway_error("503 Service Unavailable"));
        assert!(!is_gateway_error("400 Bad Request: invalid tool schema"));
        assert!(!is_gateway_error("429 Too Many Requests"));
    }

    // ------------------------------------------------------------ asynchronous workflows (OpenCode's flow)

    /// Switches the background mode on for this test's thread and back off after.
    struct AsyncMode;
    impl AsyncMode {
        fn on() -> AsyncMode {
            TEST_ASYNC.with(|t| t.set(Some(true)));
            AsyncMode
        }
    }
    impl Drop for AsyncMode {
        fn drop(&mut self) {
            TEST_ASYNC.with(|t| t.set(None));
        }
    }

    fn event_of<'a>(run: &'a Run, kind: &str) -> Option<&'a Value> {
        run.events.iter().find(|e| e["type"] == kind)
    }

    fn add_task(wt: &Path, name: &str, front_matter: &str) -> String {
        let dir = wt.join(format!("tasks/todo/{name}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.md")), format!("{front_matter}# {name}\n\n# Post Implementation Task Updates\n")).unwrap();
        format!("tasks/todo/{name}/{name}.md")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_the_background_a_workflow_starts_and_returns_at_once() {
        let _mode = AsyncMode::on();
        let wt = new_worktree("bg-start");
        let task = add_task(&wt, "TASK-BG", "---\nstatus: active\ncomplexity: complex\ntrack: implementation\nslice: logic\n---\n");
        let requests = Arc::new(AtomicUsize::new(0));
        let run = flow(&wt, Runner::Happy, run_call(json!({ "task_path": task, "instructions": "run the lifecycle" })), requests.clone()).await;
        assert_eq!(run.answer.as_deref(), Ok("PMA DONE"));
        assert_eq!(requests.load(Ordering::SeqCst), 0, "the manager's process does not run the runner");

        // The tool's answer is NomadWorks' plugin's, word for word.
        let result = workflow_result(&run);
        let session = result.strip_prefix("SUCCESS: Workflow Runner session started. ID: ").and_then(|r| r.split('\n').next()).expect(&result).to_string();
        assert!(session.starts_with("ses_lcl_wf_"), "{session}");
        assert_eq!(
            result,
            format!("SUCCESS: Workflow Runner session started. ID: {session}\nTrack: implementation\nInstructions sent for {task}. You will be notified on completion in this session (this session).")
        );

        // The host is told to run it.
        let start = event_of(&run, "workflow.start").expect("workflow.start");
        assert_eq!(start["session_id"], session.as_str());
        assert_eq!(start["workflow_id"], session.trim_start_matches("ses_lcl_"));
        assert_eq!(start["title"], "Workflow Run: TASK-BG.md");
        assert_eq!(start["track"], "implementation");
        assert_eq!(start["instructions"], "run the lifecycle");
        assert_eq!(start["call_id"], "wf1");
        let prompt = start["prompt"].as_str().unwrap();
        assert!(prompt.starts_with(&format!("Task File: {task}\n\nComplexity: complex\nTrack: implementation\nSlice: logic\nStatus: active\n\nInstructions: run the lifecycle")), "{prompt}");
        assert!(event_of(&run, "task.start").is_none(), "no task: the runner is a session of its own");

        // And it is on record as running.
        let entry = workflows::find(&wt, session.trim_start_matches("ses_lcl_")).unwrap();
        assert!(entry.running() && entry.task_path == task);
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_one_implementation_workflow_runs_at_a_time() {
        let _mode = AsyncMode::on();
        let wt = new_worktree("bg-guard");
        let spec = add_task(&wt, "TASK-SPEC", "---\ntrack: spec\n---\n");
        let first = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "go" })), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&first).starts_with("SUCCESS: Workflow Runner session started."));

        let second = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "go again" })), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(
            workflow_result(&second),
            "FAIL: A shared-worktree implementation workflow is already running. You may continue investigation or spec work separately, but only one implementation workflow can own the shared worktree at a time."
        );
        assert!(event_of(&second, "workflow.start").is_none());
        assert_eq!(second.answer.as_deref(), Ok("PMA DONE"), "a refusal is a result, not the end of the run");

        // Spec and investigation work may run alongside.
        let third = flow(&wt, Runner::Happy, run_call(json!({ "task_path": spec, "instructions": "write it up" })), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&third).contains("Track: spec"), "{}", workflow_result(&third));

        // Once the implementation workflow's runner is done, the next one may start.
        let id = event_of(&first, "workflow.start").unwrap()["workflow_id"].as_str().unwrap().to_string();
        workflows::set_state(&wt, &id, "idle");
        let fourth = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "go again" })), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&fourth).starts_with("SUCCESS: Workflow Runner session started."));
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mini_team_has_no_workflow_runner() {
        let _mode = AsyncMode::on();
        let wt = new_worktree("bg-mini");
        fs::create_dir_all(wt.join(".codenomad")).unwrap();
        fs::write(wt.join(".codenomad/nomadworks.yaml"), "enabled: true\nteam_mode: mini\n").unwrap();
        let run = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "go" })), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(workflow_result(&run), "FAIL: Workflow Runner is unavailable in the current team configuration. Switch to full team mode to run complex workflows.");
        let prompt = flow(&wt, Runner::Happy, json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": "ses_lcl_wf_x", "text": "hi" } }), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&prompt).ends_with("Switch to full team mode to send workflow runner prompts."));
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prompting_a_background_workflow_queues_or_resumes_it() {
        let _mode = AsyncMode::on();
        let wt = new_worktree("bg-prompt");
        let first = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "go" })), Arc::new(AtomicUsize::new(0))).await;
        let start = event_of(&first, "workflow.start").unwrap();
        let (session, id) = (start["session_id"].as_str().unwrap().to_string(), start["workflow_id"].as_str().unwrap().to_string());
        let prompt = |text: &str| json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": session, "text": text } });

        // Still running: the text is queued behind the runner's current run, and its own notification is promised.
        let queued = flow(&wt, Runner::Happy, prompt("also check the CSV header"), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(workflow_result(&queued), format!("SUCCESS: Prompt sent to active session '{session}'. Instructions added to queue. You will be notified on completion in this session (this session)."));
        let ev = event_of(&queued, "workflow.prompt").unwrap();
        assert_eq!((ev["active"].clone(), ev["text"].as_str()), (json!(true), Some("also check the CSV header")));

        // Finished (idle): it is resumed, and the manager is promised a notification.
        workflows::set_state(&wt, &id, "idle");
        let resumed = flow(&wt, Runner::Happy, prompt("the header is wrong"), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(
            workflow_result(&resumed),
            format!("SUCCESS: Session '{session}' was not tracked. Sent prompt and resumed monitoring. You will be notified on completion in this session (this session).")
        );
        let ev = event_of(&resumed, "workflow.prompt").unwrap();
        assert_eq!((ev["active"].clone(), ev["session_id"].as_str()), (json!(false), Some(session.as_str())));
        assert!(workflows::find(&wt, &id).unwrap().running(), "running again");

        // A session nobody started is an error, and so are missing arguments.
        let unknown = flow(&wt, Runner::Happy, json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": "ses_lcl_wf_nope", "text": "hi" } }), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&unknown).starts_with("Error: unknown workflow session `ses_lcl_wf_nope`"), "{}", workflow_result(&unknown));
        let missing = flow(&wt, Runner::Happy, json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": session } }), Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(workflow_result(&missing), "Error: nomadflow_prompt_workflow requires session_id and text.");
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_workflow_sent_back_too_often_is_left_to_the_user() {
        let _mode = AsyncMode::on();
        let wt = new_worktree("bg-limit");
        let first = flow(&wt, Runner::Happy, run_call(json!({ "task_path": TASK, "instructions": "go" })), Arc::new(AtomicUsize::new(0))).await;
        let start = event_of(&first, "workflow.start").unwrap();
        let (session, id) = (start["session_id"].as_str().unwrap().to_string(), start["workflow_id"].as_str().unwrap().to_string());
        let prompt = |n: usize| json!({ "name": "nomadflow_prompt_workflow", "args": { "session_id": session, "text": format!("try {n}") } });
        workflows::set_state(&wt, &id, "idle");
        for n in 1..=3 {
            let sent = flow(&wt, Runner::Happy, prompt(n), Arc::new(AtomicUsize::new(0))).await;
            assert!(workflow_result(&sent).starts_with("SUCCESS"), "send-back {n}: {}", workflow_result(&sent));
            workflows::set_state(&wt, &id, "idle");
        }
        assert_eq!(workflows::find(&wt, &id).unwrap().send_backs, 3);

        // The fourth is refused, nothing is sent, and the manager is told to ask the user.
        let refused = flow(&wt, Runner::Happy, prompt(4), Arc::new(AtomicUsize::new(0))).await;
        let text = workflow_result(&refused);
        assert!(text.starts_with("FAIL: Workflow '") && text.contains("sent back 3 times since the user last wrote (limit 3)") && text.contains("ask how to proceed"), "{text}");
        assert!(event_of(&refused, "workflow.prompt").is_none(), "nothing was sent to the runner");
        assert!(!workflows::find(&wt, &id).unwrap().running());
        assert_eq!(workflows::find(&wt, &id).unwrap().send_backs, 3, "a refusal is not a send-back");

        // The user speaks: the limit is lifted.
        workflows::reset_all_send_backs(&wt);
        let again = flow(&wt, Runner::Happy, prompt(5), Arc::new(AtomicUsize::new(0))).await;
        assert!(workflow_result(&again).starts_with("SUCCESS"), "{}", workflow_result(&again));
        assert_eq!(workflows::find(&wt, &id).unwrap().send_backs, 1);
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn only_the_users_message_lifts_the_send_back_limit() {
        let wt = new_worktree("bg-lift");
        workflows::write_entry(&wt, &workflows::Entry { workflow_id: "wf_l".into(), task_path: TASK.into(), track: "spec".into(), state: "idle".into(), send_backs: 3 }).unwrap();
        lift_send_back_limit("[NomadFlow Notification] Workflow Runner has finished work for: t.md.", &wt);
        lift_send_back_limit("[NomadFlow Error] Workflow Runner failed.", &wt);
        assert_eq!(workflows::find(&wt, "wf_l").unwrap().send_backs, 3);
        lift_send_back_limit("Please try once more.", &wt);
        assert_eq!(workflows::find(&wt, "wf_l").unwrap().send_backs, 0);
        let _ = fs::remove_dir_all(wt);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_nested_caller_still_waits_for_its_workflow() {
        // Only the Product Manager's own call runs in the background; a workflow runner delegating to another does not.
        let _mode = AsyncMode::on();
        let wt = new_worktree("bg-nested");
        let base = scripted_model(|req| {
            let system = system_text(req);
            let last = last_message(req);
            let content = last["content"].as_str().unwrap_or("").to_string();
            if system.contains("workflow_runner") {
                return match last["role"].as_str().unwrap_or("") {
                    "tool" if content.contains("Handover filed") => text_message("closing note"),
                    _ => call_message("h1", "handover", valid_handover("export shipped")),
                };
            }
            if req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool")) { text_message("DONE") } else {
                call_message("w1", "nomadflow_run_workflow", json!({ "task_path": TASK, "instructions": "go" }))
            }
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("workflow_runner", true)];
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let mut messages = vec![json!({ "role": "user", "content": "go" })];
        // depth 1: as a subagent of the manager would call it.
        let answer = run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, Some(&top), 1, None).await.map_err(|e| e.to_string());
        assert_eq!(answer.as_deref(), Ok("DONE"));
        let result = messages.iter().find(|m| m["role"] == "tool").and_then(|m| m["content"].as_str()).unwrap_or_default().to_string();
        assert!(result.starts_with("SUCCESS: Workflow completed — handover filed"), "{result}");
        assert!(sink.lock().unwrap().iter().all(|e| e["type"] != "workflow.start"));
        let _ = fs::remove_dir_all(wt);
    }

    /// The host's second process: this one is the Workflow Runner, and its events are the runner session's own.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_runner_process_files_a_handover_and_reports_it_as_its_own_session() {
        let wt = new_worktree("runner-process");
        let base = scripted_model(|req| {
            let last = last_message(req);
            let content = last["content"].as_str().unwrap_or("").to_string();
            match last["role"].as_str().unwrap_or("") {
                "tool" if content.contains("Handover filed") => text_message("closing note"),
                _ if content.contains("sent the workflow back") => call_message("h2", "handover", valid_handover("fixed the header")),
                _ => call_message("h1", "handover", valid_handover("export shipped")),
            }
        }).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("workflow_runner", true)];
        let id = delegation::new_id("wf");
        workflows::write_entry(&wt, &workflows::Entry { workflow_id: id.clone(), task_path: TASK.into(), track: "implementation".into(), state: "running".into(), send_backs: 2 }).unwrap();
        std::env::set_var("NOMAD_WORKFLOW_SESSION", workflows::session_id(&id));
        std::env::set_var("NOMAD_WORKFLOW_TASK_PATH", TASK);

        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let answer = run_workflow_session(&client, &cfg, &skills, Some(&top), "run the lifecycle").await;
        assert_eq!(answer.as_deref(), Ok("closing note"));
        let events = sink.lock().unwrap().clone();
        assert!(events.iter().all(|e| e["type"] != "task.start" && e["type"] != "task.end"), "the process is the session, not a task");
        assert!(events.iter().all(|e| e.get("task_id").is_none()), "its own events are not stamped as a task's");
        assert!(events.iter().any(|e| e["type"] == "tool" && e["name"] == "handover"), "its tool calls stream as the session's own");
        let done = events.iter().find(|e| e["type"] == "workflow.done").expect("workflow.done");
        assert_eq!(done["status"], "completed");
        assert_eq!(done["session_id"], workflows::session_id(&id).as_str());
        assert_eq!(done["handover"]["outcome"], "completed");
        assert_eq!(done["handover"]["revision"], 1);
        assert_eq!(done["handover"]["path"], "tasks/todo/TASK-9/HANDOVER.md");
        assert!(done["handover"]["markdown"].as_str().unwrap().contains("export shipped"));
        assert!(!workflows::find(&wt, &id).unwrap().running(), "idle once its runner is done");
        assert_eq!(workflows::find(&wt, &id).unwrap().send_backs, 2, "a completed handover does not start the count again (D62)");
        assert!(fs::read_to_string(wt.join("tasks/todo/TASK-9/HANDOVER.md")).unwrap().contains("revision: 1"));

        // Sent back: the same session continues, and the reason goes in the review log.
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let again = run_workflow_session(&client, &cfg, &skills, Some(&top), "The CSV header is wrong, fix it.").await;
        assert_eq!(again.as_deref(), Ok("closing note"));
        let done = sink.lock().unwrap().iter().find(|e| e["type"] == "workflow.done").cloned().unwrap();
        assert_eq!(done["handover"]["revision"], 2);
        let file = fs::read_to_string(wt.join("tasks/todo/TASK-9/HANDOVER.md")).unwrap();
        assert!(file.contains("The CSV header is wrong, fix it."), "{file}");
        std::env::remove_var("NOMAD_WORKFLOW_SESSION");
        std::env::remove_var("NOMAD_WORKFLOW_TASK_PATH");
        let _ = fs::remove_dir_all(wt);
        // A runner that never files its handover fails, and says so.
        a_runner_that_cannot_finish_reports_the_failure_and_goes_idle().await;
    }

    /// Part of the runner-process test: the two share the process-wide environment, so they run one after the other.
    async fn a_runner_that_cannot_finish_reports_the_failure_and_goes_idle() {
        let wt = new_worktree("runner-fails");
        let base = scripted_model(|_| text_message("no handover from me")).await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("workflow_runner", true)];
        let id = delegation::new_id("wf");
        workflows::write_entry(&wt, &workflows::Entry { workflow_id: id.clone(), task_path: TASK.into(), track: "implementation".into(), state: "running".into(), send_backs: 2 }).unwrap();
        std::env::set_var("NOMAD_WORKFLOW_SESSION", &id);
        std::env::set_var("NOMAD_WORKFLOW_TASK_PATH", TASK);
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: true };
        let answer = run_workflow_session(&client, &cfg, &skills, Some(&top), "go").await;
        std::env::remove_var("NOMAD_WORKFLOW_SESSION");
        std::env::remove_var("NOMAD_WORKFLOW_TASK_PATH");
        assert!(answer.unwrap_err().contains("without filing a handover"));
        let done = sink.lock().unwrap().iter().find(|e| e["type"] == "workflow.done").cloned().unwrap();
        assert_eq!(done["status"], "failed");
        assert!(done["error"].as_str().unwrap().contains("without filing a handover"));
        assert!(done["handover"].is_null());
        assert!(!workflows::find(&wt, &id).unwrap().running());
        assert_eq!(workflows::find(&wt, &id).unwrap().send_backs, 2, "a failed run leaves the count as it was");
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn a_handover_cannot_claim_verified_over_a_validation_that_failed() {
        let wt = new_worktree("verified-claim");
        fs::write(wt.join(TASK), "---\nid: T\n---\n").unwrap();
        let scope = WorkflowScope {
            session_id: "wf_claim".into(),
            task_path: TASK.into(),
            worktree: wt.clone(),
            agent: "workflow_runner".into(),
            filed: Mutex::new(None),
            last_validation_failure: Mutex::new(Some("FAIL: docs/specifications/EXPORT.md has fewer than 40 words".into())),
            nudge: Mutex::new(None),
        };
        let claimed = file_handover(Some(&scope), &valid_handover("done"));
        assert!(claimed.starts_with("Error: handover rejected.") && claimed.contains("validation says: FAIL: docs/specifications/EXPORT.md has fewer than 40 words"), "{claimed}");
        assert!(scope.filed.lock().unwrap().is_none(), "nothing was filed");

        // Honest about it: `unverified` (and partial) is accepted.
        let mut honest = valid_handover("spec still too short");
        honest["outcome"] = json!("partial");
        honest["verification"] = json!("unverified");
        honest["open_issues"] = json!(["the validator still fails"]);
        honest["requirements"][0]["status"] = json!("partial");
        let filed = file_handover(Some(&scope), &honest);
        assert!(filed.starts_with("Handover filed:"), "{filed}");

        // The runner never ran the validator, but the project has one: it is run at handover, and its FAIL stops the claim.
        *scope.last_validation_failure.lock().unwrap() = None;
        fs::create_dir_all(wt.join("scripts")).unwrap();
        fs::write(wt.join("scripts/validate-nomadworks.sh"), "#!/bin/bash\necho 'FAIL: no specification under docs/specifications/'\nexit 1\n").unwrap();
        let unrun = file_handover(Some(&scope), &valid_handover("claimed without running it"));
        assert!(unrun.starts_with("Error: handover rejected.") && unrun.contains("validation says: FAIL: no specification under docs/specifications/"), "{unrun}");
        fs::write(wt.join("scripts/validate-nomadworks.sh"), "#!/bin/bash\necho 'PASS: fine'\n").unwrap();

        // The validation passes on a later try: the claim goes through.
        *scope.last_validation_failure.lock().unwrap() = None;
        let ok = file_handover(Some(&scope), &valid_handover("fixed"));
        assert!(ok.starts_with("Handover filed:") && ok.contains("verification verified"), "{ok}");
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn the_handover_tool_belongs_to_workflow_sessions_only() {
        assert!(file_handover(None, &valid_handover("x")).starts_with("Error: the handover tool is only available inside a workflow session"));
        let with = tool_specs(ToolMode::All, &[], 0, true, "m").unwrap();
        let without = tool_specs(ToolMode::All, &[], 0, false, "m").unwrap();
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

    /// Blocks the calling (test model) thread until both parallel writers have reached their last turn, so the lane
    /// of whichever one finishes first is still held when the other's write has already been tried. A fixed sleep
    /// made these tests depend on how fast the machine was (D22, and again under load in Phase 7).
    fn wait_for_both(arrived: &AtomicUsize) {
        arrived.fetch_add(1, Ordering::SeqCst);
        let started = std::time::Instant::now();
        while arrived.load(Ordering::SeqCst) < 2 && started.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn two_writers(shared: String, other: Option<String>) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
        let arrived = Arc::new(AtomicUsize::new(0));
        // Lanes are named by call id and the lane table is process-wide: tests that run at the same time need ids of their own.
        let (id_a, id_b) = (format!("ta-{}", delegation::new_task_id()), format!("tb-{}", delegation::new_task_id()));
        move |req| {
            let system = system_text(req);
            let last = last_message(req);
            if system.contains("agent `writer`") {
                if last["role"] == "tool" {
                    // Hold this writer's lane open until the other writer has also tried its write (it is at its last
                    // turn only after that), so one of the two is always turned away.
                    wait_for_both(&arrived);
                    return text_message("writer done");
                }
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
                { "id": id_a, "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "writer", "description": "a", "prompt": "WRITER-A" }).to_string() } },
                { "id": id_b, "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "writer", "description": "b", "prompt": "WRITER-B" }).to_string() } },
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
        let _turn = CONTROL_DIR_LOCK.lock().await;
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
        let _turn = CONTROL_DIR_LOCK.lock().await;
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

    // ------------------------------------------------------------ OpenCode's tool surface

    /// One agent that works through `steps` (one tool call per round trip), then reports.
    async fn run_steps(steps: Vec<(&'static str, Value)>) -> Run {
        let steps = Arc::new(steps);
        let base = scripted_model(move |req| {
            let done = req["messages"].as_array().map(|m| m.iter().filter(|x| x["role"] == "tool").count()).unwrap_or(0);
            match steps.get(done) {
                Some((name, args)) => call_message(&format!("s{done}"), name, args.clone()),
                None => text_message("ALL STEPS DONE"),
            }
        })
        .await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let (emitter, sink) = Emitter::capturing();
        let top = TopLevel { emitter: &emitter, base_url: base.clone(), stream: false, protocol2: false };
        let mut messages = vec![json!({ "role": "system", "content": "you work" }), json!({ "role": "user", "content": "go" })];
        // Its own todo scope: tests share one process, and the top-level run's default scope is shared state.
        let owner = delegation::new_id("test");
        let scope = RunScope { checkpoint: None, workflow: None, lane: None, owner: Some(owner.as_str()), rules: None };
        let started = std::time::Instant::now();
        let answer = run_agent_loop(&client, &cfg, &mut messages, &[], ToolMode::All, Some(&top), 0, Some(&scope)).await.map_err(|e| e.to_string());
        let events = sink.lock().unwrap().clone();
        Run { answer, messages, events, elapsed: started.elapsed() }
    }

    fn tool_outputs(run: &Run) -> Vec<String> {
        run.messages.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or_default().to_string()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_using_opencodes_names_and_arguments_can_plan_edit_search_and_fetch() {
        let dir = std::env::temp_dir().join(format!("surface-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/app.ts"), "export const answer = 41\nexport const name = 'x'\n").unwrap();
        let (file, root) = (dir.join("src/app.ts").to_string_lossy().to_string(), dir.to_string_lossy().to_string());
        let site = tools::tests::serve(vec![("/doc", "text/html", b"<h1>Docs</h1><p>See <a href=\"/more\">more</a>.</p>".to_vec())]).await;

        let run = run_steps(vec![
            // lower-case names and camelCase arguments, sloppy statuses: what OpenCode prompts and weak models send
            ("todowrite", json!({ "todos": [{ "content": "Fix the answer", "status": "doing" }, { "content": "Check it", "status": "todo", "priority": "HIGH" }] })),
            ("edit", json!({ "filePath": file, "oldString": "answer = 41", "newString": "answer = 42" })),
            ("grep", json!({ "pattern": "answer", "path": root, "include": "*.ts" })),
            ("glob", json!({ "pattern": "src/*.ts", "path": root })),
            ("webfetch", json!({ "url": format!("{site}/doc"), "format": "markdown" })),
            ("todoread", json!({})),
        ])
        .await;
        assert_eq!(run.answer.as_deref(), Ok("ALL STEPS DONE"));
        let out = tool_outputs(&run);
        assert!(out[0].starts_with("Todo list updated: 2 item(s): 0 completed, 1 in progress, 1 pending"), "{}", out[0]);
        assert!(out[1].contains("-export const answer = 41") && out[1].contains("+export const answer = 42"), "{}", out[1]);
        assert_eq!(fs::read_to_string(&file).unwrap(), "export const answer = 42\nexport const name = 'x'\n");
        assert!(out[2].starts_with("Found 1 match") && out[2].contains("Line 1: export const answer = 42"), "{}", out[2]);
        assert!(out[3].contains("app.ts"), "{}", out[3]);
        assert_eq!(out[4], "# Docs\n\nSee [more](/more).");
        assert!(out[5].contains("\"content\": \"Check it\"") && out[5].contains("\"priority\": \"high\""), "{}", out[5]);

        // The events the UI is fed carry canonical names and a todo list in canonical form.
        let tool_events: Vec<&Value> = run.events.iter().filter(|e| e["type"] == "tool").collect();
        let names: Vec<&str> = tool_events.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["TodoWrite", "Edit", "Grep", "Glob", "WebFetch", "TodoRead"]);
        assert_eq!(tool_events[0]["input"]["todos"][0]["status"], "in_progress", "canonical status for the plan panel");
        assert_eq!(tool_events[1]["input"]["old_string"], "answer = 41", "arguments were normalized to one style");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_delegated_task_keeps_its_own_todo_list() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let base = scripted_model(move |req| {
            let system = system_text(req);
            let done = req["messages"].as_array().map(|m| m.iter().filter(|x| x["role"] == "tool").count()).unwrap_or(0);
            if system.contains("agent `helper`") {
                return if done == 0 { call_message("h1", "TodoWrite", json!({ "todos": [{ "content": "helper step", "status": "pending" }] })) } else { text_message("helper done") };
            }
            match done {
                0 => call_message("m1", "TodoWrite", json!({ "todos": [{ "content": "manager plan", "status": "in_progress" }] })),
                1 => call_message("m2", "Task", json!({ "subagent_type": "helper", "description": "h", "prompt": "do it" })),
                2 => call_message("m3", "TodoRead", json!({})),
                _ => {
                    record.lock().unwrap().push(req["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string());
                    text_message("MANAGER DONE")
                }
            }
        })
        .await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("helper", true)];
        let mut messages = vec![json!({ "role": "system", "content": "you manage" }), json!({ "role": "user", "content": "go" })];
        let owner = delegation::new_id("test");
        let scope = RunScope { checkpoint: None, workflow: None, lane: None, owner: Some(owner.as_str()), rules: None };
        run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, None, 0, Some(&scope)).await.unwrap();
        let last_read = seen.lock().unwrap()[0].clone();
        assert!(last_read.contains("manager plan") && !last_read.contains("helper step"), "the helper's list did not replace the manager's: {last_read}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_tasks_cannot_edit_the_same_file_either() {
        let _turn = CONTROL_DIR_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!("lanes-edit-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.txt");
        fs::write(&shared, "alpha\nbeta\n").unwrap();
        let path = shared.to_string_lossy().to_string();
        let arrived = Arc::new(AtomicUsize::new(0));
        let (id_a, id_b) = (format!("ta-{}", delegation::new_task_id()), format!("tb-{}", delegation::new_task_id()));
        let base = scripted_model(move |req| {
            let system = system_text(req);
            if system.contains("agent `editor`") {
                if last_message(req)["role"] == "tool" {
                    wait_for_both(&arrived); // keep the lane open until the other editor has tried (see two_writers)
                    return text_message("editor done");
                }
                let mine = req["messages"].to_string().contains("EDITOR-B");
                let (old, new) = if mine { ("beta", "BETA") } else { ("alpha", "ALPHA") };
                return call_message("e", "Edit", json!({ "file_path": path, "old_string": old, "new_string": new }));
            }
            if req["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["role"] == "tool")) { return text_message("PMA DONE"); }
            json!({ "role": "assistant", "content": null, "tool_calls": [
                { "id": id_a, "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "editor", "description": "a", "prompt": "EDITOR-A" }).to_string() } },
                { "id": id_b, "type": "function", "function": { "name": "Task", "arguments": json!({ "subagent_type": "editor", "description": "b", "prompt": "EDITOR-B" }).to_string() } },
            ] })
        })
        .await;
        let cfg = LlmConfig { base_url: base.clone(), fallback_base_url: base.clone(), api_key: "k".into(), model: "m".into(), max_tokens: 256 };
        let client = make_client(&base, "k");
        let skills = vec![skill("editor", true)];
        let mut messages = vec![json!({ "role": "system", "content": "you orchestrate" }), json!({ "role": "user", "content": "go" })];
        run_agent_loop(&client, &cfg, &mut messages, &skills, ToolMode::All, None, 0, None).await.unwrap();
        let refusals: Vec<String> = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["content"].as_str().and_then(|r| r.split("task_id: ").nth(1)).and_then(|t| t.split_whitespace().next()).and_then(delegation::load_task))
            .flat_map(|t| t.messages)
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["content"].as_str().map(str::to_string))
            .filter(|c| c.contains("is being written by another task running in parallel"))
            .collect();
        assert_eq!(refusals.len(), 1, "one editor is turned away: {refusals:?}");
        let content = fs::read_to_string(&shared).unwrap();
        assert!(content == "ALPHA\nbeta\n" || content == "alpha\nBETA\n", "exactly one edit landed, whole: {content:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_large_edit_diff_reaches_the_ui_whole() {
        let dir = std::env::temp_dir().join(format!("bigedit-{}-{}", std::process::id(), delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.txt");
        fs::write(&file, (1..=120).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let run = run_steps(vec![("Edit", json!({ "file_path": file.to_string_lossy(), "old_string": "line", "new_string": "LINE", "replace_all": true }))]).await;
        let shown = run.events.iter().find(|e| e["type"] == "tool_result").unwrap()["output"].as_str().unwrap().to_string();
        let model = tool_outputs(&run)[0].clone();
        assert!(model.len() > 2000, "a diff well over the usual limit: {}", model.len());
        assert_eq!(shown, model, "the UI gets the whole diff, so its hunk line counts match");
        // Other tools stay capped.
        assert_eq!(ui_output_limit("Grep"), 2000);
        let _ = fs::remove_dir_all(dir);
    }
}
