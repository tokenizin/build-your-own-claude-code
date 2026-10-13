//! NomadWorks tool shims for the playground (P3).
//!
//! - `nomadworks_validate` — Phase 0 artifact check or strict CodeMap walk
//! - `nomadworks_start_discussion` / `stop_discussion` — file + registry (no OpenCode session API)
//! - `nomadflow_run_workflow` — synchronous nested fork (OpenCode session spawn is P4)
//! - `nomadflow_prompt_workflow` — explains sync model / follow-up via run_workflow again

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static SESSION_ID: OnceLock<String> = OnceLock::new();

pub fn session_id() -> &'static str {
    SESSION_ID.get_or_init(|| {
        std::env::var("NOMAD_SESSION_ID").unwrap_or_else(|_| {
            format!(
                "playground-{}",
                std::process::id()
            )
        })
    })
}

/// Worktree root for NomadWorks artifacts (cwd → STARWORLD → walk-up `.nomadworks` parent).
pub fn resolve_worktree() -> PathBuf {
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.join(".nomadworks").is_dir() || cwd.join("tasks").is_dir() {
            return cwd;
        }
    }
    if let Ok(sw) = std::env::var("STARWORLD") {
        let p = PathBuf::from(sw);
        if p.is_dir() {
            return p;
        }
    }
    for root in crate::agents::collect_nomadworks_roots() {
        if let Some(parent) = root.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "nomadworks_validate",
                "description": "Validate NomadWorks workflow artifacts and CodeMap integrity. mode=phase0 (default) runs scripts/validate-nomadworks.sh; mode=strict runs the full recursive CodeMap walk (often many pre-existing errors).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "description": "phase0 | strict",
                            "enum": ["phase0", "strict"]
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "nomadworks_start_discussion",
                "description": "Start or reopen a NomadWorks discussion transcript under tasks/discussions/ (playground file shim; no OpenCode live capture).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Discussion title for a new discussion"
                        },
                        "existing_discussion_id": {
                            "type": "string",
                            "description": "Existing discussion ID to reopen (e.g. DISCUSSION-001)"
                        },
                        "seed": {
                            "type": "string",
                            "description": "Optional opening note to append as the first message"
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "nomadworks_stop_discussion",
                "description": "Stop the active NomadWorks discussion for this playground session",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "nomadflow_run_workflow",
                "description": "Run a task lifecycle synchronously via a forked workflow_runner context (playground shim; not an OpenCode background session).",
                "parameters": {
                    "type": "object",
                    "required": ["task_path", "instructions"],
                    "properties": {
                        "task_path": {
                            "type": "string",
                            "description": "Path to the task markdown file (e.g. tasks/todo/TASK-….md)"
                        },
                        "instructions": {
                            "type": "string",
                            "description": "Detailed instructions for the workflow runner"
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "nomadflow_prompt_workflow",
                "description": "Follow-up to a workflow. Playground runs workflows synchronously — prefer nomadflow_run_workflow again with updated instructions.",
                "parameters": {
                    "type": "object",
                    "required": ["session_id", "text"],
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id from nomadflow_run_workflow (informational in playground)"
                        },
                        "text": {
                            "type": "string",
                            "description": "Follow-up message"
                        }
                    }
                }
            }
        }),
    ]
}

pub fn dispatch(name: &str, args: &Value) -> Option<String> {
    match name {
        "nomadworks_validate" => Some(execute_validate(args)),
        "nomadworks_start_discussion" => Some(execute_start_discussion(args)),
        "nomadworks_stop_discussion" => Some(execute_stop_discussion()),
        "nomadflow_prompt_workflow" => Some(execute_prompt_workflow(args)),
        // run_workflow needs async fork — handled in main
        "nomadflow_run_workflow" => None,
        _ => None,
    }
}

fn execute_validate(args: &Value) -> String {
    let mode = args["mode"].as_str().unwrap_or("phase0");
    let worktree = resolve_worktree();
    match mode {
        "strict" => validate_strict(&worktree),
        _ => validate_phase0(&worktree),
    }
}

fn validate_phase0(worktree: &Path) -> String {
    let script = worktree.join("scripts/validate-nomadworks.sh");
    if !script.is_file() {
        return format!(
            "FAIL: {} not found. Run from StarWorld (or set STARWORLD) for Phase 0 validate.",
            script.display()
        );
    }
    match Command::new("bash")
        .arg(&script)
        .current_dir(worktree)
        .env("NOMADWORKS_STRICT_CODEMAP", "0")
        .output()
    {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            truncate(&text, 8_000)
        }
        Err(e) => format!("FAIL: could not run validate script: {e}"),
    }
}

