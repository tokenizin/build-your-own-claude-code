//! Load OpenCode / CodeNomad `opencode.json(c)` agent catalogs for the Rust playground.
//!
//! StarWorld / CodeNomad remain source of truth. This module resolves `{file:…}` prompts,
//! remaps provider-prefixed models to Ollama tags, and exposes agents as `context: fork` skills.

use crate::skills::{Skill, parse_frontmatter};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct OpencodeAdapter {
    pub default_fork_model: String,
    pub small_model: Option<String>,
    pub model_remap: HashMap<String, String>,
    pub aliases: HashMap<String, String>,
    pub core_fallback: Vec<String>,
    pub agent_mode: String,
}

impl Default for OpencodeAdapter {
    fn default() -> Self {
        let mut model_remap = HashMap::new();
        model_remap.insert(
            "opencode/space-bunny-free".into(),
            "qwen3.6:latest".into(),
        );
        model_remap.insert(
            "openrouter-fallback/auto".into(),
            "qwen3.6:latest".into(),
        );
        model_remap.insert("opencode-go/mimo-v2.5".into(), "qwen3.6:latest".into());

        let mut aliases = HashMap::new();
        for (a, c) in [
            ("pma", "product_manager"),
            ("ba", "business_analyst"),
            ("tl", "tech_lead"),
            ("tech-lead", "tech_lead"),
            ("dev", "developer"),
            ("qa", "qa_engineer"),
            ("runner", "workflow_runner"),
            ("wr", "workflow_runner"),
        ] {
            aliases.insert(a.into(), c.into());
        }

        Self {
            default_fork_model: "qwen3.6:latest".into(),
            small_model: Some("qwen3:8b".into()),
            model_remap,
            aliases,
            core_fallback: vec![
                "product_manager".into(),
                "business_analyst".into(),
                "tech_lead".into(),
                "developer".into(),
                "qa_engineer".into(),
                "workflow_runner".into(),
            ],
            agent_mode: "opencode".into(),
        }
    }
}

fn expand_env_path(raw: &str) -> String {
    let mut out = raw.to_string();
    if let Ok(sw) = std::env::var("STARWORLD") {
        out = out.replace("${STARWORLD}", &sw);
        out = out.replace("$STARWORLD", &sw);
    }
    if let Ok(root) = std::env::var("CLAUDE_CODE_PLAYGROUND") {
        out = out.replace("${CLAUDE_CODE_PLAYGROUND}", &root);
        out = out.replace("$CLAUDE_CODE_PLAYGROUND", &root);
    }
    out
}

/// Course-repo adapter next to playground/, or OPENCODE_RUST_ADAPTER.
pub fn load_adapter() -> OpencodeAdapter {
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("OPENCODE_RUST_ADAPTER") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(root) = std::env::var("CLAUDE_CODE_PLAYGROUND") {
        candidates.push(PathBuf::from(root).join("playground/opencode-rust.json"));
    }
    // Relative to CWD and common layouts
    candidates.push(PathBuf::from("playground/opencode-rust.json"));
    candidates.push(PathBuf::from("../opencode-rust.json"));
    candidates.push(PathBuf::from("../../playground/opencode-rust.json"));
    // Absolute fallback for this machine
    candidates.push(PathBuf::from(
        "/Users/alexshapiro/0-claudecode/build-your-own-claude-code/playground/opencode-rust.json",
    ));

    for path in candidates {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                return adapter_from_json(&v);
            }
        }
    }
    OpencodeAdapter::default()
}

fn adapter_from_json(v: &Value) -> OpencodeAdapter {
    let mut a = OpencodeAdapter::default();
    if let Some(s) = v.get("defaultForkModel").and_then(|x| x.as_str()) {
        a.default_fork_model = s.to_string();
    }
    if let Some(s) = v.get("smallModel").and_then(|x| x.as_str()) {
        a.small_model = Some(s.to_string());
    }
    if let Some(s) = v.get("agentMode").and_then(|x| x.as_str()) {
        a.agent_mode = s.to_string();
    }
    if let Some(obj) = v.get("modelRemap").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                a.model_remap.insert(k.clone(), s.to_string());
            }
        }
    }
    if let Some(obj) = v.get("aliases").and_then(|x| x.as_object()) {
        a.aliases.clear();
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                a.aliases.insert(k.clone(), s.to_string());
            }
        }
    }
    if let Some(arr) = v.get("coreFallback").and_then(|x| x.as_array()) {
        a.core_fallback = arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect();
    }
    a
}

