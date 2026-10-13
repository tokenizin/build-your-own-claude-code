//! NomadWorks agent roles → `context: fork` skills.
//!
//! Discovers `.nomadworks/agents/*.md` (walk-up + `NOMADWORKS_PATH`) and registers
//! core delivery roles (PMA, BA, tech_lead, developer, qa_engineer) plus aliases
//! (`pma`, `ba`, …) as forked skills with policy injection.

use crate::skills::{Skill, parse_frontmatter};
use std::fs;
use std::path::{Path, PathBuf};

/// Core P2 roles and short slash aliases.
const CORE_ROLES: &[(&str, &[&str])] = &[
    ("product_manager", &["pma"]),
    ("business_analyst", &["ba"]),
    ("tech_lead", &["tech-lead", "tl"]),
    ("developer", &["dev"]),
    ("qa_engineer", &["qa"]),
    ("workflow_runner", &["runner", "wr"]),
];

/// Policies from `starworld/.nomadworks/nomadworks.yaml` for core roles.
fn default_policies_for(agent: &str) -> &'static [&'static str] {
    match agent {
        "product_manager" => &[
            "starworld",
            "ecosystem.workflow",
            "ecosystem.a2a",
            "ecosystem.methodology",
            "hindsight-memory",
        ],
        "business_analyst" => &["starworld", "hindsight-memory", "ecosystem.methodology"],
        "tech_lead" => &["hindsight-memory", "starworld", "ecosystem.methodology"],
        "developer" => &[
            "starworld",
            "frontend-state-management",
            "hindsight-memory",
            "ecosystem.dapp-ui",
            "ecosystem.methodology",
        ],
        "qa_engineer" => &["starworld", "ecosystem.methodology", "hindsight-memory"],
        "workflow_runner" => &["starworld", "ecosystem.workflow", "ecosystem.methodology"],
        _ => &["starworld", "ecosystem.methodology"],
    }
}

fn default_model_for(agent: &str) -> Option<&'static str> {
    // Align with NomadWorks capability router defaults (Ollama tags, no provider prefix).
    match agent {
        "product_manager"
        | "business_analyst"
        | "tech_lead"
        | "developer"
        | "qa_engineer"
        | "workflow_runner" => Some("qwen3.6:latest"),
        _ => None,
    }
}

/// Find `.nomadworks` directories (project walk-up + env).
pub fn collect_nomadworks_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let scope_all = std::env::var("SKILLS_SCOPE")
        .map(|v| v.eq_ignore_ascii_case("all"))
        .unwrap_or(false);

    let mut path = std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    for _ in 0..12 {
        let candidate = path.join(".nomadworks");
        if candidate.is_dir() {
            let key = candidate.canonicalize().unwrap_or(candidate);
            if seen.insert(key.clone()) {
                roots.push(key);
            }
        }
        if !scope_all && path.join(".git").exists() {
            break;
        }
        if !path.pop() {
            break;
        }
    }

    if let Ok(value) = std::env::var("NOMADWORKS_PATH") {
        for part in value.split(':') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            let candidate = PathBuf::from(trimmed);
            let resolved = candidate.canonicalize().unwrap_or(candidate);
            if resolved.is_dir() && seen.insert(resolved.clone()) {
                roots.push(resolved);
            }
        }
    }

    // Convenience default when developing from the course repo on this machine.
    if roots.is_empty() {
        if let Ok(value) = std::env::var("STARWORLD") {
            let candidate = PathBuf::from(value).join(".nomadworks");
            if candidate.is_dir() {
                let resolved = candidate.canonicalize().unwrap_or(candidate);
                roots.push(resolved);
            }
        }
    }

    roots
}

fn policy_filename(policy_id: &str) -> String {
    // ecosystem.methodology → ecosystem-methodology.md
    format!("{}.md", policy_id.replace('.', "-"))
}

