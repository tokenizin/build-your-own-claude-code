//! Bookkeeping for asynchronous workflow sessions (`NOMAD_WORKFLOW_MODE=async`).
//!
//! In OpenCode, `nomadflow_run_workflow` returns at once and the Workflow Runner works in a
//! session of its own; the Product Manager is told when it is done. This agent is one process
//! per message, so the runner cannot live inside the Product Manager's process: the host
//! (CodeNomad) starts a second process for it and posts the notification. What the two processes
//! share is a small registry in the worktree — one file per workflow in
//! `.codenomad/runtime/workflows/` (git-ignored runtime state):
//!
//! * the Product Manager's process writes an entry as `running` when it starts or re-prompts one;
//! * the host marks it `idle` when the runner's process ends;
//! * both read it: the one-implementation-workflow-at-a-time guard, and "is this session running?".

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Local sessions are named `ses_lcl_<id>`; a workflow's id is the part after it.
pub const SESSION_PREFIX: &str = "ses_lcl_";

/// The workflow id for a session id a model passed back (the prefix is optional).
pub fn workflow_id(session: &str) -> String {
    let session = session.trim();
    session.strip_prefix(SESSION_PREFIX).unwrap_or(session).to_string()
}

/// The session id the host uses for a workflow.
pub fn session_id(workflow_id: &str) -> String {
    format!("{SESSION_PREFIX}{workflow_id}")
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The fields of a task file's front matter the Workflow Runner is told about.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskMeta {
    pub complexity: Option<String>,
    pub track: Option<String>,
    pub slice: Option<String>,
    pub status: Option<String>,
}

impl TaskMeta {
    /// `implementation` unless the task file says otherwise.
    pub fn track_or_default(&self) -> String {
        self.track.clone().unwrap_or_else(|| "implementation".into())
    }
}

/// Reads `complexity`, `track`, `slice` and `status` from the task file's `---` block.
/// The file is looked for inside the worktree only; a missing or unreadable file gives no fields.
pub fn task_meta(worktree: &Path, task_path: &str) -> TaskMeta {
    let Some(file) = crate::handover::locate_task_file(worktree, task_path) else { return TaskMeta::default() };
    let Ok(text) = fs::read_to_string(file) else { return TaskMeta::default() };
    parse_front_matter(&text)
}

fn parse_front_matter(text: &str) -> TaskMeta {
    let mut meta = TaskMeta::default();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return meta;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "complexity" => meta.complexity = Some(value),
            "track" => meta.track = Some(value),
            "slice" => meta.slice = Some(value),
            "status" => meta.status = Some(value),
            _ => {}
        }
    }
    meta
}

/// True when `.codenomad/nomadworks.yaml` says the team is not the full one, which has no Workflow Runner.
/// No file, or no `team_mode` line, allows workflows.
pub fn team_mode_blocks_workflows(worktree: &Path) -> bool {
    let Ok(text) = fs::read_to_string(worktree.join(".codenomad").join("nomadworks.yaml")) else { return false };
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("team_mode:"))
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase())
        .next()
        .is_some_and(|mode| !mode.is_empty() && mode != "full")
}

