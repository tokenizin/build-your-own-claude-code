//! OpenCode's permission rules, for the Rust agent's tools.
//!
//! An agent file's front matter (and the OpenCode config, globally) can say what a tool may do:
//!
//! ```yaml
//! permission:
//!   "*": allow
//!   bash:
//!     "*": ask
//!     "ls *": allow
//!   edit:
//!     "**/*.env*": deny
//! ```
//!
//! Each entry is a rule `(permission, pattern) -> allow | ask | deny`; the **last** rule that matches wins, as in
//! OpenCode (`*` matches anything, a trailing ` *` also matches the bare command). The rules in force are OpenCode's
//! defaults, then the config's, then the agent's. `ask` puts a request to the user (the host passes the answer back);
//! with nobody to ask it is refused. A call is checked as `(tool's permission, its pattern)`: `bash` by command,
//! `edit` (Edit, Write, apply_patch) by path, `read`, `glob`, `grep`, `webfetch` by URL, `task` by subagent, `skill`;
//! a file outside the working directory is also `external_directory`.

use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

impl Action {
    fn parse(word: &str) -> Option<Action> {
        match word.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase().as_str() {
            "allow" => Some(Action::Allow),
            "ask" => Some(Action::Ask),
            "deny" => Some(Action::Deny),
            _ => None,
        }
    }
    fn word(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Ask => "ask",
            Action::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub permission: String,
    pub pattern: String,
    pub action: Action,
    /// One of OpenCode's built-in defaults, not something the config or the agent wrote.
    pub builtin: bool,
}

/// OpenCode's wildcard: `*` any text, `?` one character, and a trailing ` *` also matches the bare command.
pub fn wildcard(text: &str, pattern: &str) -> bool {
    let text = text.replace('\\', "/");
    // OpenCode reads `**/*.env*` as needing a directory in front, so a secret at the project root would slip past a
    // `deny`; here a leading `**/` also matches no directory at all (stricter than OpenCode, on purpose).
    let (lead, pattern) = match pattern.strip_prefix("**/") {
        Some(rest) => ("(.*/)?".to_string(), rest),
        None => (String::new(), pattern),
    };
    let mut escaped = lead;
    for c in pattern.chars() {
        match c {
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    if let Some(stem) = escaped.strip_suffix(" .*") {
        escaped = format!("{stem}( .*)?");
    }
    Regex::new(&format!("(?s)^{escaped}$")).map(|re| re.is_match(&text)).unwrap_or(false)
}

/// The rules in force for one agent.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    rules: Vec<Rule>,
}

impl Policy {
    /// OpenCode's defaults: everything allowed, a file outside the project asks, reading `.env` files asks.
    pub fn defaults() -> Policy {
        let rule = |permission: &str, pattern: &str, action| Rule { permission: permission.into(), pattern: pattern.into(), action, builtin: true };
        Policy {
            rules: vec![
                rule("*", "*", Action::Allow),
                rule("external_directory", "*", Action::Ask),
                rule("read", "*", Action::Allow),
                rule("read", "*.env", Action::Ask),
                rule("read", "*.env.*", Action::Ask),
                rule("read", "*.env.example", Action::Allow),
            ],
        }
    }

    pub fn with(mut self, more: Vec<Rule>) -> Policy {
        self.rules.extend(more);
        self
    }

    /// The action for `(permission, pattern)` and the rule that decided it (the last that matches; ask when none does).
    pub fn evaluate(&self, permission: &str, pattern: &str) -> (Action, Option<&Rule>) {
        match self.rules.iter().rev().find(|r| wildcard(permission, &r.permission) && wildcard(pattern, &r.pattern)) {
            Some(rule) => (rule.action, Some(rule)),
            None => (Action::Ask, None),
        }
    }
}

/// Rules from an OpenCode `permission` object (`"allow"`, or `{ tool: "ask" | { pattern: action } }`).
pub fn rules_from_json(value: &Value) -> Vec<Rule> {
    let mut rules = Vec::new();
    if let Some(action) = value.as_str().and_then(Action::parse) {
        rules.push(Rule { permission: "*".into(), pattern: "*".into(), action, builtin: false });
        return rules;
    }
    let Some(map) = value.as_object() else { return rules };
    for (permission, entry) in map {
        if let Some(action) = entry.as_str().and_then(Action::parse) {
            rules.push(Rule { permission: permission.clone(), pattern: "*".into(), action, builtin: false });
        } else if let Some(patterns) = entry.as_object() {
            for (pattern, action) in patterns {
                if let Some(action) = action.as_str().and_then(Action::parse) {
                    rules.push(Rule { permission: permission.clone(), pattern: pattern.clone(), action, builtin: false });
                }
            }
        }
    }
    rules
}

/// Rules from the `permission:` block of an agent file's front matter, in the order written.
pub fn rules_from_front_matter(text: &str) -> Vec<Rule> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Vec::new();
    }
    let block: Vec<&str> = lines.take_while(|l| l.trim() != "---").collect();
    let entry = Regex::new(r#"^(\s*)("[^"]*"|'[^']*'|[^:\s][^:]*?)\s*:\s*(.*?)\s*$"#).unwrap();
    let unquote = |s: &str| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
    let mut rules = Vec::new();
    let mut inside = false;
    let (mut base_indent, mut section): (Option<usize>, Option<String>) = (None, None);
    for line in block {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some(caps) = entry.captures(line) else { continue };
        let indent = caps[1].len();
        let (key, value) = (unquote(&caps[2]), caps[3].to_string());
        if !inside {
            if indent == 0 && key == "permission" {
                inside = true;
                // `permission: allow` on one line.
                if let Some(action) = Action::parse(&value) {
                    rules.push(Rule { permission: "*".into(), pattern: "*".into(), action, builtin: false });
                    inside = false;
                }
            }
            continue;
        }
        if indent == 0 {
            break; // the next top-level key
        }
        let base = *base_indent.get_or_insert(indent);
        if indent <= base {
            match Action::parse(&value) {
                Some(action) => {
                    rules.push(Rule { permission: key, pattern: "*".into(), action, builtin: false });
                    section = None;
                }
                None => section = Some(key),
            }
        } else if let (Some(permission), Some(action)) = (&section, Action::parse(&value)) {
            rules.push(Rule { permission: permission.clone(), pattern: key, action, builtin: false });
        }
    }
    rules
}

/// The rules for the agent named `agent` (empty: no agent): defaults, then the OpenCode config's, then the agent's.
pub fn policy_for(agent: &str) -> Policy {
    let mut more: Vec<Rule> = Vec::new();
    let adapter = crate::opencode_config::load_adapter();
    if let Some(path) = crate::opencode_config::resolve_opencode_config_path(&adapter) {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str::<Value>(&crate::opencode_config::strip_jsonc(&raw)) {
                if let Some(permission) = config.get("permission") {
                    more.extend(rules_from_json(permission));
                }
                if let Some(permission) = config.get("agent").and_then(|a| a.get(agent)).and_then(|a| a.get("permission")) {
                    more.extend(rules_from_json(permission));
                }
            }
        }
    }
    if !agent.is_empty() && agent.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        for root in crate::agents::collect_nomadworks_roots() {
            if let Ok(text) = std::fs::read_to_string(root.join("agents").join(format!("{agent}.md"))) {
                more.extend(rules_from_front_matter(&text));
                break;
            }
        }
    }
    if std::env::var("NOMAD_PERMISSIONS_DEBUG").is_ok_and(|v| v != "0") {
        eprintln!("permissions: agent `{agent}`: {} rule(s) beyond the defaults", more.len());
    }
    Policy::defaults().with(more)
}