fn validate_strict(worktree: &Path) -> String {
    let pkg = worktree
        .join("node_modules/@neuralnomads/nomadworks/dist/validate_logic.js");
    if !pkg.is_file() {
        return format!(
            "FAIL: {} missing. Install @neuralnomads/nomadworks in the worktree first.",
            pkg.display()
        );
    }
    let pkg_url = format!("file://{}", pkg.display());
    let js = format!(
        r#"
import {{ nomadworks_validate_logic }} from '{pkg_url}';
const r = await nomadworks_validate_logic({workdir});
const max = 40;
if (r.ok) {{
  console.log('PASS: All source directories indexed. Hierarchy validated.');
  console.log('Warnings: ' + r.warnings.length);
  for (const w of r.warnings.slice(0, max)) console.log('- ' + w);
  if (r.warnings.length > max) console.log('... and ' + (r.warnings.length - max) + ' more');
}} else {{
  console.log('FAIL: Validation errors found: ' + r.errors.length);
  for (const e of r.errors.slice(0, max)) console.log('- ' + e);
  if (r.errors.length > max) console.log('... and ' + (r.errors.length - max) + ' more');
  console.log('Warnings: ' + r.warnings.length);
}}
"#,
        pkg_url = pkg_url.replace('\'', "%27"),
        workdir = serde_json::to_string(&worktree.to_string_lossy()).unwrap_or_else(|_| "\".\"".into()),
    );
    match Command::new("node")
        .args(["--input-type=module", "-e", &js])
        .current_dir(worktree)
        .output()
    {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            if text.trim().is_empty() {
                text = format!(
                    "FAIL: strict validate produced no output (exit {:?})",
                    out.status.code()
                );
            }
            truncate(&text, 12_000)
        }
        Err(e) => format!("FAIL: node not available for strict validate: {e}"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\n… [truncated at {max} chars]", &s[..max])
    }
}

fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in title.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let sliced: String = trimmed.chars().take(80).collect();
    if sliced.is_empty() {
        "discussion".into()
    } else {
        sliced
    }
}

fn registry_path(worktree: &Path) -> PathBuf {
    worktree.join(".codenomad/runtime/discussions.json")
}

fn load_registry(worktree: &Path) -> Value {
    let path = registry_path(worktree);
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| json!({"version": 1, "active": {}})),
        Err(_) => json!({"version": 1, "active": {}}),
    }
}

fn save_registry(worktree: &Path, registry: &Value) -> Result<(), String> {
    let path = registry_path(worktree);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let pretty = serde_json::to_string_pretty(registry).map_err(|e| e.to_string())?;
    fs::write(&path, pretty).map_err(|e| e.to_string())
}

fn execute_start_discussion(args: &Value) -> String {
    let worktree = resolve_worktree();
    let sid = session_id();
    let title = args["title"].as_str().unwrap_or("").trim();
    let existing = args["existing_discussion_id"].as_str().unwrap_or("").trim();
    let seed = args["seed"].as_str().unwrap_or("").trim();

    if (title.is_empty() && existing.is_empty()) || (!title.is_empty() && !existing.is_empty()) {
        return "Error: Provide exactly one of 'title' or 'existing_discussion_id'.".into();
    }

    let mut registry = load_registry(&worktree);
    if registry["active"][sid].is_object() {
        let ex = &registry["active"][sid];
        return format!(
            "FAIL: An active discussion already exists for this session.\nID: {}\nTitle: {}\nFile: {}\nStatus: {}",
            ex["id"].as_str().unwrap_or("?"),
            ex["title"].as_str().unwrap_or("?"),
            ex["filePath"].as_str().unwrap_or("?"),
            ex["status"].as_str().unwrap_or("?")
        );
    }

    let discussions_dir = worktree.join("tasks/discussions");
    if let Err(e) = fs::create_dir_all(&discussions_dir) {
        return format!("FAIL: could not create discussions dir: {e}");
    }

    let (id, relative, absolute, discussion_title, action) = if !existing.is_empty() {
        let matches: Vec<_> = match fs::read_dir(&discussions_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(&format!("{existing}-")) && n.ends_with(".md"))
                .collect(),
            Err(_) => Vec::new(),
        };
        if matches.is_empty() {
            return format!("Error: Discussion '{existing}' was not found.");
        }
        let filename = matches.into_iter().min().unwrap();
        let relative = format!("tasks/discussions/{filename}");
        let absolute = worktree.join(&relative);
        let raw = fs::read_to_string(&absolute).unwrap_or_default();
        let (mut fm, body) = crate::skills::parse_frontmatter(&raw);
        fm.insert("status".into(), "active".into());
        fm.insert("agent".into(), "playground".into());
        fm.insert("session_id".into(), sid.to_string());
        if let Err(e) = write_discussion_md(&absolute, &fm, &body) {
            return format!("FAIL: {e}");
        }
        let discussion_title = fm
            .get("title")
            .cloned()
            .unwrap_or_else(|| existing.to_string());
        (
            existing.to_string(),
            relative,
            absolute,
            discussion_title,
            "reopened",
        )
    } else {
        let mut sequence = 1u32;
        let (id, relative, absolute) = loop {
            let id = format!("DISCUSSION-{sequence:03}");
            let filename = format!("{}-{}.md", id, slugify(title));
            let relative = format!("tasks/discussions/{filename}");
            let absolute = worktree.join(&relative);
            if !absolute.exists() {
                break (id, relative, absolute);
            }
            sequence += 1;
            if sequence > 9999 {
                return "FAIL: could not allocate discussion id".into();
            }
        };
        let mut fm = std::collections::HashMap::new();
        fm.insert("id".into(), id.clone());
        fm.insert("title".into(), title.to_string());
        fm.insert("status".into(), "active".into());
        fm.insert("agent".into(), "playground".into());
        fm.insert("session_id".into(), sid.to_string());
        let body = format!("# Discussion: {title}\n\n## Messages\n");
        if let Err(e) = write_discussion_md(&absolute, &fm, &body) {
            return format!("FAIL: {e}");
        }
        (id, relative, absolute, title.to_string(), "started")
    };

    if !seed.is_empty() {
        let _ = append_discussion_message(&absolute, "User", seed);
    }

    registry["active"][sid] = json!({
        "id": id,
        "title": discussion_title,
        "filePath": relative,
        "status": "active",
        "agent": "playground",
        "appendedMessageIDs": []
    });
    if let Err(e) = save_registry(&worktree, &registry) {
        return format!("FAIL: could not save registry: {e}");
    }

    format!(
        "SUCCESS: Discussion {action}.\nID: {id}\nTitle: {discussion_title}\nFile: {relative}\nStatus: active\nSession: {sid}"
    )
}