/// The first message the Workflow Runner is shown in its session (the shape NomadWorks' plugin uses).
pub fn display_prompt(task_path: &str, meta: &TaskMeta, track: &str, instructions: &str) -> String {
    let summary = [
        meta.complexity.as_ref().map(|v| format!("Complexity: {v}")),
        Some(format!("Track: {track}")),
        meta.slice.as_ref().map(|v| format!("Slice: {v}")),
        meta.status.as_ref().map(|v| format!("Status: {v}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");
    let lifecycle = match track {
        "implementation" => "Please execute the full lifecycle (Sync -> Implementation -> Commit -> Archive) and provide a final summary.",
        "spec" => "Please execute the full spec lifecycle for this task, update the required documentation artifacts, and provide a final summary.",
        _ => "Please execute the investigation lifecycle for this task, capture findings clearly, and provide a final summary.",
    };
    format!("Task File: {task_path}\n\n{summary}\n\nInstructions: {instructions}\n\n{lifecycle}")
}

// ---------------------------------------------------------------- registry

/// One workflow session, as the registry records it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub workflow_id: String,
    pub task_path: String,
    pub track: String,
    /// `running` while its runner has work, `idle` after.
    pub state: String,
}

impl Entry {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

pub fn registry_dir(worktree: &Path) -> PathBuf {
    worktree.join(".codenomad").join("runtime").join("workflows")
}

fn entry_path(worktree: &Path, workflow_id: &str) -> Option<PathBuf> {
    safe_id(workflow_id).then(|| registry_dir(worktree).join(format!("{workflow_id}.json")))
}

pub fn write_entry(worktree: &Path, entry: &Entry) -> Result<(), String> {
    let path = entry_path(worktree, &entry.workflow_id).ok_or_else(|| format!("bad workflow id `{}`", entry.workflow_id))?;
    fs::create_dir_all(registry_dir(worktree)).map_err(|e| e.to_string())?;
    let doc = json!({ "workflow_id": entry.workflow_id, "task_path": entry.task_path, "track": entry.track, "state": entry.state });
    fs::write(path, doc.to_string()).map_err(|e| e.to_string())
}

fn read_entry(path: &Path) -> Option<Entry> {
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    Some(Entry {
        workflow_id: doc["workflow_id"].as_str()?.to_string(),
        task_path: doc["task_path"].as_str().unwrap_or_default().to_string(),
        track: doc["track"].as_str().unwrap_or("implementation").to_string(),
        state: doc["state"].as_str().unwrap_or("idle").to_string(),
    })
}

pub fn find(worktree: &Path, workflow_id: &str) -> Option<Entry> {
    read_entry(&entry_path(worktree, workflow_id)?)
}

/// A workflow that is running on the `implementation` track, if there is one: only one may own the shared worktree.
pub fn running_implementation(worktree: &Path) -> Option<Entry> {
    let mut found: Vec<Entry> = fs::read_dir(registry_dir(worktree))
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| read_entry(&e.path()))
        .filter(|e| e.running() && e.track == "implementation")
        .collect();
    found.sort_by(|a, b| a.workflow_id.cmp(&b.workflow_id));
    found.into_iter().next()
}

/// Marks a workflow `idle` (its runner is done) or `running` again.
pub fn set_state(worktree: &Path, workflow_id: &str, state: &str) {
    if let Some(mut entry) = find(worktree, workflow_id) {
        entry.state = state.into();
        let _ = write_entry(worktree, &entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(name: &str) -> PathBuf {
        let wt = std::env::temp_dir().join(format!("wfreg-{name}-{}-{}", std::process::id(), crate::delegation::new_task_id()));
        fs::create_dir_all(wt.join("tasks/todo/TASK-1")).unwrap();
        wt
    }

    #[test]
    fn session_and_workflow_ids_round_trip_and_accept_both_forms() {
        assert_eq!(session_id("wf_ab12"), "ses_lcl_wf_ab12");
        assert_eq!(workflow_id("ses_lcl_wf_ab12"), "wf_ab12");
        assert_eq!(workflow_id(" wf_ab12 "), "wf_ab12");
    }

    #[test]
    fn front_matter_gives_the_fields_the_runner_is_told() {
        let meta = parse_front_matter("---\nid: T\nstatus: active\ncomplexity: complex\ntrack: spec\nslice: \"logic\"\n---\n# Body\ntrack: ignored\n");
        assert_eq!(meta.track.as_deref(), Some("spec"));
        assert_eq!(meta.complexity.as_deref(), Some("complex"));
        assert_eq!(meta.slice.as_deref(), Some("logic"));
        assert_eq!(meta.status.as_deref(), Some("active"));
        assert_eq!(parse_front_matter("# no front matter\ntrack: spec"), TaskMeta::default());
        assert_eq!(TaskMeta::default().track_or_default(), "implementation");
    }

    #[test]
    fn the_display_prompt_follows_the_tracks_lifecycle() {
        let meta = TaskMeta { complexity: Some("complex".into()), track: None, slice: Some("logic".into()), status: Some("active".into()) };
        let text = display_prompt("tasks/todo/T/T.md", &meta, "implementation", "run it");
        assert_eq!(
            text,
            "Task File: tasks/todo/T/T.md\n\nComplexity: complex\nTrack: implementation\nSlice: logic\nStatus: active\n\nInstructions: run it\n\n\
Please execute the full lifecycle (Sync -> Implementation -> Commit -> Archive) and provide a final summary."
        );
        assert!(display_prompt("t", &TaskMeta::default(), "spec", "x").ends_with("update the required documentation artifacts, and provide a final summary."));
        assert!(display_prompt("t", &TaskMeta::default(), "investigation", "x").contains("investigation lifecycle"));
    }

    #[test]
    fn only_an_explicit_non_full_team_mode_blocks_workflows() {
        let wt = worktree("team");
        assert!(!team_mode_blocks_workflows(&wt), "no file");
        fs::create_dir_all(wt.join(".codenomad")).unwrap();
        fs::write(wt.join(".codenomad/nomadworks.yaml"), "enabled: true\nteam_mode: full\n").unwrap();
        assert!(!team_mode_blocks_workflows(&wt));
        fs::write(wt.join(".codenomad/nomadworks.yaml"), "enabled: true\nteam_mode: mini\n").unwrap();
        assert!(team_mode_blocks_workflows(&wt));
        fs::write(wt.join(".codenomad/nomadworks.yaml"), "enabled: true\n").unwrap();
        assert!(!team_mode_blocks_workflows(&wt), "no team_mode line");
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn the_registry_guards_one_running_implementation_workflow() {
        let wt = worktree("registry");
        let entry = |id: &str, track: &str, state: &str| Entry { workflow_id: id.into(), task_path: "t.md".into(), track: track.into(), state: state.into() };
        assert!(running_implementation(&wt).is_none());
        write_entry(&wt, &entry("wf_spec", "spec", "running")).unwrap();
        assert!(running_implementation(&wt).is_none(), "a spec workflow does not own the worktree");
        write_entry(&wt, &entry("wf_impl", "implementation", "running")).unwrap();
        assert_eq!(running_implementation(&wt).unwrap().workflow_id, "wf_impl");
        set_state(&wt, "wf_impl", "idle");
        assert!(running_implementation(&wt).is_none(), "released when its runner is done");
        assert!(!find(&wt, "wf_impl").unwrap().running());
        set_state(&wt, "wf_impl", "running");
        assert!(running_implementation(&wt).is_some());
        assert!(find(&wt, "../escape").is_none() && find(&wt, "").is_none());
        let _ = fs::remove_dir_all(wt);
    }
}
