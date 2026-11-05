//! The handover an orchestrator (a `nomadflow_run_workflow` session) files when it
//! finishes: what was asked, what came of it, with what evidence, and what is left.
//!
//! The Product Manager evaluates the handover and either accepts it or sends the
//! workflow back (by resuming the same `session_id`). The engine does not judge the
//! work; it makes sure the handover is complete and consistent with the repository's
//! own success rules (no task is successful with unmet requirements, unverified
//! work or open discrepancies), so a "completed" handover can be taken at its word.
//!
//! A handover is a Markdown file with a front matter block, kept with the task:
//! `<task folder>/HANDOVER.md`, or `<stem>.HANDOVER.md` next to a bare task file.

use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Partial,
    Blocked,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Partial => "partial",
            Outcome::Blocked => "blocked",
            Outcome::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<Outcome> {
        match text.trim().to_lowercase().as_str() {
            "completed" | "complete" | "done" | "success" => Some(Outcome::Completed),
            "partial" => Some(Outcome::Partial),
            "blocked" => Some(Outcome::Blocked),
            "failed" | "failure" => Some(Outcome::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Requirement {
    pub id: String,
    pub text: String,
    /// met | partial | unmet | waived
    pub status: String,
    pub evidence: String,
}

#[derive(Clone, Debug)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    pub owner: String,
    /// done | failed | skipped | pending
    pub status: String,
    pub result: String,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Evidence {
    /// file | command | test | note
    pub kind: String,
    pub reference: String,
    pub result: String,
}

#[derive(Clone, Debug)]
pub struct Handover {
    pub outcome: Outcome,
    pub objective: String,
    pub summary: String,
    pub requirements: Vec<Requirement>,
    pub plan: Vec<PlanItem>,
    pub evidence: Vec<Evidence>,
    pub open_issues: Vec<String>,
    pub next_actions: Vec<String>,
    /// verified | unverified | failed
    pub verification: String,
    pub verification_notes: String,
}

// ---------------------------------------------------------------- parsing + validation

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().map(|s| s.trim().to_string()).unwrap_or_default()
}

/// An array, whether the model sent it as an array or as a JSON string of one.
fn list(v: &Value, key: &str) -> Vec<Value> {
    match &v[key] {
        Value::Array(items) => items.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw.trim()) {
            Ok(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    list(v, key)
        .into_iter()
        .filter_map(|item| item.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn one_of(value: &str, allowed: &[&str]) -> Option<String> {
    let lower = value.trim().to_lowercase();
    allowed.contains(&lower.as_str()).then_some(lower)
}

impl Handover {
    /// Parses and checks the `handover` tool's arguments. All problems are
    /// reported together, so the model can fix them in one go.
    pub fn from_args(args: &Value) -> Result<Handover, Vec<String>> {
        let mut errors: Vec<String> = Vec::new();

        let outcome = match Outcome::parse(args["outcome"].as_str().unwrap_or("")) {
            Some(o) => o,
            None => {
                errors.push("outcome must be one of: completed, partial, blocked, failed".into());
                Outcome::Failed
            }
        };
        let objective = text(args, "objective");
        if objective.is_empty() {
            errors.push("objective is required: what the task set out to achieve".into());
        }
        let summary = text(args, "summary");
        if summary.is_empty() {
            errors.push("summary is required: a short statement of what came of the work".into());
        }

        let mut requirements = Vec::new();
        for (i, item) in list(args, "requirements").iter().enumerate() {
            let label = format!("requirements[{i}]");
            let id = text(item, "id");
            let body = text(item, "text");
            if id.is_empty() || body.is_empty() {
                errors.push(format!("{label} needs an id and a text"));
                continue;
            }
            let Some(status) = one_of(item["status"].as_str().unwrap_or(""), &["met", "partial", "unmet", "waived"]) else {
                errors.push(format!("{label} ({id}): status must be one of: met, partial, unmet, waived"));
                continue;
            };
            requirements.push(Requirement { id, text: body, status, evidence: text(item, "evidence") });
        }
        if let Some(dup) = first_duplicate(requirements.iter().map(|r| r.id.as_str())) {
            errors.push(format!("requirements: id `{dup}` is used twice"));
        }

        let mut plan = Vec::new();
        for (i, item) in list(args, "plan").iter().enumerate() {
            let label = format!("plan[{i}]");
            let id = text(item, "id");
            let title = text(item, "title");
            if id.is_empty() || title.is_empty() {
                errors.push(format!("{label} needs an id and a title"));
                continue;
            }
            let Some(status) = one_of(item["status"].as_str().unwrap_or(""), &["done", "failed", "skipped", "pending"]) else {
                errors.push(format!("{label} ({id}): status must be one of: done, failed, skipped, pending"));
                continue;
            };
            plan.push(PlanItem {
                id,
                title,
                owner: text(item, "owner"),
                status,
                result: text(item, "result"),
                depends_on: strings(item, "depends_on"),
            });
        }
        if let Some(dup) = first_duplicate(plan.iter().map(|p| p.id.as_str())) {
            errors.push(format!("plan: id `{dup}` is used twice"));
        }
        for item in &plan {
            for dep in &item.depends_on {
                if dep == &item.id {
                    errors.push(format!("plan item `{}` depends on itself", item.id));
                } else if !plan.iter().any(|p| &p.id == dep) {
                    errors.push(format!("plan item `{}` depends on `{dep}`, which is not in the plan", item.id));
                }
            }
        }

        let mut evidence = Vec::new();
        for (i, item) in list(args, "evidence").iter().enumerate() {
            let reference = text(item, "ref");
            let reference = if reference.is_empty() { text(item, "reference") } else { reference };
            let Some(kind) = one_of(item["kind"].as_str().unwrap_or(""), &["file", "command", "test", "note"]) else {
                errors.push(format!("evidence[{i}]: kind must be one of: file, command, test, note"));
                continue;
            };
            if reference.is_empty() {
                errors.push(format!("evidence[{i}] needs a ref (the path, command or note it points to)"));
                continue;
            }
            evidence.push(Evidence { kind, reference, result: text(item, "result") });
        }

        let verification_obj = &args["verification"];
        let verification_raw = if verification_obj.is_string() {
            verification_obj.as_str().unwrap_or("").to_string()
        } else {
            text(verification_obj, "status")
        };
        let verification = match one_of(&verification_raw, &["verified", "unverified", "failed"]) {
            Some(v) => v,
            None => {
                errors.push("verification.status must be one of: verified, unverified, failed".into());
                "unverified".into()
            }
        };
        let verification_notes = text(verification_obj, "notes");

        let handover = Handover {
            outcome,
            objective,
            summary,
            requirements,
            plan,
            evidence,
            open_issues: strings(args, "open_issues"),
            next_actions: strings(args, "next_actions"),
            verification,
            verification_notes,
        };
        errors.extend(handover.consistency_errors());
        if errors.is_empty() { Ok(handover) } else { Err(errors) }
    }

    /// The repository's success rules, applied to what the handover claims.
    fn consistency_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        match self.outcome {
            Outcome::Completed => {
                if self.requirements.is_empty() {
                    errors.push("a completed handover must list the requirements it was held to (requirements is empty)".into());
                }
                for r in &self.requirements {
                    match r.status.as_str() {
                        "met" => {}
                        "waived" if !r.evidence.is_empty() => {}
                        "waived" => errors.push(format!("requirement `{}` is waived without a reason (put it in evidence)", r.id)),
                        other => errors.push(format!("outcome is completed but requirement `{}` is {other}", r.id)),
                    }
                }
                for p in &self.plan {
                    if !matches!(p.status.as_str(), "done" | "skipped") {
                        errors.push(format!("outcome is completed but plan item `{}` is {}", p.id, p.status));
                    }
                }
                if self.verification != "verified" {
                    errors.push(format!(
                        "outcome is completed but verification is {}: no task is successful until builds and tests pass; verify it, or use partial/blocked",
                        self.verification
                    ));
                }
                if self.evidence.is_empty() {
                    errors.push("a completed handover must point at its evidence (evidence is empty)".into());
                }
                if !self.open_issues.is_empty() {
                    errors.push(
                        "outcome is completed but open_issues is not empty: discrepancies must be fixed, not listed; fix them, or use partial".into(),
                    );
                }
            }
            Outcome::Partial | Outcome::Blocked | Outcome::Failed => {
                if self.open_issues.is_empty() {
                    errors.push(format!("a {} handover must say why in open_issues", self.outcome.as_str()));
                }
                if matches!(self.outcome, Outcome::Blocked | Outcome::Failed) && self.next_actions.is_empty() {
                    errors.push(format!("a {} handover must say what has to happen next (next_actions)", self.outcome.as_str()));
                }
            }
        }
        errors
    }
}

fn first_duplicate<'a>(ids: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter().find(|id| !seen.insert(*id))
}

// ---------------------------------------------------------------- time

/// `YYYY-MM-DDTHH:MM:SSZ` for seconds since the Unix epoch (no time crate needed).
pub fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_utc(secs)
}

// ---------------------------------------------------------------- rendering

pub struct Meta<'a> {
    pub task_path: &'a str,
    pub task_id: &'a str,
    pub session_id: &'a str,
    pub agent: &'a str,
    pub revision: u32,
    pub filed_at: &'a str,
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        "_None._\n".into()
    } else {
        items.iter().map(|i| format!("- {}\n", i.replace('\n', "\n  "))).collect()
    }
}

const REVIEW_HEADING: &str = "## Review log";

impl Handover {
    /// The handover as the file the Product Manager reads. `review_log` is carried over from earlier revisions.
    pub fn render(&self, meta: &Meta<'_>, review_log: &str) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("task: {}\n", meta.task_path));
        out.push_str(&format!("task_id: {}\n", meta.task_id));
        out.push_str(&format!("session_id: {}\n", meta.session_id));
        out.push_str(&format!("agent: {}\n", meta.agent));
        out.push_str(&format!("outcome: {}\n", self.outcome.as_str()));
        out.push_str(&format!("verification: {}\n", self.verification));
        out.push_str(&format!("revision: {}\n", meta.revision));
        out.push_str(&format!("filed_at: {}\n", meta.filed_at));
        out.push_str("---\n\n");
        out.push_str(&format!("# Handover: {}\n\n", meta.task_id));
        out.push_str(&format!("**Outcome: {}** · verification: {}\n\n", self.outcome.as_str(), self.verification));
        out.push_str(&format!("## Summary\n\n{}\n\n", self.summary));
        out.push_str(&format!("## Objective\n\n{}\n\n", self.objective));

        out.push_str("## Requirements\n\n");
        if self.requirements.is_empty() {
            out.push_str("_None listed._\n\n");
        } else {
            out.push_str("| Id | Requirement | Status | Evidence |\n|---|---|---|---|\n");
            for r in &self.requirements {
                out.push_str(&format!("| {} | {} | {} | {} |\n", cell(&r.id), cell(&r.text), r.status, cell(&r.evidence)));
            }
            out.push('\n');
        }

        out.push_str("## Plan and results\n\n");
        if self.plan.is_empty() {
            out.push_str("_None listed._\n\n");
        } else {
            out.push_str("| Id | Step | Owner | Status | Depends on | Result |\n|---|---|---|---|---|---|\n");
            for p in &self.plan {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    cell(&p.id),
                    cell(&p.title),
                    cell(&p.owner),
                    p.status,
                    cell(&p.depends_on.join(", ")),
                    cell(&p.result)
                ));
            }
            out.push('\n');
        }

        out.push_str("## Evidence\n\n");
        if self.evidence.is_empty() {
            out.push_str("_None listed._\n\n");
        } else {
            for e in &self.evidence {
                let result = if e.result.is_empty() { String::new() } else { format!(" — {}", e.result) };
                out.push_str(&format!("- **{}** `{}`{result}\n", e.kind, e.reference));
            }
            out.push('\n');
        }

        out.push_str(&format!("## Open issues\n\n{}\n", bullets(&self.open_issues)));
        out.push_str(&format!("## Next actions\n\n{}\n", bullets(&self.next_actions)));
        let notes = if self.verification_notes.is_empty() { String::new() } else { format!("\n\n{}", self.verification_notes) };
        out.push_str(&format!("## Verification\n\n{}{notes}\n\n", self.verification));
        out.push_str(REVIEW_HEADING);
        out.push_str("\n\n");
        let carried = review_log.trim();
        if carried.is_empty() {
            out.push_str("_No review yet._\n");
        } else {
            out.push_str(carried);
            out.push('\n');
        }
        out
    }
}

