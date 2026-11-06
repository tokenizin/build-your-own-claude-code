//! Stopping one delegated task without ending the whole run.
//!
//! The host (CodeNomad) owns the process, so it cannot reach a single task inside it. It drops a marker
//! file, `stop-<task_id>`, into the directory named by `NOMAD_CONTROL_DIR`; a running task looks for its
//! own marker (every `POLL`) and, when it finds one, is dropped where it stands: an in-flight model
//! request is cancelled, nested tasks go with it, and the task ends as `aborted`. The other tasks of the
//! batch, and the caller, carry on. The caller gets the task's id back so the work can be resumed.
//!
//! The same directory carries the other direction too: a question the agent asks (`question` tool) is answered
//! by the host dropping `answer-<id>.json` there.

use std::path::PathBuf;
use std::time::Duration;

/// How often a running task looks for its stop marker.
const POLL: Duration = Duration::from_millis(200);

/// What a stopped task's run ends with (the text becomes the task's result for its caller).
pub const STOPPED: &str = "stopped by the user";

fn marker(task_id: &str) -> Option<PathBuf> {
    let dir = std::env::var("NOMAD_CONTROL_DIR").ok().filter(|d| !d.trim().is_empty())?;
    let safe = !task_id.is_empty() && task_id.len() <= 100 && task_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    safe.then(|| PathBuf::from(dir).join(format!("stop-{task_id}")))
}

/// Forgets an earlier stop request: a task that is resumed must not stop at once.
pub fn clear(task_id: &str) {
    if let Some(path) = marker(task_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Resolves when a stop was asked for this task (and takes the request). Never resolves without a control directory.
pub async fn requested(task_id: &str) {
    let Some(path) = marker(task_id) else { return std::future::pending().await };
    loop {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Runs `work`, unless a stop is asked for `task_id` first: then `work` is dropped and `Err(STOPPED)` comes back.
pub async fn stoppable<T>(task_id: &str, work: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::select! {
        result = work => result,
        _ = requested(task_id) => Err(STOPPED.to_string()),
    }
}

/// What the user said to a question the agent asked (`question` tool).
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// One list of chosen labels (or typed text) per question, in order.
    Answers(Vec<Vec<String>>),
    /// The user dismissed the question.
    Dismissed,
}

fn answer_marker(question_id: &str) -> Option<PathBuf> {
    let dir = std::env::var("NOMAD_CONTROL_DIR").ok().filter(|d| !d.trim().is_empty())?;
    let safe = !question_id.is_empty() && question_id.len() <= 100 && question_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    safe.then(|| PathBuf::from(dir).join(format!("answer-{question_id}.json")))
}

/// Whether anyone can answer: the host gave this run a control directory.
pub fn can_ask() -> bool {
    std::env::var("NOMAD_CONTROL_DIR").map(|d| !d.trim().is_empty()).unwrap_or(false)
}

/// Waits for the host to put `answer-<id>.json` (`{"answers":[["A"],["B","C"]]}` or `{"dismissed":true}`) in the
/// control directory, and takes it. A task that is stopped meanwhile is dropped by `stoppable`, so this just waits.
pub async fn wait_for_answer(question_id: &str) -> Answer {
    let Some(path) = answer_marker(question_id) else { return std::future::pending().await };
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) {
                let _ = std::fs::remove_file(&path);
                if doc["dismissed"].as_bool() == Some(true) {
                    return Answer::Dismissed;
                }
                let answers = doc["answers"]
                    .as_array()
                    .map(|list| list.iter().map(|one| one.as_array().map(|labels| labels.iter().filter_map(|l| l.as_str().map(str::to_string)).collect()).unwrap_or_default()).collect())
                    .unwrap_or_default();
                return Answer::Answers(answers);
            }
        }
        tokio::time::sleep(POLL).await;
    }
}