/// One thing a tool call needs permission for.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub permission: String,
    pub pattern: String,
    /// What "always" would allow from now on.
    pub always: String,
}

fn within(base: &Path, path: &Path) -> bool {
    let norm = |p: &Path| {
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    };
    norm(path).starts_with(norm(base))
}

/// What a tool call needs permission for (nothing for tools that have no permission).
pub fn requests_for(tool: &str, args: &Value, cwd: &Path) -> Vec<Request> {
    let text = |key: &str| args[key].as_str().unwrap_or("").to_string();
    let file_request = |permission: &str, raw: &str, out: &mut Vec<Request>| {
        let absolute = if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { cwd.join(raw) };
        let shown = absolute.strip_prefix(cwd).map(|p| p.display().to_string()).unwrap_or_else(|_| absolute.display().to_string());
        out.push(Request { permission: permission.into(), pattern: shown.clone(), always: shown });
        if !within(cwd, &absolute) {
            let dir = absolute.parent().map(|p| p.display().to_string()).unwrap_or_default();
            out.push(Request { permission: "external_directory".into(), pattern: format!("{dir}/*"), always: format!("{dir}/*") });
        }
    };
    let mut out = Vec::new();
    match tool {
        "Bash" => {
            let command = text("command");
            let first = command.split_whitespace().next().unwrap_or("").to_string();
            out.push(Request { permission: "bash".into(), always: format!("{first} *"), pattern: command });
        }
        "Edit" | "Write" => file_request("edit", &text("file_path"), &mut out),
        "apply_patch" => {
            for path in crate::patch::paths(&text("patchText")) {
                file_request("edit", &path, &mut out);
            }
        }
        "Read" => file_request("read", &text("file_path"), &mut out),
        "Glob" => out.push(Request { permission: "glob".into(), pattern: text("pattern"), always: "*".into() }),
        "Grep" => out.push(Request { permission: "grep".into(), pattern: text("pattern"), always: "*".into() }),
        "WebFetch" => out.push(Request { permission: "webfetch".into(), pattern: text("url"), always: "*".into() }),
        "Task" => out.push(Request { permission: "task".into(), pattern: text("subagent_type"), always: text("subagent_type") }),
        "Skill" => out.push(Request { permission: "skill".into(), pattern: text("name"), always: text("name") }),
        _ => {}
    }
    out
}

