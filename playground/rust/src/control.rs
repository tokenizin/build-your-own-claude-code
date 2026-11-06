//! Stopping one delegated task without ending the whole run.
//!
//! The host (CodeNomad) owns the process, so it cannot reach a single task inside it. It drops a marker
//! file, `stop-<task_id>`, into the directory named by `NOMAD_CONTROL_DIR`; a running task looks for its
//! own marker (every `POLL`) and, when it finds one, is dropped where it stands: an in-flight model
//! request is cancelled, nested tasks go with it, and the task ends as `aborted`. The other tasks of the
//! batch, and the caller, carry on. The caller gets the task's id back so the work can be resumed.

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
