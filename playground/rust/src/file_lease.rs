use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// File lease registry for concurrent tool-call safety.
///
/// The Rust agent loop is single-threaded, but recursive subagents (`Task`,
/// forked skills) share the same process and can interleave file operations
/// via `await`. This module provides an in-memory lease table so that two
/// concurrent tool calls cannot modify the same file simultaneously.
///
/// Leases are process-local and die with the process — no stale lease cleanup
/// is required.

static LEASES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn table() -> &'static Mutex<HashMap<String, String>> {
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone)]
pub struct LeaseError {
    pub path: String,
    pub holder: String,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file `{}` is currently leased by tool call `{}`",
            self.path, self.holder
        )
    }
}

/// RAII guard that releases a file lease on drop.
#[derive(Debug)]
pub struct FileLeaseGuard {
    path: String,
    call_id: String,
}

impl Drop for FileLeaseGuard {
    fn drop(&mut self) {
        release(&self.path, &self.call_id);
    }
}

/// Acquire an exclusive lease on `path` for `call_id`.
///
/// Returns `Ok(guard)` if the lease was acquired, or `Err(LeaseError)` if
/// another call id currently holds the lease.
pub fn acquire(path: &str, call_id: &str) -> Result<FileLeaseGuard, LeaseError> {
    let mut map = table().lock().expect("file lease table poisoned");
    if let Some(holder) = map.get(path) {
        if holder != call_id {
            return Err(LeaseError {
                path: path.to_string(),
                holder: holder.clone(),
            });
        }
        // Same call_id re-acquiring: idempotent refresh.
    } else {
        map.insert(path.to_string(), call_id.to_string());
    }
    Ok(FileLeaseGuard {
        path: path.to_string(),
        call_id: call_id.to_string(),
    })
}

/// Release a lease early (the RAII guard also releases on drop).
pub fn release(path: &str, call_id: &str) {
    let mut map = table().lock().expect("file lease table poisoned");
    if let Some(holder) = map.get(path) {
        if holder == call_id {
            map.remove(path);
        }
    }
}

/// Acquire a lease, run `f`, then release the lease.
///
/// This is the preferred entry point for file-touching tools.
pub fn with_lease<F, T>(path: &str, call_id: &str, f: F) -> Result<T, LeaseError>
where
    F: FnOnce() -> T,
{
    let _guard = acquire(path, call_id)?;
    Ok(f())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_basic() {
        let path = "/tmp/test-lease.txt";
        let c1 = "call-1";
        let c2 = "call-2";

        let _guard1 = acquire(path, c1).expect("first acquire should succeed");
        let err = acquire(path, c2).expect_err("second acquire should fail");
        assert_eq!(err.path, path);
        assert_eq!(err.holder, c1);

        release(path, c1);
        acquire(path, c2).expect("acquire after release should succeed");
    }

    #[test]
    fn with_lease_runs_closure() {
        let path = "/tmp/test-lease-with.txt";
        let call_id = "call-with";
        let result = with_lease(path, call_id, || 42).expect("with_lease should succeed");
        assert_eq!(result, 42);
        // Lease released after closure.
        acquire(path, "other").expect("path should be free after with_lease");
    }

    #[test]
    fn re_acquire_same_call_id_is_idempotent() {
        let path = "/tmp/test-lease-idem.txt";
        let call_id = "call-idem";
        acquire(path, call_id).expect("first acquire");
        acquire(path, call_id).expect("re-acquire same id should be ok");
    }
}
