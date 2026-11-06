//! OpenCode's `apply_patch` tool: several file changes in one call, in a stripped-down diff envelope.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: hello.txt
//! +Hello world
//! *** Update File: src/app.py
//! *** Move to: src/main.py
//! @@ def greet():
//! -print("Hi")
//! +print("Hello, world!")
//! *** Delete File: obsolete.txt
//! *** End Patch
//! ```
//!
//! OpenCode offers it to GPT models in place of Edit and Write; the Rust agent does the same (`offered_for`).
//! Every change is worked out in memory first and written only if all of them apply, so a patch that does not fit
//! leaves the files as they were.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// OpenCode's rule: GPT models (not `gpt-4*`, not the open-weight `gpt-oss`) get `apply_patch` instead of Edit and Write.
pub fn offered_for(model: &str) -> bool {
    let id = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    id.contains("gpt-") && !id.contains("oss") && !id.contains("gpt-4")
}

pub fn tool_spec() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "apply_patch",
            "description": "Use the `apply_patch` tool to edit files. Your patch language is a stripped-down, file-oriented diff format designed to be easy to parse and safe to apply. \
You can think of it as a high-level envelope:\n\n*** Begin Patch\n[ one or more file sections ]\n*** End Patch\n\n\
Within that envelope, you get a sequence of file operations.\nYou MUST include a header to specify the action you are taking.\n\
Each operation starts with one of three headers:\n\n\
*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).\n\
*** Delete File: <path> - remove an existing file. Nothing follows.\n\
*** Update File: <path> - patch an existing file in place (optionally with a rename).\n\n\
Example patch:\n\n```\n*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello, world!\")\n*** Delete File: obsolete.txt\n*** End Patch\n```\n\n\
It is important to remember:\n\n- You must include a header with your intended action (Add/Delete/Update)\n- You must prefix new lines with `+` even when creating a new file",
            "parameters": {
                "type": "object",
                "required": ["patchText"],
                "properties": { "patchText": { "type": "string", "description": "The full patch text that describes all changes to be made" } }
            }
        }
    })
}

#[derive(Debug, PartialEq)]
struct Hunk {
    /// The text after `@@`: a line to find first, to place the change.
    context: String,
    old: Vec<String>,
    new: Vec<String>,
    at_end: bool,
}

#[derive(Debug, PartialEq)]
enum Op {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, hunks: Vec<Hunk> },
}

fn parse(text: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = text.trim().lines().collect();
    if lines.first().map(|l| l.trim()) != Some("*** Begin Patch") || lines.last().map(|l| l.trim()) != Some("*** End Patch") {
        return Err("the patch must start with `*** Begin Patch` and end with `*** End Patch`".into());
    }
    let body = &lines[1..lines.len() - 1];
    let is_header = |l: &str| l.starts_with("*** Add File:") || l.starts_with("*** Delete File:") || l.starts_with("*** Update File:");
    let mut ops = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File:") {
            let mut content = String::new();
            i += 1;
            while i < body.len() && !is_header(body[i]) {
                let added = body[i].strip_prefix('+').ok_or_else(|| format!("in `Add File: {}` every line must start with `+`, got `{}`", path.trim(), body[i]))?;
                content.push_str(added);
                content.push('\n');
                i += 1;
            }
            ops.push(Op::Add { path: path.trim().into(), content });
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            ops.push(Op::Delete { path: path.trim().into() });
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File:") {
            i += 1;
            let mut move_to = None;
            if i < body.len() {
                if let Some(target) = body[i].strip_prefix("*** Move to:") {
                    move_to = Some(target.trim().to_string());
                    i += 1;
                }
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            while i < body.len() && !is_header(body[i]) {
                let l = body[i];
                if let Some(context) = l.strip_prefix("@@") {
                    hunks.push(Hunk { context: context.trim().to_string(), old: vec![], new: vec![], at_end: false });
                } else if l.trim() == "*** End of File" {
                    if let Some(h) = hunks.last_mut() {
                        h.at_end = true;
                    }
                } else {
                    if hunks.is_empty() {
                        hunks.push(Hunk { context: String::new(), old: vec![], new: vec![], at_end: false });
                    }
                    let h = hunks.last_mut().unwrap();
                    match l.chars().next() {
                        Some('+') => h.new.push(l[1..].to_string()),
                        Some('-') => h.old.push(l[1..].to_string()),
                        Some(' ') => {
                            h.old.push(l[1..].to_string());
                            h.new.push(l[1..].to_string());
                        }
                        // A blank context line whose leading space was lost.
                        None => {
                            h.old.push(String::new());
                            h.new.push(String::new());
                        }
                        _ => return Err(format!("in `Update File: {}` a line must start with ` `, `-` or `+`, got `{l}`", path.trim())),
                    }
                }
                i += 1;
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("`Update File: {}` has no changes", path.trim()));
            }
            ops.push(Op::Update { path: path.trim().into(), move_to, hunks });
        } else {
            return Err(format!("unexpected line `{line}`: expected `*** Add File:`, `*** Delete File:` or `*** Update File:`"));
        }
    }
    if ops.is_empty() {
        return Err("the patch has no file operations".into());
    }
    Ok(ops)
}

