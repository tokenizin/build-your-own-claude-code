mod agents;
mod config;
mod skills;

use async_openai::{Client, config::OpenAIConfig};
use clap::Parser;
use config::LlmConfig;
use serde_json::{Value, json};
use skills::{
    Skill, collect_skill_roots, discover_skills, expand_slash_invocation, fork_user_body,
    format_skill_catalog, skill_by_name, substitute_arguments, wrap_skill_body_for_location,
};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(short = 'p', long)]
    prompt: String,
}

#[derive(Clone, Copy)]
enum ToolMode {
    /// No tools — smallest prompt (good for slash smoke tests).
    None,
    /// Read, Write, Bash
    Base,
    /// Read, Write, Bash, Skill
    All,
}

struct InitialRun {
    messages: Vec<Value>,
    tool_mode: ToolMode,
    /// Per-fork model override (NomadWorks agent roles).
    model_override: Option<String>,
}

fn tool_specs(mode: ToolMode) -> Option<Value> {
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
/// `scripts/` (keeps `/smoke-echo` lean). Forked agents get policies + Base tools.
fn build_initial_run(prompt: &str, skills: &[Skill]) -> InitialRun {
    if let Some(invocations) = expand_slash_invocation(prompt, skills) {
        if invocations.len() == 1 && invocations[0].fork {
            let inv = &invocations[0];
            if let Some(skill) = skill_by_name(skills, &inv.skill_name) {
                let messages = agents::build_fork_messages(skill, &inv.body);
                return InitialRun {
                    messages,
                    tool_mode: ToolMode::Base,
                    model_override: skill.model.clone(),
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
    }
}

fn make_client(base_url: &str, api_key: &str) -> Client<OpenAIConfig> {
    let config = OpenAIConfig::new()
        .with_api_base(base_url)
        .with_api_key(api_key);
    Client::with_config(config)
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
) -> Result<String, Box<dyn std::error::Error>> {
    let tools = tool_specs(tool_mode);

    for _ in 0..40 {
        let mut request = json!({
            "model": cfg.model,
            "messages": messages,
            "max_tokens": cfg.max_tokens,
        });
        if let Some(tools) = &tools {
            request["tools"] = tools.clone();
        }

        let response: Value = client
            .chat()
            .create_byot(request)
            .await
            .map_err(|err| format_api_error(err))?;

        let choice = response["choices"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or("no choices in response")?;

        let message = &choice["message"];
        let text = assistant_text(message);
        let mut assistant = json!({
            "role": "assistant",
            "content": if text.is_empty() {
                message.get("content").cloned().unwrap_or(Value::Null)
            } else {
                Value::String(text.clone())
            },
        });

        let tool_calls = message
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

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
            let fn_name = call["function"]["name"].as_str().unwrap_or("");
            let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
            let fn_args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            let call_id = call["id"].as_str().unwrap_or("");

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
                    ))
                    .await?
                }
                other => format!("Unknown tool: {other}"),
            };

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": result,
            }));
        }
    }

    Err("agent loop exceeded maximum iterations".into())
}

async fn handle_skill(
    client: &Client<OpenAIConfig>,
    cfg: &LlmConfig,
    skills: &[Skill],
    name: &str,
    skill_args: &str,
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

fn format_api_error(err: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    let text = err.to_string();
    if text.contains("403") || text.contains("error sending request") {
        return Box::from(format!(
            "Ollama endpoint rejected the request. If using https://ollama.tokenizin.com, \
ensure the Cloudflare tunnel is up, or set OLLAMA_BASE_URL=http://127.0.0.1:11434/v1 \
for on-machine use.\n\n{text}"
        ));
    }
    Box::from(text)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let cfg = LlmConfig::from_env();
    let (client, _active_base) = resolve_client(&cfg).await;

    let skills = discover_skills();
    eprintln!(
        "skills: {} loaded from {} root(s) (+ {} NomadWorks)",
        skills.len(),
        collect_skill_roots().len(),
        agents::collect_nomadworks_roots().len()
    );

    let initial = build_initial_run(&args.prompt, &skills);
    let run_cfg = match &initial.model_override {
        Some(model) => {
            eprintln!("model: {model} (fork override)");
            cfg.with_model(model)
        }
        None => {
            eprintln!("model: {}", cfg.model);
            cfg
        }
    };

    let mut messages = initial.messages;
    let final_text =
        run_agent_loop(&client, &run_cfg, &mut messages, &skills, initial.tool_mode).await?;
    println!("{final_text}");
    Ok(())
}