fn execute_stop_discussion() -> String {
    let worktree = resolve_worktree();
    let sid = session_id();
    let mut registry = load_registry(&worktree);
    let Some(existing) = registry["active"].as_object().and_then(|m| m.get(sid)).cloned() else {
        return "FAIL: No active discussion exists for this session.".into();
    };
    let file_path = existing["filePath"].as_str().unwrap_or("");
    let absolute = worktree.join(file_path);
    if absolute.is_file() {
        let raw = fs::read_to_string(&absolute).unwrap_or_default();
        let (mut fm, body) = crate::skills::parse_frontmatter(&raw);
        fm.insert("status".into(), "closing".into());
        let _ = write_discussion_md(&absolute, &fm, &body);
    }
    if let Some(obj) = registry["active"].as_object_mut() {
        if let Some(entry) = obj.get_mut(sid) {
            entry["status"] = json!("closing");
        }
    }
    let _ = save_registry(&worktree, &registry);
    format!(
        "SUCCESS: Discussion stop requested.\nID: {}\nTitle: {}\nFile: {}\nStatus: closing",
        existing["id"].as_str().unwrap_or("?"),
        existing["title"].as_str().unwrap_or("?"),
        file_path
    )
}

fn write_discussion_md(
    path: &Path,
    fm: &std::collections::HashMap<String, String>,
    body: &str,
) -> Result<(), String> {
    let mut yaml = String::new();
    for key in ["id", "title", "status", "agent", "session_id"] {
        if let Some(v) = fm.get(key) {
            yaml.push_str(&format!("{key}: {v}\n"));
        }
    }
    for (k, v) in fm {
        if matches!(k.as_str(), "id" | "title" | "status" | "agent" | "session_id") {
            continue;
        }
        if k == "appended_message_ids" {
            continue;
        }
        yaml.push_str(&format!("{k}: {v}\n"));
    }
    let content = format!("---\n{}---\n\n{}\n", yaml, body.trim_end());
    fs::write(path, content).map_err(|e| e.to_string())
}

fn append_discussion_message(path: &Path, speaker: &str, text: &str) -> Result<(), String> {
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (fm, body) = crate::skills::parse_frontmatter(&raw);
    let entry = format!("**{speaker}**\n{}", text.trim());
    let next = if body.trim().is_empty() {
        entry
    } else {
        format!("{}\n\n{entry}", body.trim_end())
    };
    write_discussion_md(path, &fm, &next)
}

fn execute_prompt_workflow(args: &Value) -> String {
    let session = args["session_id"].as_str().unwrap_or("");
    let text = args["text"].as_str().unwrap_or("");
    format!(
        "FAIL: Playground workflows are synchronous (no OpenCode background session).\n\
Session '{session}' is not a live runner.\n\
Call nomadflow_run_workflow again with updated instructions instead.\n\
Follow-up text was:\n{text}"
    )
}

/// User body for a synchronous workflow_runner fork.
pub fn workflow_fork_prompt(task_path: &str, instructions: &str) -> String {
    let worktree = resolve_worktree();
    let abs = if Path::new(task_path).is_absolute() {
        PathBuf::from(task_path)
    } else {
        worktree.join(task_path)
    };
    let task_body = fs::read_to_string(&abs).unwrap_or_else(|e| {
        format!("[could not read task file {}: {e}]", abs.display())
    });
    format!(
        "You are the NomadWorks workflow_runner in a forked playground session.\n\
Task File: {task_path}\n\n\
## Task file contents\n\n{task_body}\n\n\
## Instructions\n\n{instructions}\n\n\
Execute the appropriate lifecycle for this task. Use Read/Write/Bash and \
nomadworks_validate when needed. When finished, respond with a concise final summary."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World!"), "hello-world");
    }
}