/// The first index at or after `from` where `needle` occurs in `lines` (exact, then ignoring trailing, then all, whitespace).
fn find_block(lines: &[String], needle: &[String], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(lines.len()));
    }
    let same: [fn(&str, &str) -> bool; 3] = [|a, b| a == b, |a, b| a.trim_end() == b.trim_end(), |a, b| a.trim() == b.trim()];
    same.iter().find_map(|eq| (from..=lines.len().saturating_sub(needle.len())).find(|&at| lines.len() >= needle.len() && needle.iter().zip(&lines[at..]).all(|(n, l)| eq(l, n))))
}

fn update(path: &str, original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let ends_with_newline = original.ends_with('\n') || original.is_empty();
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut cursor = 0;
    for hunk in hunks {
        if !hunk.context.is_empty() {
            let at = lines[cursor.min(lines.len())..].iter().position(|l| l.trim() == hunk.context.trim()).map(|p| p + cursor);
            match at {
                Some(at) => cursor = at + 1,
                None => return Err(format!("{path}: could not find `{}` to place a change", hunk.context)),
            }
        }
        let at = if hunk.at_end && hunk.old.is_empty() {
            Some(lines.len())
        } else {
            find_block(&lines, &hunk.old, cursor)
        };
        let Some(at) = at else {
            let first = hunk.old.first().map(String::as_str).unwrap_or("");
            return Err(format!("{path}: the lines to change were not found (starting `{first}`); read the file again and make the patch match it exactly"));
        };
        lines.splice(at..at + hunk.old.len(), hunk.new.iter().cloned());
        cursor = at + hunk.new.len();
    }
    let mut out = lines.join("\n");
    if ends_with_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The files a patch touches (to claim them, for tasks running at the same time).
pub fn paths(patch_text: &str) -> Vec<String> {
    parse(patch_text)
        .map(|ops| {
            ops.into_iter()
                .flat_map(|op| match op {
                    Op::Add { path, .. } | Op::Delete { path } => vec![path],
                    Op::Update { path, move_to, .. } => std::iter::once(path).chain(move_to).collect(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Applies the patch under `base` (relative paths) and returns OpenCode's summary, or the reason it does not fit.
pub fn apply(patch_text: &str, base: &Path) -> Result<String, String> {
    let ops = parse(patch_text)?;
    let resolve = |p: &str| -> PathBuf { if Path::new(p).is_absolute() { PathBuf::from(p) } else { base.join(p) } };
    // path -> new content (None: removed), in the order first touched.
    let mut result: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let mut summary: Vec<String> = Vec::new();
    let current = |result: &BTreeMap<PathBuf, Option<String>>, path: &Path| -> Option<String> {
        match result.get(path) {
            Some(state) => state.clone(),
            None => fs::read_to_string(path).ok(),
        }
    };
    for op in ops {
        match op {
            Op::Add { path, content } => {
                let full = resolve(&path);
                if current(&result, &full).is_some() {
                    return Err(format!("{path}: cannot add a file that already exists (use Update File)"));
                }
                result.insert(full, Some(content));
                summary.push(format!("A {path}"));
            }
            Op::Delete { path } => {
                let full = resolve(&path);
                if current(&result, &full).is_none() {
                    return Err(format!("{path}: cannot delete a file that does not exist"));
                }
                result.insert(full, None);
                summary.push(format!("D {path}"));
            }
            Op::Update { path, move_to, hunks } => {
                let full = resolve(&path);
                let Some(original) = current(&result, &full) else { return Err(format!("{path}: cannot update a file that does not exist")) };
                let updated = update(&path, &original, &hunks)?;
                match move_to {
                    Some(target) => {
                        let target_full = resolve(&target);
                        if target_full != full && current(&result, &target_full).is_some() {
                            return Err(format!("{target}: cannot move onto a file that already exists"));
                        }
                        result.insert(full, None);
                        result.insert(target_full, Some(updated));
                        summary.push(format!("M {target}"));
                    }
                    None => {
                        result.insert(full, Some(updated));
                        summary.push(format!("M {path}"));
                    }
                }
            }
        }
    }
    for (path, state) in &result {
        match state {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", path.display()))?;
                }
                fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            None => {
                let _ = fs::remove_file(path);
            }
        }
    }
    Ok(format!("Success. Updated the following files:\n{}", summary.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("patch-{name}-{}-{}", std::process::id(), crate::delegation::new_task_id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn gpt_models_get_apply_patch_instead_of_edit_and_write() {
        assert!(offered_for("gpt-5") && offered_for("openai/gpt-5-codex") && offered_for("GPT-5.1"));
        assert!(!offered_for("gpt-4o") && !offered_for("gpt-oss:20b") && !offered_for("qwen2.5:14b"));
    }

    #[test]
    fn add_update_move_and_delete_in_one_patch() {
        let d = dir("all");
        fs::write(d.join("app.py"), "def greet():\n    print(\"Hi\")\n\ngreet()\n").unwrap();
        fs::write(d.join("obsolete.txt"), "old\n").unwrap();
        let patch = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n+second\n*** Update File: app.py\n*** Move to: src/main.py\n@@ def greet():\n-    print(\"Hi\")\n+    print(\"Hello, world!\")\n*** Delete File: obsolete.txt\n*** End Patch";
        assert_eq!(apply(patch, &d).unwrap(), "Success. Updated the following files:\nA hello.txt\nM src/main.py\nD obsolete.txt");
        assert_eq!(fs::read_to_string(d.join("hello.txt")).unwrap(), "Hello world\nsecond\n");
        assert_eq!(fs::read_to_string(d.join("src/main.py")).unwrap(), "def greet():\n    print(\"Hello, world!\")\n\ngreet()\n");
        assert!(!d.join("app.py").exists() && !d.join("obsolete.txt").exists());
        assert_eq!(paths(patch), ["hello.txt", "app.py", "src/main.py", "obsolete.txt"]);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn hunks_are_placed_by_their_context_and_apply_in_order() {
        let d = dir("hunks");
        fs::write(d.join("f.txt"), "a\nb\nc\nb\nd\n").unwrap();
        // The second `b` is the one after `c`; context lines keep the rest.
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@ c\n-b\n+B\n d\n+e\n*** End of File\n*** End Patch";
        apply(patch, &d).unwrap();
        assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "a\nb\nc\nB\nd\ne\n");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn a_patch_that_does_not_fit_changes_nothing() {
        let d = dir("atomic");
        fs::write(d.join("one.txt"), "x\n").unwrap();
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+hi\n*** Update File: one.txt\n-not there\n+y\n*** End Patch";
        let err = apply(patch, &d).unwrap_err();
        assert!(err.contains("one.txt: the lines to change were not found"), "{err}");
        assert!(!d.join("new.txt").exists(), "the earlier addition was not written");
        assert_eq!(fs::read_to_string(d.join("one.txt")).unwrap(), "x\n");
        // Adding a file that exists, deleting one that does not.
        assert!(apply("*** Begin Patch\n*** Add File: one.txt\n+z\n*** End Patch", &d).unwrap_err().contains("already exists"));
        assert!(apply("*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch", &d).unwrap_err().contains("does not exist"));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn the_envelope_is_checked() {
        assert!(parse("hello").unwrap_err().contains("*** Begin Patch"));
        assert!(parse("*** Begin Patch\n*** End Patch").unwrap_err().contains("no file operations"));
        assert!(parse("*** Begin Patch\n*** Add File: a\nplain\n*** End Patch").unwrap_err().contains("must start with `+`"));
        assert!(parse("*** Begin Patch\n*** Update File: a\n*** End Patch").unwrap_err().contains("no changes"));
        assert!(parse("*** Begin Patch\nrandom\n*** End Patch").unwrap_err().contains("unexpected line"));
        // Whitespace differences in the lines to change are forgiven, in steps.
        let lines: Vec<String> = ["  a", "b  ", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(find_block(&lines, &["a".to_string(), "b".to_string()], 0), Some(0));
    }
}
