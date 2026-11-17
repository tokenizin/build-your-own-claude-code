//! Scope contracts, as the agent reads them.
//!
//! A task that splits into several independently running parts (scopes) declares a contract in its parent task file
//! (docs/core/workflow-scope-contract.md in the StarWorld repository). The pre-flight check (`bun run scope:check`) is the only
//! thing that parses that YAML: when the contract is valid it writes a normalized JSON copy to
//! `<manager worktree>/.codenomad/runtime/scope-contracts/<parent task id>.json`, and that copy is all this module reads:
//!
//! ```json
//! { "scope_contract_version": 1, "parent_task": "TASK-2026-11-20-001",
//!   "source": "tasks/todo/TASK-2026-11-20-001/TASK-2026-11-20-001.md",
//!   "limits": { "max_concurrency": 2 },
//!   "scopes": [ { "scope_id": "api", "track": "implementation", "task_path": "tasks/…/api.md",
//!                 "workspace": "/abs/path/to/worktree" | "shared", "spawn_depth": 0 } ] }
//! ```
//!
//! A scope's task file names its contract in its front matter (`scope_contract: <parent task id>`, `scope_id: <id>`), so the
//! workspace, track and limits come from the contract, never from what a model puts in a tool call.
//!
//! The rules (the standard's §5), checked when a workflow starts, never in the middle of a run:
//! 1. one implementation workflow per workspace (`workflows::blocking_implementation`, applied in `main.rs`);
//! 2. at most `max_concurrency` scopes of a contract running, of any track;
//! 3. a workflow started under a scope is at most `spawn_depth` levels below it;
//! 4. a scope's workspace is `shared` or a distinct git worktree of the manager's repository.

use crate::workflows;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What to run to make the normalized copy.
pub const CHECK_COMMAND: &str = "bun run scope:check";

/// The contract a task file says it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeRef {
    pub contract: String,
    pub scope_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub scope_id: String,
    pub track: String,
    pub task_path: String,
    /// The folder the scope's runner works in; none: `shared`, the manager's own worktree.
    pub workspace: Option<String>,
    pub spawn_depth: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    pub id: String,
    /// The parent task file the contract was made from, relative to the manager's worktree.
    pub source: String,
    pub max_concurrency: u32,
    pub scopes: Vec<Scope>,
}

/// A scope of a contract, both loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub contract: Contract,
    pub scope: Scope,
}

impl Resolved {
    /// What the workflow's entry records about it.
    pub fn lineage(&self) -> workflows::Lineage {
        workflows::Lineage {
            workspace: self.scope.workspace.clone(),
            contract: Some(self.contract.id.clone()),
            scope_id: Some(self.scope.scope_id.clone()),
            spawn_depth: Some(self.scope.spawn_depth),
            ..workflows::Lineage::default()
        }
    }
}

pub fn contracts_dir(registry: &Path) -> PathBuf {
    registry.join(".codenomad").join("runtime").join("scope-contracts")
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && !id.contains("..") && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ---------------------------------------------------------------- the task file

/// `scope_contract` and `scope_id` from a task file's `---` block; both are needed.
pub fn parse_scope_ref(text: &str) -> Option<ScopeRef> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    let (mut contract, mut scope_id) = (None, None);
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match key.trim() {
            "scope_contract" if !value.is_empty() => contract = Some(value),
            "scope_id" if !value.is_empty() => scope_id = Some(value),
            _ => {}
        }
    }
    Some(ScopeRef { contract: contract?, scope_id: scope_id? })
}

/// The contract the task file names, if it names one. The file is looked for inside `worktree` only.
pub fn scope_ref(worktree: &Path, task_path: &str) -> Option<ScopeRef> {
    let file = crate::handover::locate_task_file(worktree, task_path)?;
    parse_scope_ref(&fs::read_to_string(file).ok()?)
}

// ---------------------------------------------------------------- the contract