/// What the rules say about a call.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allow,
    /// A rule forbids it (the rule, for the model to read).
    Deny(Rule),
    /// These requests need the user.
    Ask(Vec<Request>),
}

/// Patterns the user allowed with "always", for this session (kept beside the task store, so the next message knows them).
static APPROVED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn approvals_file() -> Option<PathBuf> {
    crate::delegation::store_path().map(|dir| dir.join("permissions.json"))
}

fn load_approvals() {
    let Some(file) = approvals_file() else { return };
    let Ok(text) = std::fs::read_to_string(file) else { return };
    let Ok(list) = serde_json::from_str::<Vec<Value>>(&text) else { return };
    let mut approved = APPROVED.lock().unwrap();
    for entry in list {
        if let (Some(p), Some(pattern)) = (entry["permission"].as_str(), entry["pattern"].as_str()) {
            let pair = (p.to_string(), pattern.to_string());
            if !approved.contains(&pair) {
                approved.push(pair);
            }
        }
    }
}

#[cfg(test)]
pub fn forget_all() {
    APPROVED.lock().unwrap().clear();
}

pub fn remember(requests: &[Request]) {
    let mut approved = APPROVED.lock().unwrap();
    for r in requests {
        let pair = (r.permission.clone(), r.always.clone());
        if !approved.contains(&pair) {
            approved.push(pair);
        }
    }
    if let Some(file) = approvals_file() {
        let list: Vec<Value> = approved.iter().map(|(p, pattern)| serde_json::json!({ "permission": p, "pattern": pattern })).collect();
        let _ = std::fs::write(file, Value::Array(list).to_string());
    }
}

fn approved(request: &Request) -> bool {
    APPROVED.lock().unwrap().iter().any(|(p, pattern)| p == &request.permission && wildcard(&request.pattern, pattern))
}

/// Checks a call against the rules. Reads the approvals saved with the session first. With nobody to ask
/// (`attended` false) OpenCode's built-in `ask` defaults (a file outside the project, `.env` files) are not enforced,
/// so a plain command-line run is not stopped by them; an `ask` the config or the agent wrote still is.
pub fn check(policy: &Policy, attended: bool, requests: &[Request]) -> Verdict {
    load_approvals();
    let mut asks = Vec::new();
    for request in requests {
        match policy.evaluate(&request.permission, &request.pattern) {
            (Action::Deny, Some(rule)) => return Verdict::Deny(rule.clone()),
            (Action::Ask, Some(rule)) if rule.builtin && !attended => {}
            (Action::Ask, _) if !approved(request) => asks.push(request.clone()),
            _ => {}
        }
    }
    if asks.is_empty() { Verdict::Allow } else { Verdict::Ask(asks) }
}

