use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub directory: PathBuf,
    pub context: Option<String>,
    /// NomadWorks policy ids (e.g. `ecosystem.methodology`) injected on fork.
    pub policies: Vec<String>,
    /// Optional model override for forked runs (Ollama tag).
    pub model: Option<String>,
}

/// Relative skill roots scanned at each ancestor of cwd (project → parent).
/// Order is precedence within a directory level (first wins).
const RELATIVE_SKILL_ROOTS: &[&str] = &[
    ".claude/skills",
    ".opencode/skills",
    ".agents/skills",
];

/// Env vars holding extra skill roots (colon-separated on Unix).
const EXTRA_PATH_ENV_KEYS: &[&str] = &[
    "NOMAD_SKILLS_PATH",
    "OPENCODE_SKILLS_PATH",
    "SKILLS_EXTRA_PATH",
];

/// Collect skill directories in precedence order (highest first).
///
/// 1. Walk up from cwd: at each level `.claude/skills`, then `.opencode/skills`,
///    then `.agents/skills` (closer ancestors beat farther ones).
/// 2. Then absolute roots from `NOMAD_SKILLS_PATH` / `OPENCODE_SKILLS_PATH` /
///    `SKILLS_EXTRA_PATH` (colon-separated).
pub fn collect_skill_roots() -> Vec<PathBuf> {
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
        for rel in RELATIVE_SKILL_ROOTS {
            let candidate = path.join(rel);
            push_root(&mut roots, &mut seen, candidate);
        }
        // Default scope is project-only: stop after the git worktree root.
        if !scope_all && path.join(".git").exists() {
            break;
        }
        if !path.pop() {
            break;
        }
    }

    for key in EXTRA_PATH_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            for part in value.split(':') {
                let trimmed = part.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let candidate = PathBuf::from(trimmed);
                let resolved = candidate.canonicalize().unwrap_or(candidate);
                push_root(&mut roots, &mut seen, resolved);
            }
        }
    }

    roots
}

fn push_root(
    roots: &mut Vec<PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
    candidate: PathBuf,
) {
    if !candidate.is_dir() {
        return;
    }
    let key = candidate.canonicalize().unwrap_or(candidate.clone());
    if seen.insert(key.clone()) {
        roots.push(key);
    }
}

/// Backward-compatible: first `.claude/skills` among roots, else first root, else default path.
#[allow(dead_code)]
pub fn find_skills_dir() -> PathBuf {
    let roots = collect_skill_roots();
    roots
        .iter()
        .find(|p| {
            p.file_name().is_some_and(|n| n == "skills")
                && p.parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n == ".claude")
        })
        .cloned()
        .or_else(|| roots.into_iter().next())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".claude")
                .join("skills")
        })
}

/// Parse YAML-ish frontmatter. Supports simple multiline values and `- item` lists
/// flattened into a comma-separated string for `policies`.
pub fn parse_frontmatter(text: &str) -> (HashMap<String, String>, String) {
    if !text.starts_with("---") {
        return (HashMap::new(), text.to_string());
    }

    let parts: Vec<&str> = text.splitn(3, "---").collect();
    if parts.len() < 3 {
        return (HashMap::new(), text.to_string());
    }

    let mut frontmatter: HashMap<String, String> = HashMap::new();
    let mut current_key: Option<String> = None;
    for line in parts[1].lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(key) = current_key.clone() {
                let entry: &mut String = frontmatter.entry(key).or_default();
                if trimmed.starts_with('-') {
                    let item = trimmed.trim_start_matches('-').trim();
                    if !entry.is_empty() {
                        entry.push_str(", ");
                    }
                    entry.push_str(item);
                } else {
                    if !entry.is_empty() {
                        entry.push(' ');
                    }
                    entry.push_str(trimmed);
                }
            }
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            current_key = Some(key.clone());
            frontmatter.insert(key, value);
        } else {
            current_key = None;
        }
    }

    let body = parts[2].trim_start_matches('\n').to_string();
    (frontmatter, body)
}