pub fn parse_contract(text: &str) -> Result<Contract, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not JSON ({e})"))?;
    if doc["scope_contract_version"] != 1 {
        return Err("scope_contract_version is not 1".into());
    }
    let string = |v: &Value, what: &str| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| format!("{what} is missing"));
    let id = string(&doc["parent_task"], "parent_task")?;
    if !safe_id(&id) {
        return Err(format!("parent_task `{id}` is not a plain id"));
    }
    let source = string(&doc["source"], "source")?;
    let max_concurrency = doc["limits"]["max_concurrency"].as_u64().filter(|n| *n >= 1).and_then(|n| u32::try_from(n).ok()).ok_or("limits.max_concurrency is not a whole number of at least 1")?;
    let raw = doc["scopes"].as_array().filter(|s| !s.is_empty()).ok_or("scopes is empty")?;
    let mut scopes: Vec<Scope> = Vec::new();
    for item in raw {
        let scope_id = string(&item["scope_id"], "a scope's scope_id")?;
        if scopes.iter().any(|s| s.scope_id == scope_id) {
            return Err(format!("scope_id `{scope_id}` appears twice"));
        }
        let track = string(&item["track"], &format!("scope {scope_id}: track"))?;
        if !matches!(track.as_str(), "implementation" | "investigation" | "spec") {
            return Err(format!("scope {scope_id}: track `{track}` is not implementation, investigation or spec"));
        }
        let task_path = string(&item["task_path"], &format!("scope {scope_id}: task_path"))?;
        let workspace = match string(&item["workspace"], &format!("scope {scope_id}: workspace"))?.as_str() {
            "shared" => None,
            other => Some(other.to_string()),
        };
        let spawn_depth = match &item["spawn_depth"] {
            Value::Null => 0,
            v => v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| format!("scope {scope_id}: spawn_depth is not a whole number"))?,
        };
        scopes.push(Scope { scope_id, track, task_path, workspace, spawn_depth });
    }
    Ok(Contract { id, source, max_concurrency, scopes })
}

/// The normalized contract `id`, if it is there, readable and not older than the task file it was made from.
pub fn load(registry: &Path, id: &str) -> Result<Contract, String> {
    if !safe_id(id) {
        return Err(format!("FAIL: `{id}` is not a scope contract id."));
    }
    let file = contracts_dir(registry).join(format!("{id}.json"));
    let shown = format!(".codenomad/runtime/scope-contracts/{id}.json");
    let text = fs::read_to_string(&file).map_err(|_| format!("FAIL: Scope contract {id} has no normalized copy ({shown}). Run `{CHECK_COMMAND}` and start the workflow again."))?;
    let contract = parse_contract(&text).map_err(|why| format!("FAIL: The normalized copy of scope contract {id} cannot be used: {why}. Run `{CHECK_COMMAND}` again."))?;
    if contract.id != id {
        return Err(format!("FAIL: {shown} is for scope contract {}, not {id}. Run `{CHECK_COMMAND}` again.", contract.id));
    }
    let source = crate::handover::locate_task_file(registry, &contract.source).ok_or_else(|| format!("FAIL: Scope contract {id} was made from {}, which cannot be found.", contract.source))?;
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    if let (Some(copy), Some(task)) = (modified(&file), modified(&source)) {
        if task > copy {
            return Err(format!("FAIL: {} has changed since the normalized copy of scope contract {id} was made. Run `{CHECK_COMMAND}` again.", contract.source));
        }
    }
    Ok(contract)
}

fn find_scope(contract: Contract, scope_id: &str) -> Result<Resolved, String> {
    match contract.scopes.iter().find(|s| s.scope_id == scope_id).cloned() {
        Some(scope) => Ok(Resolved { contract, scope }),
        None => Err(format!("FAIL: Scope contract {} has no scope `{scope_id}`.", contract.id)),
    }
}

/// The scope a task file belongs to: none when it names no contract; an error when it names one that cannot be used.
pub fn resolve(registry: &Path, meta_tree: &Path, task_path: &str) -> Result<Option<Resolved>, String> {
    let Some(named) = scope_ref(meta_tree, task_path) else { return Ok(None) };
    let resolved = find_scope(load(registry, &named.contract)?, &named.scope_id)?;
    let plain = |p: &str| p.trim().trim_start_matches("./").to_string();
    if plain(&resolved.scope.task_path) != plain(task_path) {
        return Err(format!(
            "FAIL: Scope `{}` of contract {} is for {}, not {task_path}.",
            resolved.scope.scope_id, resolved.contract.id, resolved.scope.task_path
        ));
    }
    Ok(Some(resolved))
}