/// What the model is told when a rule forbids a call (OpenCode's wording).
pub fn denied_text(rule: &Rule) -> String {
    format!(
        "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules [{{\"permission\":\"{}\",\"pattern\":\"{}\",\"action\":\"{}\"}}]",
        rule.permission, rule.pattern, rule.action.word()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CAO: &str = "---\nname: cao\npermission:\n  bash:\n    \"*\": ask\n    \"ls *\": allow\n    \"git status\": allow\n  edit:\n    \"**/*.env*\": deny\n    \"**/*.key\": deny\n  webfetch: deny\ntools:\n  \"*\": true\n---\nbody\n";

    #[test]
    fn wildcards_follow_opencode() {
        assert!(wildcard("ls", "ls *") && wildcard("ls -la src", "ls *") && !wildcard("lsof", "ls *"));
        assert!(wildcard("a/b/.env.local", "**/*.env*") && wildcard("x.env", "**/*.env*"), "a leading `**/` also matches the project root");
        assert!(!wildcard("notes.txt", "**/*.env*") && wildcard("a\\b", "a/b"));
        assert!(wildcard("anything at all", "*") && wildcard("x.env", "*.env") && !wildcard("x.env.local", "*.env"));
        assert!(wildcard("a.b", "a.b") && !wildcard("axb", "a.b"), "dots are literal");
    }

    #[test]
    fn the_last_matching_rule_wins_and_no_match_asks() {
        let rules = rules_from_front_matter(CAO);
        assert_eq!(rules.len(), 6);
        let policy = Policy::defaults().with(rules);
        let action = |permission: &str, pattern: &str| policy.evaluate(permission, pattern).0;
        assert_eq!(action("bash", "ls -la"), Action::Allow);
        assert_eq!(action("bash", "git status"), Action::Allow);
        assert_eq!(action("bash", "git push"), Action::Ask, "the catch-all `*: ask` before the specific rules");
        assert_eq!(action("bash", "rm -rf /"), Action::Ask);
        assert_eq!(action("edit", "config/prod.env"), Action::Deny);
        assert_eq!(action("edit", "src/app.ts"), Action::Allow, "the default `*: allow`");
        assert_eq!(action("webfetch", "https://example.com"), Action::Deny);
        assert_eq!(action("read", "app/.env"), Action::Ask, "OpenCode's default for secrets");
        assert_eq!(action("read", ".env.example"), Action::Allow);
        assert_eq!(Policy::default().evaluate("bash", "ls").0, Action::Ask, "no rule at all asks");
        // A one-line `permission: allow`, and an agent with none.
        assert_eq!(rules_from_front_matter("---\npermission: allow\n---\n").len(), 1);
        assert!(rules_from_front_matter("---\nname: x\n---\n").is_empty() && rules_from_front_matter("no front matter").is_empty());
        // The config's object form.
        let config = rules_from_json(&json!({ "edit": "allow", "bash": { "*": "ask" } }));
        assert_eq!(config.len(), 2);
    }

    #[test]
    fn tool_calls_map_to_permissions_and_patterns() {
        let cwd = Path::new("/work");
        let one = |tool: &str, args: Value| requests_for(tool, &args, cwd);
        assert_eq!(one("Bash", json!({ "command": "git push origin main" })), [Request { permission: "bash".into(), pattern: "git push origin main".into(), always: "git *".into() }]);
        assert_eq!(one("Edit", json!({ "file_path": "/work/src/a.rs" }))[0].pattern, "src/a.rs");
        assert_eq!(one("Write", json!({ "file_path": "notes.md" }))[0].permission, "edit");
        let outside = one("Write", json!({ "file_path": "/etc/hosts" }));
        assert_eq!(outside.iter().map(|r| r.permission.as_str()).collect::<Vec<_>>(), ["edit", "external_directory"]);
        assert_eq!(outside[1].pattern, "/etc/*");
        assert!(one("Write", json!({ "file_path": "/work/../other/x" })).len() == 2, "`..` leaves the project");
        assert_eq!(one("apply_patch", json!({ "patchText": "*** Begin Patch\n*** Add File: a.txt\n+x\n*** Delete File: b.txt\n*** End Patch" })).len(), 2);
        assert_eq!(one("Task", json!({ "subagent_type": "qa_engineer" }))[0].permission, "task");
        assert!(one("TodoWrite", json!({})).is_empty() && one("nomadflow_run_workflow", json!({})).is_empty());
    }

    #[test]
    fn a_verdict_is_deny_then_ask_then_allow() {
        let policy = Policy::defaults().with(rules_from_front_matter(CAO));
        let req = |tool: &str, args: Value| requests_for(tool, &args, Path::new("/work"));
        assert!(matches!(check(&policy, true, &req("Edit", json!({ "file_path": "/work/x.key" }))), Verdict::Deny(rule) if rule.pattern == "**/*.key"));
        assert!(matches!(check(&policy, true, &req("Bash", json!({ "command": "rm x" }))), Verdict::Ask(asked) if asked.len() == 1));
        assert_eq!(check(&policy, true, &req("Bash", json!({ "command": "ls" }))), Verdict::Allow);
        // A deny beats an ask in the same call.
        let both = [req("Bash", json!({ "command": "rm x" })), req("Edit", json!({ "file_path": "/work/s.key" }))].concat();
        assert!(matches!(check(&policy, true, &both), Verdict::Deny(_)));
        // With nobody to ask, OpenCode's built-in asks are not enforced; the agent's own are.
        let outside = req("Read", json!({ "file_path": "/etc/hosts" }));
        assert!(matches!(check(&policy, true, &outside), Verdict::Ask(_)));
        assert_eq!(check(&policy, false, &outside), Verdict::Allow);
        assert!(matches!(check(&policy, false, &req("Bash", json!({ "command": "rm x" }))), Verdict::Ask(_)), "`bash: ask` is the agent's own rule");
        // What the model reads, in OpenCode's words.
        assert!(denied_text(&Rule { permission: "edit".into(), pattern: "**/*.key".into(), action: Action::Deny, builtin: false }).starts_with("The user has specified a rule which prevents you from using this specific tool call."));
    }
}