/// The review log section of an existing handover file (without its heading or placeholder).
pub fn review_log_of(file: &str) -> String {
    let Some(start) = file.find(REVIEW_HEADING) else { return String::new() };
    let body = file[start + REVIEW_HEADING.len()..].trim();
    if body == "_No review yet._" { String::new() } else { body.to_string() }
}

/// The front matter value for `key` in an existing handover file.
pub fn front_matter_value(file: &str, key: &str) -> Option<String> {
    let rest = file.strip_prefix("---\n")?;
    let block = rest.split("\n---").next()?;
    block.lines().find_map(|l| l.strip_prefix(&format!("{key}: "))).map(|v| v.trim().to_string())
}

// ---------------------------------------------------------------- where it lives

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` resolved against the worktree, refused if it leaves it.
pub fn inside_worktree(worktree: &Path, path: &str) -> Result<PathBuf, String> {
    let given = Path::new(path.trim());
    let joined = if given.is_absolute() { given.to_path_buf() } else { worktree.join(given) };
    let normal = lexical_normalize(&joined);
    let root = lexical_normalize(worktree);
    if normal.starts_with(&root) {
        Ok(normal)
    } else {
        Err(format!("`{path}` is outside the worktree"))
    }
}

fn walk_for(dir: &Path, name: &std::ffi::OsStr, depth: u8) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name() == Some(name) {
            return Some(path);
        }
        if depth > 0 && path.is_dir() {
            subdirs.push(path);
        }
    }
    subdirs.into_iter().find_map(|d| walk_for(&d, name, depth - 1))
}

/// The task file: where it was said to be, else — the Workflow Runner moves a
/// finished task folder to `tasks/done/` — wherever that file name is now.
pub fn locate_task_file(worktree: &Path, task_path: &str) -> Option<PathBuf> {
    let given = inside_worktree(worktree, task_path).ok()?;
    if given.is_file() {
        return Some(given);
    }
    let name = given.file_name()?;
    ["tasks/done", "tasks/todo", "tasks"].iter().find_map(|dir| walk_for(&worktree.join(dir), name, 3))
}

fn safe_stem(task_file: &Path) -> String {
    let stem = task_file.file_stem().and_then(|s| s.to_str()).unwrap_or("task");
    let cleaned: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    if cleaned.is_empty() { "task".into() } else { cleaned }
}

/// The task's id for display: its file stem.
pub fn task_id_of(task_path: &str) -> String {
    safe_stem(Path::new(task_path))
}

/// Where the handover goes: in the task's own folder when it has one (the folder moves
/// with the task), else beside the task file; if the task cannot be found, `tasks/handovers/`.
pub fn handover_path(worktree: &Path, task_path: &str) -> PathBuf {
    match locate_task_file(worktree, task_path) {
        Some(file) => {
            let dir = file.parent().unwrap_or(worktree);
            let folder_per_task = dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("TASK"));
            if folder_per_task {
                dir.join("HANDOVER.md")
            } else {
                dir.join(format!("{}.HANDOVER.md", safe_stem(&file)))
            }
        }
        None => worktree.join("tasks").join("handovers").join(format!("{}.HANDOVER.md", safe_stem(Path::new(task_path)))),
    }
}

// ---------------------------------------------------------------- filing

pub struct Filed {
    /// Relative to the worktree when it is inside it.
    pub display_path: String,
    pub revision: u32,
    pub outcome: Outcome,
    pub verification: String,
    pub open_issues: usize,
    pub markdown: String,
    /// Things worth a second look that do not make the handover wrong.
    pub warnings: Vec<String>,
}

/// Writes the handover (revision + 1 of whatever is there, keeping its review log).
pub fn file(worktree: &Path, task_path: &str, session_id: &str, agent: &str, handover: &Handover) -> Result<Filed, String> {
    let path = handover_path(worktree, task_path);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let revision = front_matter_value(&existing, "revision").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0) + 1;
    let task_id = task_id_of(task_path);
    let meta = Meta { task_path, task_id: &task_id, session_id, agent, revision, filed_at: &now_iso() };
    let markdown = handover.render(&meta, &review_log_of(&existing));

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let key = path.to_string_lossy().to_string();
    let written = crate::file_lease::with_lease(&key, &format!("handover-{session_id}"), || {
        let tmp = path.with_extension(format!("md.{}.tmp", std::process::id()));
        fs::write(&tmp, &markdown).and_then(|_| fs::rename(&tmp, &path))
    });
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("cannot write {}: {e}", path.display())),
        Err(lease) => return Err(lease.to_string()),
    }

    let mut warnings = Vec::new();
    if handover.outcome == Outcome::Completed {
        match locate_task_file(worktree, task_path).and_then(|f| fs::read_to_string(f).ok()) {
            Some(body) if body.contains("Post Implementation Task Updates") => {}
            Some(_) => warnings.push(
                "the task file has no `# Post Implementation Task Updates` section; every subagent is required to add one".into(),
            ),
            None => warnings.push(format!("the task file `{task_path}` could not be found, so its Post Implementation Task Updates were not checked")),
        }
    }
    let display_path = path.strip_prefix(worktree).map(|p| p.display().to_string()).unwrap_or_else(|_| path.display().to_string());
    Ok(Filed {
        display_path,
        revision,
        outcome: handover.outcome,
        verification: handover.verification.clone(),
        open_issues: handover.open_issues.len(),
        markdown,
        warnings,
    })
}