/// The scope a workflow's entry says it is one of, loaded again (a workflow sent back keeps its scope).
pub fn resolve_entry(registry: &Path, lineage: &workflows::Lineage) -> Result<Option<Resolved>, String> {
    match (&lineage.contract, &lineage.scope_id) {
        (Some(contract), Some(scope_id)) => find_scope(load(registry, contract)?, scope_id).map(Some),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------- the rules

/// Rule 4: a scope's workspace is `shared` or a distinct git worktree of the manager's repository.
pub fn check_workspace(registry: &Path, resolved: &Resolved) -> Result<(), String> {
    let Some(workspace) = &resolved.scope.workspace else { return Ok(()) };
    let refuse = |why: String| Err(format!("FAIL: Workspace {workspace} of scope `{}` (contract {}) cannot be used: {why} (rule 4: workspace valid).", resolved.scope.scope_id, resolved.contract.id));
    if !Path::new(workspace).is_absolute() {
        return refuse("it is not an absolute path".into());
    }
    let Ok(here) = fs::canonicalize(workspace) else { return refuse("it does not exist".into()) };
    if !here.is_dir() {
        return refuse("it is not a folder".into());
    }
    let root = fs::canonicalize(registry).unwrap_or_else(|_| registry.to_path_buf());
    if here == root {
        return refuse("it is the manager's own worktree; write the workspace as `shared`".into());
    }
    match git_worktrees(&root) {
        Ok(known) if known.contains(&here) => {}
        Ok(_) => return refuse(format!("it is not a git worktree of {}", root.display())),
        Err(why) => return refuse(format!("git could not list the worktrees of {}: {why}", root.display())),
    }
    for other in resolved.contract.scopes.iter().filter(|s| s.scope_id != resolved.scope.scope_id) {
        let Some(theirs) = other.workspace.as_deref().and_then(|w| fs::canonicalize(w).ok()) else { continue };
        if theirs.starts_with(&here) || here.starts_with(&theirs) {
            return refuse(format!("it overlaps the workspace of scope `{}`", other.scope_id));
        }
    }
    Ok(())
}

fn git_worktrees(root: &Path) -> Result<Vec<PathBuf>, String> {
    let out = Command::new("git").arg("-C").arg(root).args(["worktree", "list", "--porcelain"]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
        .collect())
}

/// Rule 3: a workflow started under a scope is at most `spawn_depth` levels below it. `parent` is the workflow whose run
/// starts this one; every scope among its ancestors is checked, whether or not this workflow is a scope itself.
pub fn check_depth(registry: &Path, parent: Option<&str>) -> Result<(), String> {
    for (index, ancestor) in workflows::ancestors(registry, parent).iter().enumerate() {
        let lineage = workflows::lineage(registry, ancestor);
        if let (Some(scope), Some(limit)) = (&lineage.scope_id, lineage.spawn_depth) {
            let level = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if level > limit {
                return Err(format!(
                    "FAIL: Scope `{scope}` of contract {} may start workflows {limit} level(s) deep; this would be level {level} (rule 3: spawn_depth).",
                    lineage.contract.as_deref().unwrap_or("?")
                ));
            }
        }
    }
    Ok(())
}

/// Rule 2: at most `max_concurrency` scopes of the contract running at once, of any track. `own_id` is the workflow
/// being started (or sent back), which does not count against itself.
pub fn check_concurrency(registry: &Path, resolved: &Resolved, own_id: &str) -> Result<(), String> {
    let running = workflows::running_scopes(registry, &resolved.contract.id, own_id);
    if running.len() as u64 >= u64::from(resolved.contract.max_concurrency) {
        let ids: Vec<&str> = running.iter().map(|e| e.workflow_id.as_str()).collect();
        return Err(format!(
            "FAIL: Scope contract {} allows {} scope(s) to run at once and {} {} running ({}) (rule 2: max_concurrency). Wait for one to finish.",
            resolved.contract.id,
            resolved.contract.max_concurrency,
            running.len(),
            if running.len() == 1 { "is" } else { "are" },
            ids.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scopes-{name}-{}-{}", std::process::id(), crate::delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").args(["-c", "user.email=t@example.com", "-c", "user.name=t", "-C"]).arg(dir).args(args).output().expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A git repository with one commit and a parent task file: the manager's worktree.
    fn manager(name: &str) -> PathBuf {
        let root = temp(name);
        git(&root, &["init", "-q"]);
        fs::create_dir_all(root.join("tasks/todo/TASK-P")).unwrap();
        fs::write(root.join("tasks/todo/TASK-P/TASK-P.md"), "# TASK-P\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        root
    }

    fn worktree_of(root: &Path, name: &str) -> PathBuf {
        let path = root.parent().unwrap().join(format!("{}-{name}", root.file_name().unwrap().to_string_lossy()));
        git(root, &["worktree", "add", "-q", path.to_str().unwrap(), "-b", name]);
        fs::canonicalize(path).unwrap()
    }

    fn contract_json(scopes: Value, max_concurrency: u64) -> Value {
        serde_json::json!({ "scope_contract_version": 1, "parent_task": "TASK-P", "source": "tasks/todo/TASK-P/TASK-P.md", "limits": { "max_concurrency": max_concurrency }, "scopes": scopes })
    }

    fn scope_json(id: &str, track: &str, workspace: &str, spawn_depth: u64) -> Value {
        serde_json::json!({ "scope_id": id, "track": track, "task_path": format!("tasks/todo/TASK-P/{id}/TASK-P-{id}.md"), "workspace": workspace, "spawn_depth": spawn_depth })
    }

    fn write_contract(registry: &Path, doc: &Value) {
        fs::create_dir_all(contracts_dir(registry)).unwrap();
        fs::write(contracts_dir(registry).join("TASK-P.json"), doc.to_string()).unwrap();
    }

    fn resolved(doc: &Value, scope_id: &str) -> Resolved {
        let contract = parse_contract(&doc.to_string()).unwrap();
        let scope = contract.scopes.iter().find(|s| s.scope_id == scope_id).unwrap().clone();
        Resolved { contract, scope }
    }

    fn run_entry(registry: &Path, id: &str, track: &str, lineage: workflows::Lineage) {
        workflows::write_entry(registry, &workflows::Entry { workflow_id: id.into(), task_path: "t.md".into(), track: track.into(), state: "running".into(), send_backs: 0 }).unwrap();
        workflows::set_lineage(registry, id, &lineage).unwrap();
    }

    #[test]
    fn a_task_file_names_its_contract_only_with_both_keys() {
        let both = "---\ntrack: implementation\nscope_contract: TASK-P\nscope_id: \"api\"\n---\n# T\nscope_id: ignored\n";
        assert_eq!(parse_scope_ref(both), Some(ScopeRef { contract: "TASK-P".into(), scope_id: "api".into() }));
        assert_eq!(parse_scope_ref("---\nscope_contract: TASK-P\n---\n"), None);
        assert_eq!(parse_scope_ref("---\nscope_id: api\n---\n"), None);
        assert_eq!(parse_scope_ref("---\nscope_contract:\nscope_id: api\n---\n"), None);
        assert_eq!(parse_scope_ref("# no front matter\nscope_contract: TASK-P\nscope_id: api"), None);
    }

    #[test]
    fn a_normalized_contract_is_read_and_a_bad_one_says_what_is_wrong() {
        let good = contract_json(serde_json::json!([scope_json("api", "implementation", "shared", 0), scope_json("ui", "spec", "/w/ui", 2)]), 2);
        let contract = parse_contract(&good.to_string()).unwrap();
        assert_eq!((contract.id.as_str(), contract.max_concurrency, contract.scopes.len()), ("TASK-P", 2, 2));
        assert_eq!(contract.scopes[0].workspace, None, "`shared` is the manager's own worktree");
        assert_eq!(contract.scopes[1].workspace.as_deref(), Some("/w/ui"));
        assert_eq!(contract.scopes[1].spawn_depth, 2);

        let bad = |mutate: &dyn Fn(&mut Value)| {
            let mut doc = good.clone();
            mutate(&mut doc);
            parse_contract(&doc.to_string()).unwrap_err()
        };
        assert!(bad(&|d| d["scope_contract_version"] = serde_json::json!(2)).contains("version"));
        assert!(bad(&|d| d["limits"]["max_concurrency"] = serde_json::json!(0)).contains("max_concurrency"));
        assert!(bad(&|d| d["scopes"] = serde_json::json!([])).contains("scopes"));
        assert!(bad(&|d| d["scopes"][0]["track"] = serde_json::json!("deploy")).contains("track"));
        assert!(bad(&|d| d["scopes"][1]["scope_id"] = serde_json::json!("api")).contains("twice"));
        assert!(bad(&|d| d["parent_task"] = serde_json::json!("../x")).contains("plain id"));
        assert!(parse_contract("not json").unwrap_err().contains("not JSON"));
    }

    #[test]
    fn a_contract_with_no_copy_or_a_stale_one_is_refused_with_what_to_run() {
        let root = manager("load");
        let doc = contract_json(serde_json::json!([scope_json("api", "implementation", "shared", 0)]), 1);
        let missing = load(&root, "TASK-P").unwrap_err();
        assert!(missing.starts_with("FAIL:") && missing.contains(CHECK_COMMAND), "{missing}");

        write_contract(&root, &doc);
        assert_eq!(load(&root, "TASK-P").unwrap().id, "TASK-P");

        // The parent task changed after the copy was made.
        let task = root.join("tasks/todo/TASK-P/TASK-P.md");
        fs::File::options().write(true).open(&task).unwrap().set_modified(SystemTime::now() + Duration::from_secs(60)).unwrap();
        let stale = load(&root, "TASK-P").unwrap_err();
        assert!(stale.contains("has changed since") && stale.contains(CHECK_COMMAND), "{stale}");

        assert!(load(&root, "../escape").unwrap_err().contains("not a scope contract id"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_task_resolves_to_its_scope_or_is_refused() {
        let root = manager("resolve");
        write_contract(&root, &contract_json(serde_json::json!([scope_json("api", "implementation", "shared", 0)]), 1));
        let task = "tasks/todo/TASK-P/api/TASK-P-api.md";
        fs::create_dir_all(root.join("tasks/todo/TASK-P/api")).unwrap();
        // The contract file must be newer than the parent task: write it again after the files above.
        write_contract(&root, &contract_json(serde_json::json!([scope_json("api", "implementation", "shared", 0)]), 1));

        fs::write(root.join(task), "---\ntrack: spec\n---\n# plain task\n").unwrap();
        assert_eq!(resolve(&root, &root, task), Ok(None), "a task that names no contract is not in one");

        fs::write(root.join(task), "---\nscope_contract: TASK-P\nscope_id: api\n---\n").unwrap();
        let found = resolve(&root, &root, task).unwrap().unwrap();
        assert_eq!((found.scope.scope_id.as_str(), found.scope.track.as_str()), ("api", "implementation"));
        assert_eq!(found.lineage().contract.as_deref(), Some("TASK-P"));

        fs::write(root.join(task), "---\nscope_contract: TASK-P\nscope_id: nope\n---\n").unwrap();
        assert!(resolve(&root, &root, task).unwrap_err().contains("has no scope `nope`"));
        fs::write(root.join(task), "---\nscope_contract: TASK-NONE\nscope_id: api\n---\n").unwrap();
        assert!(resolve(&root, &root, task).unwrap_err().contains(CHECK_COMMAND));

        // The contract lists another file for this scope.
        fs::write(root.join(task), "---\nscope_contract: TASK-P\nscope_id: api\n---\n").unwrap();
        let other = "tasks/todo/TASK-P/api/OTHER.md";
        fs::write(root.join(other), "---\nscope_contract: TASK-P\nscope_id: api\n---\n").unwrap();
        assert!(resolve(&root, &root, other).unwrap_err().contains("is for tasks/todo/TASK-P/api/TASK-P-api.md"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_scope_workspace_must_be_shared_or_a_distinct_worktree_of_the_manager() {
        let root = manager("workspace");
        let api = worktree_of(&root, "api");
        let ui = worktree_of(&root, "ui");
        let on = |ws: &str| {
            let doc = contract_json(serde_json::json!([scope_json("api", "implementation", ws, 0), scope_json("ui", "implementation", ui.to_str().unwrap(), 0)]), 2);
            check_workspace(&root, &resolved(&doc, "api"))
        };
        assert_eq!(on("shared"), Ok(()));
        assert_eq!(on(api.to_str().unwrap()), Ok(()));

        let refused = |ws: &str, why: &str| {
            let err = on(ws).unwrap_err();
            assert!(err.starts_with("FAIL:") && err.contains("rule 4") && err.contains(why), "{ws}: {err}");
        };
        refused("relative/dir", "not an absolute path");
        refused("/no/such/folder", "does not exist");
        refused(root.to_str().unwrap(), "manager's own worktree");
        let stranger = temp("stranger");
        refused(stranger.to_str().unwrap(), "not a git worktree");
        refused(ui.to_str().unwrap(), "overlaps the workspace of scope `ui`");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(api);
        let _ = fs::remove_dir_all(ui);
        let _ = fs::remove_dir_all(stranger);
    }

    #[test]
    fn a_workflow_may_start_workflows_only_as_deep_below_a_scope_as_it_allows() {
        let root = manager("depth");
        let scope = |depth: u32, parent: Option<&str>| workflows::Lineage { contract: Some("TASK-P".into()), scope_id: Some("api".into()), spawn_depth: Some(depth), parent: parent.map(str::to_string), ..Default::default() };
        let child = |parent: &str| workflows::Lineage { parent: Some(parent.into()), ..Default::default() };

        // A scope that may not start workflows.
        run_entry(&root, "wf_scope0", "implementation", scope(0, None));
        let refused = check_depth(&root, Some("wf_scope0")).unwrap_err();
        assert!(refused.contains("0 level(s) deep") && refused.contains("level 1") && refused.contains("rule 3"), "{refused}");
        assert_eq!(check_depth(&root, None), Ok(()), "the manager's own start is not below any scope");

        // One that may start them one level deep: a child is fine, a grandchild is not.
        run_entry(&root, "wf_scope1", "implementation", scope(1, None));
        run_entry(&root, "wf_kid", "spec", child("wf_scope1"));
        assert_eq!(check_depth(&root, Some("wf_scope1")), Ok(()));
        assert!(check_depth(&root, Some("wf_kid")).unwrap_err().contains("level 2"));

        // A workflow that is under no scope is under no limit.
        run_entry(&root, "wf_free", "spec", workflows::Lineage::default());
        run_entry(&root, "wf_free_kid", "spec", child("wf_free"));
        assert_eq!(check_depth(&root, Some("wf_free_kid")), Ok(()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_contract_allows_only_max_concurrency_scopes_at_once_of_any_track() {
        let root = manager("concurrency");
        let doc = contract_json(serde_json::json!([scope_json("a", "implementation", "shared", 0), scope_json("b", "spec", "shared", 0), scope_json("c", "investigation", "shared", 0)]), 2);
        let me = resolved(&doc, "c");
        let in_contract = |scope: &str| workflows::Lineage { contract: Some("TASK-P".into()), scope_id: Some(scope.into()), ..Default::default() };

        assert_eq!(check_concurrency(&root, &me, "wf_c"), Ok(()));
        run_entry(&root, "wf_a", "implementation", in_contract("a"));
        assert_eq!(check_concurrency(&root, &me, "wf_c"), Ok(()), "one of two");
        run_entry(&root, "wf_b", "spec", in_contract("b"));
        let refused = check_concurrency(&root, &me, "wf_c").unwrap_err();
        assert!(refused.contains("allows 2") && refused.contains("2 are running") && refused.contains("wf_a, wf_b") && refused.contains("rule 2"), "{refused}");
        assert_eq!(check_concurrency(&root, &me, "wf_b"), Ok(()), "a workflow sent back does not count against itself");

        workflows::set_state(&root, "wf_b", "idle");
        assert_eq!(check_concurrency(&root, &me, "wf_c"), Ok(()), "a finished scope frees its place");
        run_entry(&root, "wf_other", "spec", workflows::Lineage { contract: Some("TASK-OTHER".into()), scope_id: Some("x".into()), ..Default::default() });
        assert_eq!(check_concurrency(&root, &me, "wf_c"), Ok(()), "another contract's scopes are not counted");
        let _ = fs::remove_dir_all(root);
    }
}