fn load_skills_from_dir(skills_dir: &Path) -> Vec<Skill> {
    let mut entries: Vec<_> = match fs::read_dir(skills_dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort_by_key(|e| e.file_name());

    let mut skills = Vec::new();
    for entry in entries {
        let path = entry.path();
        // Follow symlinks to skill folders (P0 link script).
        let meta = fs::metadata(&path).ok();
        if meta.as_ref().is_none_or(|m| !m.is_dir()) {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&skill_file) else {
            continue;
        };
        let (frontmatter, body) = parse_frontmatter(&raw);
        let name = frontmatter.get("name").cloned().unwrap_or_else(|| {
            path.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let description = frontmatter
            .get("description")
            .cloned()
            .unwrap_or_default();
        let context = frontmatter.get("context").cloned();
        let policies = frontmatter
            .get("policies")
            .map(|p| {
                p.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let model = frontmatter
            .get("model")
            .cloned()
            .filter(|s| !s.is_empty());
        let directory = path.canonicalize().unwrap_or(path);
        skills.push(Skill {
            name,
            description,
            body,
            directory,
            context,
            policies,
            model,
        });
    }
    skills
}

/// One expanded slash skill ready for the agent loop or fork.
#[derive(Clone, Debug)]
pub struct SlashInvocation {
    pub skill_name: String,
    pub body: String,
    pub fork: bool,
}

/// Build the user message for a forked skill.
/// NomadWorks agent bodies rarely include `$ARGUMENTS`; append the request instead.
pub fn fork_user_body(skill: &Skill, arguments: &str) -> String {
    let body = substitute_arguments(&skill.body, arguments);
    if arguments.is_empty() {
        return body;
    }
    let has_placeholder = skill.body.contains("$ARGUMENTS") || skill.body.contains("$0");
    if has_placeholder {
        body
    } else {
        format!("{body}\n\n---\n\nUser request:\n{arguments}")
    }
}

/// Discover skills across all roots. Higher-precedence roots win on name collision
/// (stderr warning when `SKILLS_DEBUG` is set or always for collisions).
pub fn discover_skills() -> Vec<Skill> {
    let roots = collect_skill_roots();
    let debug = std::env::var("SKILLS_DEBUG").is_ok_and(|v| v != "0");
    if debug {
        let _ = writeln!(
            io::stderr(),
            "skills: scanning {} root(s)",
            roots.len()
        );
        for root in &roots {
            let _ = writeln!(io::stderr(), "  - {}", root.display());
        }
    }

    let mut by_name: HashMap<String, Skill> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for root in &roots {
        for skill in load_skills_from_dir(root) {
            if let Some(existing) = by_name.get(&skill.name) {
                let _ = writeln!(
                    io::stderr(),
                    "skills: ignoring duplicate '{}' from {} (keeping {})",
                    skill.name,
                    skill.directory.display(),
                    existing.directory.display()
                );
                continue;
            }
            order.push(skill.name.clone());
            by_name.insert(skill.name.clone(), skill);
        }
    }

    // P2: NomadWorks agent roles as forked skills (lower precedence than disk skills).
    for skill in crate::agents::discover_agent_skills() {
        if let Some(existing) = by_name.get(&skill.name) {
            let _ = writeln!(
                io::stderr(),
                "skills: ignoring NomadWorks agent '{}' (keeping {})",
                skill.name,
                existing.directory.display()
            );
            continue;
        }
        if debug {
            let _ = writeln!(
                io::stderr(),
                "skills: + agent fork `{}` (model={:?})",
                skill.name,
                skill.model
            );
        }
        order.push(skill.name.clone());
        by_name.insert(skill.name.clone(), skill);
    }

    order.sort();
    order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect()
}

pub fn skill_by_name<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

pub fn substitute_arguments(body: &str, arguments: &str) -> String {
    let parts: Vec<&str> = if arguments.is_empty() {
        Vec::new()
    } else {
        arguments.split_whitespace().collect()
    };

    // Indexed forms must run before bare $ARGUMENTS, or `$ARGUMENTS[0]` becomes `arg[0]`.
    let re = Regex::new(r"\$ARGUMENTS\[(\d+)\]").expect("valid regex");
    let mut result = re
        .replace_all(body, |caps: &regex::Captures| {
            let idx: usize = caps[1].parse().unwrap_or(usize::MAX);
            parts.get(idx).copied().unwrap_or("").to_string()
        })
        .into_owned();

    result = result.replace("$ARGUMENTS", arguments);

    for (idx, part) in parts.iter().enumerate() {
        result = result.replace(&format!("${idx}"), part);
    }
    result
}

pub fn format_skill_catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "You have access to the following skills:".to_string(),
        String::new(),
    ];
    for skill in skills {
        lines.push(format!("- {}: {}", skill.name, skill.description));
    }
    lines.extend([
        String::new(),
        "If a skill matches the user's request, call the Skill tool with its name".to_string(),
        "and follow the instructions it returns.".to_string(),
    ]);
    lines.join("\n")
}

fn cwd_relative(path: &Path) -> PathBuf {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.to_path_buf();
        }
    }
    path.to_path_buf()
}

pub fn wrap_skill_body_for_location(skill: &Skill, body: &str) -> String {
    if !body.contains("scripts/") {
        return body.to_string();
    }
    let rel = cwd_relative(&skill.directory);
    format!(
        "Skill: {} (located at {})\nPaths in the instructions below are relative to that folder.\n\n{}",
        skill.name,
        rel.display(),
        body
    )
}

/// Returns slash expansions, or `None` if not a slash prompt.
/// A forked skill ends the expansion run (Claude Code rule); it is returned alone with args.
pub fn expand_slash_invocation(prompt: &str, skills: &[Skill]) -> Option<Vec<SlashInvocation>> {
    let stripped = prompt.trim_start();
    if !stripped.starts_with('/') {
        return None;
    }

    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let mut expanded: Vec<String> = Vec::new();
    let mut arg_tokens: Vec<&str> = Vec::new();
    let mut expanding = true;
    let mut fork_only: Option<String> = None;

    for token in tokens {
        if !expanding {
            arg_tokens.push(token);
            continue;
        }
        if !token.starts_with('/') {
            expanding = false;
            arg_tokens.push(token);
            continue;
        }
        let skill_name = &token[1..];
        match skill_by_name(skills, skill_name) {
            None => {
                expanding = false;
                arg_tokens.push(token);
            }
            Some(skill) if skill.context.as_deref() == Some("fork") => {
                // Forked skill ends stacking; remaining tokens become its arguments.
                fork_only = Some(skill_name.to_string());
                expanding = false;
            }
            Some(_) => {
                expanded.push(skill_name.to_string());
            }
        }
    }

    if let Some(name) = fork_only {
        let skill = skill_by_name(skills, &name)?;
        let arguments = arg_tokens.join(" ");
        let body = fork_user_body(skill, &arguments);
        return Some(vec![SlashInvocation {
            skill_name: name,
            body,
            fork: true,
        }]);
    }

    if expanded.is_empty() {
        return None;
    }

    let arguments = arg_tokens.join(" ");
    let mut out = Vec::new();
    for name in &expanded {
        let Some(skill) = skill_by_name(skills, name) else {
            continue;
        };
        let mut body = substitute_arguments(&skill.body, &arguments);
        if body.contains("scripts/") {
            let rel = cwd_relative(&skill.directory);
            body = format!(
                "Skill: {} (located at {})\nPaths in the instructions below are relative to that folder.\n\n{}",
                skill.name,
                rel.display(),
                body
            );
        }
        out.push(SlashInvocation {
            skill_name: name.clone(),
            body,
            fork: false,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that touch cwd / env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn substitute_positional_and_full() {
        let out = substitute_arguments("a $0 b $1 c $ARGUMENTS", "x y");
        assert_eq!(out, "a x b y c x y");
    }

    #[test]
    fn substitute_indexed() {
        let out = substitute_arguments("$ARGUMENTS[1] $ARGUMENTS[0]", "first second");
        assert_eq!(out, "second first");
    }

    #[test]
    fn fork_slash_collects_remaining_args() {
        let skills = vec![Skill {
            name: "pma".into(),
            description: "pm".into(),
            body: "Role body. Args: $ARGUMENTS".into(),
            directory: PathBuf::from("/tmp/pma"),
            context: Some("fork".into()),
            policies: vec!["starworld".into()],
            model: Some("qwen3.6:latest".into()),
        }];
        let inv = expand_slash_invocation("/pma draft a brief", &skills).expect("slash");
        assert_eq!(inv.len(), 1);
        assert!(inv[0].fork);
        assert_eq!(inv[0].skill_name, "pma");
        assert!(
            inv[0].body.contains("draft a brief"),
            "fork should append args when body has no $ARGUMENTS: {}",
            inv[0].body
        );
    }

    #[test]
    fn multi_root_precedence_claude_over_opencode() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_dir("skills-prec");
        let claude = tmp.join(".claude").join("skills").join("alpha");
        let opencode = tmp.join(".opencode").join("skills").join("alpha");
        write_skill(&claude, "alpha", "from-claude");
        write_skill(&opencode, "alpha", "from-opencode");

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        // Clear extra paths so only walk-up applies.
        for key in EXTRA_PATH_ENV_KEYS {
            std::env::remove_var(key);
        }
        let skills = discover_skills();
        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&tmp);

        let alpha = skills.iter().find(|s| s.name == "alpha").expect("alpha");
        assert!(
            alpha.description.contains("from-claude"),
            "expected claude to win, got {}",
            alpha.description
        );
    }

    #[test]
    fn extra_path_env_loads_skills() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile_dir("skills-extra");
        let empty_cwd = tmp.join("empty-cwd");
        fs::create_dir_all(&empty_cwd).unwrap();
        let pack = tmp.join("pack").join("bravo");
        write_skill(&pack, "bravo", "from-extra");

        let prev = std::env::current_dir().unwrap();
        for key in EXTRA_PATH_ENV_KEYS {
            std::env::remove_var(key);
        }
        std::env::set_var("NOMAD_SKILLS_PATH", tmp.join("pack").display().to_string());
        std::env::set_current_dir(&empty_cwd).unwrap();
        let skills = discover_skills();
        std::env::set_current_dir(prev).unwrap();
        std::env::remove_var("NOMAD_SKILLS_PATH");
        let _ = fs::remove_dir_all(&tmp);

        assert!(
            skills.iter().any(|s| s.name == "bravo"),
            "expected bravo from NOMAD_SKILLS_PATH"
        );
    }

    fn tempfile_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claude-code-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str, description: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nbody\n"),
        )
        .unwrap();
    }
}