/// Strip `//` and `/* */` comments and trailing commas so JSONC parses as JSON.
pub fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() {
            let n = bytes[i + 1] as char;
            if n == '/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if n == '*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    // Remove trailing commas before } or ]
    let mut cleaned = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    let mut j = 0;
    while j < chars.len() {
        if chars[j] == ',' {
            let mut k = j + 1;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            if k < chars.len() && (chars[k] == '}' || chars[k] == ']') {
                j += 1;
                continue;
            }
        }
        cleaned.push(chars[j]);
        j += 1;
    }
    cleaned
}

pub fn resolve_opencode_config_path(adapter: &OpencodeAdapter) -> Option<PathBuf> {
    let _ = adapter;
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("OPENCODE_CONFIG") {
        candidates.push(PathBuf::from(expand_env_path(&p)));
    }
    // Prefer CodeNomad catalog (full subagent set), then StarWorld root.
    if let Ok(sw) = std::env::var("STARWORLD") {
        candidates.push(PathBuf::from(&sw).join("CodeNomad/.opencode/opencode.jsonc"));
        candidates.push(PathBuf::from(&sw).join("opencode.json"));
        candidates.push(PathBuf::from(&sw).join(".opencode/opencode.json"));
    }
    candidates.push(PathBuf::from(
        "/Users/alexshapiro/starworld/CodeNomad/.opencode/opencode.jsonc",
    ));
    candidates.push(PathBuf::from("/Users/alexshapiro/starworld/opencode.json"));

    for path in candidates {
        if path.is_file() {
            return Some(path.canonicalize().unwrap_or(path));
        }
    }
    None
}

pub fn remap_model(adapter: &OpencodeAdapter, raw: Option<&str>) -> Option<String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(adapter.default_fork_model.clone());
    };
    if let Some(mapped) = adapter.model_remap.get(raw) {
        return Some(mapped.clone());
    }
    if let Some(rest) = raw.strip_prefix("ollama/") {
        return Some(rest.to_string());
    }
    // Unknown provider-prefixed models → default Ollama fork model
    if raw.contains('/') {
        return Some(adapter.default_fork_model.clone());
    }
    Some(raw.to_string())
}

fn resolve_prompt_file(config_dir: &Path, prompt: &str) -> Option<PathBuf> {
    let trimmed = prompt.trim();
    let inner = trimmed
        .strip_prefix("{file:")
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(trimmed);
    let path = PathBuf::from(inner);
    let resolved = if path.is_absolute() {
        path
    } else {
        config_dir.join(&path)
    };
    if resolved.is_file() {
        Some(resolved.canonicalize().unwrap_or(resolved))
    } else {
        None
    }
}

/// Agent entry from OpenCode JSON.
#[derive(Debug, Clone)]
pub struct OpencodeAgentSpec {
    pub name: String,
    pub description: String,
    pub mode: String,
    pub model: Option<String>,
    pub prompt_path: PathBuf,
}