/// Records that the Product Manager sent the workflow back: the reason goes in the file's review log.
pub fn record_bounce(path: &Path, session_id: &str, instructions: &str) -> Result<(), String> {
    let existing = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let revision = front_matter_value(&existing, "revision").unwrap_or_else(|| "?".into());
    let reason = instructions.split_whitespace().collect::<Vec<_>>().join(" ");
    let reason: String = if reason.chars().count() > 400 { format!("{}…", reason.chars().take(399).collect::<String>()) } else { reason };
    let entry = format!("- {} · sent back (revision {revision}, session {session_id}): {reason}", now_iso());
    let log = review_log_of(&existing);
    let new_log = if log.is_empty() { entry } else { format!("{log}\n{entry}") };
    let head = match existing.find(REVIEW_HEADING) {
        Some(i) => &existing[..i],
        None => existing.as_str(),
    };
    fs::write(path, format!("{head}{REVIEW_HEADING}\n\n{new_log}\n")).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// ---------------------------------------------------------------- the tool

pub fn tool_spec() -> Value {
    let status = |values: &[&str]| json!({ "type": "string", "enum": values });
    json!({
        "type": "function",
        "function": {
            "name": "handover",
            "description": "File the handover for this workflow. Call it once when the lifecycle is finished or you cannot continue; the Product Manager evaluates it. \
Rules: outcome `completed` only if every requirement is met (or waived with a reason), every plan item is done or skipped, verification is `verified`, evidence is listed and open_issues is empty (fix discrepancies, do not list them). Otherwise use `partial`, `blocked` or `failed`, and say why in open_issues (and, for blocked/failed, what must happen next in next_actions). A handover that breaks these rules is rejected with the reasons; fix it and call again.",
            "parameters": {
                "type": "object",
                "required": ["outcome", "objective", "summary", "requirements", "plan", "evidence", "verification"],
                "properties": {
                    "outcome": status(&["completed", "partial", "blocked", "failed"]),
                    "objective": { "type": "string", "description": "What the task set out to achieve" },
                    "summary": { "type": "string", "description": "What came of the work, in a few sentences" },
                    "requirements": {
                        "type": "array",
                        "description": "The requirements/acceptance criteria the task was held to, and how each came out",
                        "items": {
                            "type": "object",
                            "required": ["id", "text", "status"],
                            "properties": {
                                "id": { "type": "string" },
                                "text": { "type": "string" },
                                "status": status(&["met", "partial", "unmet", "waived"]),
                                "evidence": { "type": "string", "description": "How it was shown to be met, or why it was waived" }
                            }
                        }
                    },
                    "plan": {
                        "type": "array",
                        "description": "The steps that were planned and delegated, with owner, dependencies and result",
                        "items": {
                            "type": "object",
                            "required": ["id", "title", "status"],
                            "properties": {
                                "id": { "type": "string" },
                                "title": { "type": "string" },
                                "owner": { "type": "string", "description": "The agent that did it" },
                                "status": status(&["done", "failed", "skipped", "pending"]),
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "result": { "type": "string" }
                            }
                        }
                    },
                    "evidence": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["kind", "ref"],
                            "properties": {
                                "kind": status(&["file", "command", "test", "note"]),
                                "ref": { "type": "string", "description": "The path, command or note" },
                                "result": { "type": "string" }
                            }
                        }
                    },
                    "open_issues": { "type": "array", "items": { "type": "string" }, "description": "What is still wrong or unfinished" },
                    "next_actions": { "type": "array", "items": { "type": "string" } },
                    "verification": {
                        "type": "object",
                        "required": ["status"],
                        "properties": {
                            "status": status(&["verified", "unverified", "failed"]),
                            "notes": { "type": "string", "description": "What was run (builds, tests) and what it showed" }
                        }
                    }
                }
            }
        }
    })
}

