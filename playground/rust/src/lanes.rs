//! Lanes for Task calls that run at the same time.
//!
//! When one turn dispatches several Task calls, each runs in its own lane (nested
//! batches extend it: `call_1/call_7`). A file written inside a lane belongs to that lane
//! until the lane ends; another lane trying to write it is refused, so two parallel tasks
//! cannot overwrite each other's work. A lane and its own descendants share files freely.
//!
//! This covers the `Write` tool. A shell command (`sed -i`, `git`, `>`) cannot be seen, and a
//! file read before another lane changed it can still be stale: the Product Manager's rule
//! that implementation work is sequential in a shared worktree remains the real policy.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

fn owners() -> &'static Mutex<HashMap<String, String>> {
    static OWNERS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The lane for `call_id` under `parent` (a call id may not contain the separator).
pub fn child_lane(parent: Option<&str>, call_id: &str) -> String {
    let id: String = call_id.chars().map(|c| if c == '/' { '_' } else { c }).collect();
    match parent {
        Some(p) => format!("{p}/{id}"),
        None => id,
    }
}

/// Lanes conflict unless one is the other or its ancestor.
pub fn conflicts(a: &str, b: &str) -> bool {
    !(a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/")))
}

/// An absolute, `..`-free form of `path`, so two spellings of one file are one key.
pub fn normalize(path: &str) -> String {
    let given = Path::new(path.trim());
    let joined = if given.is_absolute() {
        given.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(given)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().to_string()
}

/// Takes `path` for `lane`, or says which lane has it.
pub fn claim(path: &str, lane: &str) -> Result<(), String> {
    let key = normalize(path);
    let mut map = owners().lock().expect("lane table poisoned");
    match map.get(&key) {
        Some(owner) if conflicts(owner, lane) => Err(owner.clone()),
        Some(_) => Ok(()),
        None => {
            map.insert(key, lane.to_string());
            Ok(())
        }
    }
}

/// The lane ended: its files (and those of lanes nested in it) are free again.
pub fn release(lane: &str) {
    let nested = format!("{lane}/");
    owners().lock().expect("lane table poisoned").retain(|_, owner| owner != lane && !owner.starts_with(&nested));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_conflict_unless_related() {
        assert!(conflicts("a", "b"));
        assert!(conflicts("a/x", "a/y"));
        assert!(conflicts("a/x", "b"));
        assert!(!conflicts("a", "a"));
        assert!(!conflicts("a", "a/x"));
        assert!(!conflicts("a/x/y", "a"));
        // A shared prefix is not a relation.
        assert!(conflicts("call_1", "call_10"));
    }

    #[test]
    fn child_lanes_nest_and_sanitize() {
        assert_eq!(child_lane(None, "call_1"), "call_1");
        assert_eq!(child_lane(Some("call_1"), "call_2"), "call_1/call_2");
        assert_eq!(child_lane(None, "a/b"), "a_b");
    }

    #[test]
    fn a_file_belongs_to_the_first_lane_until_it_ends() {
        let file = format!("/tmp/lane-test-{}-a.txt", std::process::id());
        assert_eq!(claim(&file, "one"), Ok(()));
        assert_eq!(claim(&file, "one"), Ok(()), "the owner can write again");
        assert_eq!(claim(&file, "two"), Err("one".into()));
        assert_eq!(claim(&format!("/tmp/x/../lane-test-{}-a.txt", std::process::id()), "two"), Err("one".into()), "another spelling is the same file");
        assert_eq!(claim(&file, "one/nested"), Ok(()), "a nested lane may use its ancestor's file");
        release("one");
        assert_eq!(claim(&file, "two"), Ok(()), "free once the lane ended");
        release("two");
    }

    #[test]
    fn releasing_a_lane_frees_what_its_descendants_took() {
        let file = format!("/tmp/lane-test-{}-b.txt", std::process::id());
        assert_eq!(claim(&file, "p/child"), Ok(()));
        assert_eq!(claim(&file, "q"), Err("p/child".into()));
        release("p");
        assert_eq!(claim(&file, "q"), Ok(()));
        release("q");
    }
}