pub fn parse_opencode_agents(config_path: &Path) -> Result<(Value, Vec<OpencodeAgentSpec>), String> {
    let raw = fs::read_to_string(config_path).map_err(|e| e.to_string())?;
    let cleaned = strip_jsonc(&raw);
    let root: Value = serde_json::from_str(&cleaned).map_err(|e| e.to_string())?;
    let config_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut specs = Vec::new();
    let Some(agents) = root.get("agent").and_then(|a| a.as_object()) else {
        return Ok((root, specs));
    };

    for (name, entry) in agents {
        let prompt = entry
            .get("prompt")
            .and_then(|p| p.as_str())
            .unwrap_or("");
        let Some(prompt_path) = resolve_prompt_file(&config_dir, prompt) else {
            if std::env::var("SKILLS_DEBUG").is_ok_and(|v| v != "0") {
                let _ = writeln!(
                    io::stderr(),
                    "opencode: skip agent `{name}` — prompt not found: {prompt}"
                );
            }
            continue;
        };
        let description = entry
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or(name)
            .to_string();
        let mode = entry
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("subagent")
            .to_string();
        let model = entry
            .get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        specs.push(OpencodeAgentSpec {
            name: name.clone(),
            description,
            mode,
            model,
            prompt_path,
        });
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((root, specs))
}

pub fn agent_mode_setting(adapter: &OpencodeAdapter) -> String {
    std::env::var("NOMAD_AGENTS")
        .or_else(|_| std::env::var("PLAYGROUND_AGENTS"))
        .unwrap_or_else(|_| adapter.agent_mode.clone())
        .to_lowercase()
}

/// Build fork skills from OpenCode agent catalog (+ aliases).
pub fn discover_opencode_agent_skills(
    adapter: &OpencodeAdapter,
    default_policies: &dyn Fn(&str) -> Vec<String>,
) -> Vec<Skill> {
    let mode = agent_mode_setting(adapter);
    if mode == "core" || mode == "off" {
        return Vec::new();
    }

    let Some(config_path) = resolve_opencode_config_path(adapter) else {
        let _ = writeln!(
            io::stderr(),
            "opencode: no opencode.json(c) found (set OPENCODE_CONFIG or STARWORLD)"
        );
        return Vec::new();
    };

    let debug = std::env::var("SKILLS_DEBUG").is_ok_and(|v| v != "0");
    let (root, specs) = match parse_opencode_agents(&config_path) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "opencode: failed to parse {}: {e}",
                config_path.display()
            );
            return Vec::new();
        }
    };

    if debug {
        let _ = writeln!(
            io::stderr(),
            "opencode: loaded {} agent(s) from {}",
            specs.len(),
            config_path.display()
        );
    }

    // Top-level model as fallback for agents without pin
    let top_model = root
        .get("model")
        .and_then(|m| m.as_str())
        .and_then(|m| remap_model(adapter, Some(m)));

    let mut by_name: HashMap<String, Skill> = HashMap::new();

    for spec in specs {
        let Ok(raw) = fs::read_to_string(&spec.prompt_path) else {
            continue;
        };
        let (fm, body) = parse_frontmatter(&raw);
        let description = fm
            .get("description")
            .cloned()
            .unwrap_or_else(|| spec.description.clone());
        let mut policies = fm
            .get("policies")
            .map(|p| {
                p.split(|c| c == ',' || c == '\n')
                    .map(|s| s.trim().trim_start_matches('-').trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if policies.is_empty() {
            policies = default_policies(&spec.name);
        }

        let model = remap_model(
            adapter,
            spec.model
                .as_deref()
                .or_else(|| fm.get("model").map(|s| s.as_str()))
                .or(top_model.as_deref()),
        );

        let directory = spec
            .prompt_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let skill = Skill {
            name: spec.name.clone(),
            description: format!(
                "[OpenCode {}] {description}",
                spec.mode
            ),
            body,
            directory,
            context: Some("fork".to_string()),
            policies,
            model,
        };
        by_name.entry(spec.name).or_insert(skill);
    }

    // Aliases
    let mut extras = Vec::new();
    for (alias, canonical) in &adapter.aliases {
        if by_name.contains_key(alias) {
            continue;
        }
        if let Some(base) = by_name.get(canonical) {
            let mut cloned = base.clone();
            cloned.name = alias.clone();
            extras.push(cloned);
        }
    }
    for skill in extras {
        by_name.insert(skill.name.clone(), skill);
    }

    let mut names: Vec<_> = by_name.keys().cloned().collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_comments_and_trailing_commas() {
        let raw = r#"{
          // comment
          "a": 1,
          "b": { "c": true, },
        }"#;
        let cleaned = strip_jsonc(raw);
        let v: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"]["c"], true);
    }

    #[test]
    fn remap_strips_ollama_prefix() {
        let a = OpencodeAdapter::default();
        assert_eq!(
            remap_model(&a, Some("ollama/qwen3:8b")).as_deref(),
            Some("qwen3:8b")
        );
        assert_eq!(
            remap_model(&a, Some("opencode/space-bunny-free")).as_deref(),
            Some("qwen3.6:latest")
        );
    }
}