/// Adds the aliases models use for the handover tool's argument names.
pub fn apply_aliases(args: Value) -> Value {
    let Value::Object(mut map) = args else { return json!({}) };
    for (target, alternatives) in [
        ("open_issues", &["openIssues", "issues", "discrepancies"][..]),
        ("next_actions", &["nextActions", "next_steps", "nextSteps"][..]),
        ("requirements", &["specification", "acceptance_criteria"][..]),
    ] {
        if !map.contains_key(target) {
            if let Some(v) = alternatives.iter().find_map(|a| map.get(*a).cloned()) {
                map.insert(target.into(), v);
            }
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({
            "outcome": "completed",
            "objective": "Add the export button",
            "summary": "Button added and tested.",
            "requirements": [
                { "id": "R1", "text": "Button exports CSV", "status": "met", "evidence": "test export.spec" },
                { "id": "R2", "text": "Works offline", "status": "waived", "evidence": "PO agreed: out of scope" }
            ],
            "plan": [
                { "id": "P1", "title": "Spec", "owner": "business_analyst", "status": "done", "result": "spec in task file" },
                { "id": "P2", "title": "Build", "owner": "developer", "status": "done", "depends_on": ["P1"], "result": "merged" }
            ],
            "evidence": [
                { "kind": "test", "ref": "npm test", "result": "58 passed, 0 failed" },
                { "kind": "file", "ref": "src/export.ts" }
            ],
            "open_issues": [],
            "next_actions": [],
            "verification": { "status": "verified", "notes": "build + tests green" }
        })
    }

    fn errors_of(args: Value) -> Vec<String> {
        Handover::from_args(&args).err().unwrap_or_default()
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("handover-{name}-{}-{}", std::process::id(), crate::delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_complete_handover_is_accepted() {
        let h = Handover::from_args(&good()).expect("valid");
        assert_eq!(h.outcome, Outcome::Completed);
        assert_eq!(h.requirements.len(), 2);
        assert_eq!(h.plan[1].depends_on, vec!["P1"]);
    }

    #[test]
    fn completed_is_held_to_the_repositorys_success_rules() {
        let mut a = good();
        a["requirements"][0]["status"] = json!("unmet");
        assert!(errors_of(a).iter().any(|e| e.contains("requirement `R1` is unmet")));

        let mut a = good();
        a["plan"][1]["status"] = json!("pending");
        assert!(errors_of(a).iter().any(|e| e.contains("plan item `P2` is pending")));

        let mut a = good();
        a["verification"]["status"] = json!("unverified");
        assert!(errors_of(a).iter().any(|e| e.contains("no task is successful until builds and tests pass")));

        let mut a = good();
        a["evidence"] = json!([]);
        assert!(errors_of(a).iter().any(|e| e.contains("point at its evidence")));

        let mut a = good();
        a["open_issues"] = json!(["flaky test"]);
        assert!(errors_of(a).iter().any(|e| e.contains("discrepancies must be fixed")));

        let mut a = good();
        a["requirements"][1]["evidence"] = json!("");
        assert!(errors_of(a).iter().any(|e| e.contains("waived without a reason")));

        let mut a = good();
        a["requirements"] = json!([]);
        assert!(errors_of(a).iter().any(|e| e.contains("requirements it was held to")));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let errors = errors_of(json!({ "outcome": "nope" }));
        assert!(errors.len() >= 4, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("outcome must be one of")));
        assert!(errors.iter().any(|e| e.contains("objective is required")));
        assert!(errors.iter().any(|e| e.contains("summary is required")));
        assert!(errors.iter().any(|e| e.contains("verification.status")));
    }

    #[test]
    fn partial_blocked_and_failed_must_explain_themselves() {
        let mut a = good();
        a["outcome"] = json!("partial");
        a["requirements"][0]["status"] = json!("partial");
        assert!(errors_of(a.clone()).iter().any(|e| e.contains("must say why in open_issues")));
        a["open_issues"] = json!(["CSV escaping not done"]);
        assert!(Handover::from_args(&a).is_ok(), "partial with a reason is fine");

        a["outcome"] = json!("blocked");
        assert!(errors_of(a.clone()).iter().any(|e| e.contains("what has to happen next")));
        a["next_actions"] = json!(["PO to decide on escaping"]);
        assert!(Handover::from_args(&a).is_ok());
    }

    #[test]
    fn plan_references_are_checked() {
        let mut a = good();
        a["plan"][1]["depends_on"] = json!(["P9"]);
        assert!(errors_of(a).iter().any(|e| e.contains("`P9`, which is not in the plan")));
        let mut a = good();
        a["plan"][0]["depends_on"] = json!(["P1"]);
        assert!(errors_of(a).iter().any(|e| e.contains("depends on itself")));
        let mut a = good();
        a["plan"][1]["id"] = json!("P1");
        assert!(errors_of(a).iter().any(|e| e.contains("id `P1` is used twice")));
    }

    #[test]
    fn weak_models_nested_values_are_tolerated() {
        // Arrays sent as JSON strings, status in other case, verification as a bare word, aliased names.
        let mut a = good();
        a["requirements"] = json!(good()["requirements"].to_string());
        a["verification"] = json!("Verified");
        a["outcome"] = json!("Completed");
        let h = Handover::from_args(&apply_aliases(a)).expect("tolerated");
        assert_eq!(h.requirements.len(), 2);
        assert_eq!(h.verification, "verified");
        let aliased = apply_aliases(json!({ "discrepancies": ["x"], "nextSteps": ["y"] }));
        assert_eq!(aliased["open_issues"], json!(["x"]));
        assert_eq!(aliased["next_actions"], json!(["y"]));
    }

    #[test]
    fn iso_time() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn rendered_file_has_front_matter_tables_and_escapes_pipes() {
        let mut a = good();
        a["requirements"][0]["text"] = json!("Exports a|b\nas CSV");
        let h = Handover::from_args(&a).unwrap();
        let md = h.render(&Meta { task_path: "tasks/todo/TASK-1/TASK-1.md", task_id: "TASK-1", session_id: "wf_1", agent: "workflow_runner", revision: 3, filed_at: "2026-11-05T00:00:00Z" }, "");
        assert!(md.starts_with("---\ntask: tasks/todo/TASK-1/TASK-1.md\n"));
        assert_eq!(front_matter_value(&md, "revision").as_deref(), Some("3"));
        assert_eq!(front_matter_value(&md, "outcome").as_deref(), Some("completed"));
        assert!(md.contains("| R1 | Exports a\\|b as CSV | met |"), "{md}");
        assert!(md.contains("- **test** `npm test` — 58 passed, 0 failed"));
        assert!(md.contains("## Open issues\n\n_None._"));
        assert!(md.trim_end().ends_with("_No review yet._"));
    }

    #[test]
    fn handover_lives_with_the_task_and_survives_it_moving() {
        let wt = tmp("path");
        // Folder-per-task: the handover goes in the folder.
        let folder = wt.join("tasks/todo/TASK-2026-11-05-001");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("TASK-2026-11-05-001.md"), "# task").unwrap();
        assert_eq!(handover_path(&wt, "tasks/todo/TASK-2026-11-05-001/TASK-2026-11-05-001.md"), folder.join("HANDOVER.md"));

        // Bare file: beside it.
        fs::write(wt.join("tasks/todo/TASK-BARE.md"), "# task").unwrap();
        assert_eq!(handover_path(&wt, "tasks/todo/TASK-BARE.md"), wt.join("tasks/todo/TASK-BARE.HANDOVER.md"));

        // The Workflow Runner moved the folder to tasks/done/ before filing.
        fs::create_dir_all(wt.join("tasks/done")).unwrap();
        fs::rename(&folder, wt.join("tasks/done/TASK-2026-11-05-001")).unwrap();
        assert_eq!(
            handover_path(&wt, "tasks/todo/TASK-2026-11-05-001/TASK-2026-11-05-001.md"),
            wt.join("tasks/done/TASK-2026-11-05-001/HANDOVER.md")
        );

        // Not found anywhere: a fixed place, never somewhere surprising.
        assert_eq!(handover_path(&wt, "tasks/todo/TASK-GONE.md"), wt.join("tasks/handovers/TASK-GONE.HANDOVER.md"));
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn paths_cannot_leave_the_worktree() {
        let wt = tmp("escape");
        assert!(inside_worktree(&wt, "../../etc/passwd").is_err());
        assert!(inside_worktree(&wt, "/etc/passwd").is_err());
        assert!(inside_worktree(&wt, "tasks/../../x").is_err());
        assert!(inside_worktree(&wt, "tasks/todo/a.md").is_ok());
        // A hostile task path is filed under the fixed fallback, inside the worktree.
        let p = handover_path(&wt, "../../outside/evil.md");
        assert!(p.starts_with(&wt), "{p:?}");
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn filing_bumps_the_revision_and_keeps_the_review_log() {
        let wt = tmp("file");
        let folder = wt.join("tasks/todo/TASK-7");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("TASK-7.md"), "# Post Implementation Task Updates\n").unwrap();
        let task = "tasks/todo/TASK-7/TASK-7.md";
        let h = Handover::from_args(&good()).unwrap();

        let first = file(&wt, task, "wf_a", "workflow_runner", &h).unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.display_path, "tasks/todo/TASK-7/HANDOVER.md");
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);

        record_bounce(&wt.join(&first.display_path), "wf_a", "The CSV header is wrong. Fix it.\nThen re-run the tests.").unwrap();
        let after_bounce = fs::read_to_string(wt.join(&first.display_path)).unwrap();
        assert!(after_bounce.contains("sent back (revision 1, session wf_a): The CSV header is wrong. Fix it. Then re-run the tests."));
        assert_eq!(front_matter_value(&after_bounce, "revision").as_deref(), Some("1"), "the file above the log is untouched");

        let second = file(&wt, task, "wf_a", "workflow_runner", &h).unwrap();
        assert_eq!(second.revision, 2);
        assert!(second.markdown.contains("The CSV header is wrong"), "the log survives a new revision");
        assert!(!fs::read_dir(&folder).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp")), "no temp file left");
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn a_completed_handover_notes_a_task_file_without_post_implementation_updates() {
        let wt = tmp("warn");
        fs::create_dir_all(wt.join("tasks/todo")).unwrap();
        fs::write(wt.join("tasks/todo/TASK-8.md"), "# Task\nno updates here").unwrap();
        let h = Handover::from_args(&good()).unwrap();
        let filed = file(&wt, "tasks/todo/TASK-8.md", "wf_b", "workflow_runner", &h).unwrap();
        assert_eq!(filed.warnings.len(), 1);
        assert!(filed.warnings[0].contains("Post Implementation Task Updates"));
        let _ = fs::remove_dir_all(wt);
    }
}