/// Load policy markdown texts for injection into a forked subagent.
pub fn load_policies(policy_ids: &[String], nomad_roots: &[PathBuf]) -> String {
    let max_total: usize = std::env::var("NOMAD_POLICY_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(14_000);

    let mut out = String::new();
    out.push_str("# NomadWorks policies (injected for this forked agent)\n\n");

    for id in policy_ids {
        let filename = policy_filename(id);
        let mut loaded = false;
        for root in nomad_roots {
            let path = root.join("policies").join(&filename);
            if let Ok(text) = fs::read_to_string(&path) {
                let header = format!("## Policy: {id}\n\n");
                if out.len() + header.len() + text.len() > max_total {
                    out.push_str(&format!(
                        "## Policy: {id}\n\n[truncated — set NOMAD_POLICY_MAX_CHARS higher or Read {}]\n\n",
                        path.display()
                    ));
                } else {
                    out.push_str(&header);
                    out.push_str(text.trim());
                    out.push_str("\n\n");
                }
                loaded = true;
                break;
            }
        }
        if !loaded {
            out.push_str(&format!(
                "## Policy: {id}\n\n[missing — expected policies/{}]\n\n",
                filename
            ));
        }
    }
    out
}

fn parse_policy_list(raw: &str) -> Vec<String> {
    // Supports: "starworld, ecosystem.methodology" or YAML-ish "- starworld"
    raw.split(|c| c == ',' || c == '\n')
        .map(|s| s.trim().trim_start_matches('-').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn load_agent_file(path: &Path) -> Option<(String, String, Vec<String>, Option<String>)> {
    let raw = fs::read_to_string(path).ok()?;
    let (fm, _body) = parse_frontmatter(&raw);
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() || name == "README" {
        return None;
    }
    let description = fm
        .get("description")
        .cloned()
        .unwrap_or_else(|| format!("NomadWorks agent role: {name}"));
    let mut policies = fm
        .get("policies")
        .map(|p| parse_policy_list(p))
        .unwrap_or_default();
    if policies.is_empty() {
        policies = default_policies_for(&name)
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    let model = fm
        .get("model")
        .cloned()
        .or_else(|| default_model_for(&name).map(|s| s.to_string()));
    Some((name, description, policies, model))
}

/// Discover core NomadWorks agents as forked skills (plus aliases).
pub fn discover_agent_skills() -> Vec<Skill> {
    let roots = collect_nomadworks_roots();
    if roots.is_empty() {
        return Vec::new();
    }

    let mut by_name: std::collections::HashMap<String, Skill> = std::collections::HashMap::new();

    for root in &roots {
        let agents_dir = root.join("agents");
        if !agents_dir.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&agents_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some((name, description, policies, model)) = load_agent_file(&path) else {
                continue;
            };

            // Only auto-register P2 core roles (avoid flooding catalog with 46 agents).
            let is_core = CORE_ROLES.iter().any(|(n, _)| *n == name);
            if !is_core {
                continue;
            }

            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = parse_frontmatter(&raw);
            let directory = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| agents_dir.clone());

            let skill = Skill {
                name: name.clone(),
                description: format!("[NomadWorks fork] {description}"),
                body,
                directory,
                context: Some("fork".to_string()),
                policies,
                model,
            };
            by_name.entry(name).or_insert(skill);
        }
    }

    // Aliases → clones with alternate names.
    let mut extras = Vec::new();
    for (canonical, aliases) in CORE_ROLES {
        if let Some(base) = by_name.get(*canonical) {
            for alias in *aliases {
                if by_name.contains_key(*alias) {
                    continue;
                }
                let mut cloned = base.clone();
                cloned.name = (*alias).to_string();
                extras.push(cloned);
            }
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

/// Build forked subagent messages: system(policies) + user(role body + args).
pub fn build_fork_messages(skill: &Skill, body: &str) -> Vec<serde_json::Value> {
    use serde_json::json;

    let roots = collect_nomadworks_roots();
    let mut messages = Vec::new();

    if !skill.policies.is_empty() {
        let policy_text = load_policies(&skill.policies, &roots);
        messages.push(json!({
            "role": "system",
            "content": policy_text,
        }));
    }

    messages.push(json!({
        "role": "system",
        "content": format!(
            "You are running as NomadWorks agent/skill `{}` in a forked subagent context. \
You do not see the parent conversation. Use Read/Write/Bash when needed. \
When finished, respond with a concise final answer for the parent agent.",
            skill.name
        ),
    }));

    messages.push(json!({
        "role": "user",
        "content": body,
    }));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_filename_maps_dots() {
        assert_eq!(policy_filename("ecosystem.methodology"), "ecosystem-methodology.md");
        assert_eq!(policy_filename("starworld"), "starworld.md");
    }

    #[test]
    fn parse_policy_list_csv() {
        let list = parse_policy_list("starworld, ecosystem.methodology");
        assert_eq!(list, vec!["starworld", "ecosystem.methodology"]);
    }
}
