//! OpenCode-compatible tools beyond Read / Write / Bash: `Edit`, `Grep`, `Glob`,
//! `WebFetch`, `TodoWrite` and `TodoRead`.
//!
//! The names are Claude Code's (like the rest of this agent) and `delegation::canonical_tool_name`
//! accepts OpenCode's lower-case ones; argument names are accepted in both styles, so the NomadWorks
//! prompts written for OpenCode work unchanged. Like Write and Bash these are not sandboxed: the
//! agent already has a shell, and these add convenience, not reach.

use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

// ---------------------------------------------------------------- specs

fn spec(name: &str, description: &str, required: &[&str], properties: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "required": required, "properties": properties }
        }
    })
}

/// OpenCode's `question` tool: ask the user something and wait for the answer. Offered only when the host
/// can pass an answer back (`NOMAD_CONTROL_DIR`).
pub fn question_spec() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "question",
            "description": "Use this tool when you need to ask the user questions during execution. This allows you to:\n\
1. Gather user preferences or requirements\n2. Clarify ambiguous instructions\n3. Get decisions on implementation choices as you work\n\
4. Offer choices to the user about what direction to take.\n\nUsage notes:\n\
- When `custom` is enabled (default), a \"Type your own answer\" option is added automatically; don't include \"Other\" or catch-all options\n\
- Answers are returned as arrays of labels; set `multiple: true` to allow selecting more than one\n\
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label",
            "parameters": {
                "type": "object",
                "required": ["questions"],
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "Questions to ask",
                        "items": {
                            "type": "object",
                            "required": ["question", "header", "options"],
                            "properties": {
                                "question": { "type": "string", "description": "Complete question" },
                                "header": { "type": "string", "description": "Very short label (max 30 chars)" },
                                "options": {
                                    "type": "array",
                                    "description": "Available choices",
                                    "items": {
                                        "type": "object",
                                        "required": ["label", "description"],
                                        "properties": {
                                            "label": { "type": "string", "description": "Display text (1-5 words, concise)" },
                                            "description": { "type": "string", "description": "Explanation of choice" }
                                        }
                                    }
                                },
                                "multiple": { "type": "boolean", "description": "Allow selecting multiple choices" }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// The questions of a `question` call, checked and tidied (`header` cut to 30 characters).
pub fn parse_questions(args: &Value) -> Result<Vec<Value>, String> {
    let list = args["questions"].as_array().filter(|l| !l.is_empty()).ok_or("Error: question requires `questions`: a list of {question, header, options}.")?;
    list.iter()
        .map(|item| {
            let question = item["question"].as_str().map(str::trim).filter(|q| !q.is_empty()).ok_or("Error: every question needs a `question` text.")?;
            let header: String = item["header"].as_str().unwrap_or(question).chars().take(30).collect();
            let options: Vec<Value> = item["options"]
                .as_array()
                .map(|opts| {
                    opts.iter()
                        .filter_map(|o| {
                            let label = o["label"].as_str().or_else(|| o.as_str())?.trim();
                            (!label.is_empty()).then(|| json!({ "label": label, "description": o["description"].as_str().unwrap_or("") }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({ "question": question, "header": header, "options": options, "multiple": item["multiple"].as_bool().unwrap_or(false) }))
        })
        .collect::<Result<Vec<Value>, &str>>()
        .map_err(str::to_string)
}

/// The model-facing result, in OpenCode's words.
pub fn answered_text(questions: &[Value], answers: &[Vec<String>]) -> String {
    let lines: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let given = answers.get(i).filter(|a| !a.is_empty()).map(|a| a.join(", ")).unwrap_or_else(|| "Unanswered".into());
            format!("\"{}\"=\"{given}\"", q["question"].as_str().unwrap_or(""))
        })
        .collect();
    format!("User has answered your questions: {}. You can now continue with the user's answers in mind.", lines.join(", "))
}

pub fn tool_specs() -> Vec<Value> {
    vec![
        spec(
            "Edit",
            "Replace text in an existing file. old_string must match the file exactly (including whitespace) and be unique, or pass replace_all. \
Returns the change as a unified diff. To create a new file give an empty old_string; to rewrite a whole file use Write.",
            &["file_path", "old_string", "new_string"],
            json!({
                "file_path": { "type": "string", "description": "The file to edit" },
                "old_string": { "type": "string", "description": "The exact text to replace" },
                "new_string": { "type": "string", "description": "The text to put in its place" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default: the match must be unique)" }
            }),
        ),
        spec(
            "Grep",
            "Search file contents with a regular expression. Respects .gitignore. Returns matching lines grouped by file, with line numbers.",
            &["pattern"],
            json!({
                "pattern": { "type": "string", "description": "Regular expression to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: the current directory)" },
                "include": { "type": "string", "description": "Only files matching this glob, e.g. \"*.ts\" or \"**/*.{ts,tsx}\"" }
            }),
        ),
        spec(
            "Glob",
            "Find files by name pattern (e.g. \"**/*.ts\", \"*.md\"). Respects .gitignore. Newest files first.",
            &["pattern"],
            json!({
                "pattern": { "type": "string", "description": "Glob pattern; without a slash it matches file names at any depth" },
                "path": { "type": "string", "description": "Directory to search (default: the current directory)" }
            }),
        ),
        spec(
            "WebFetch",
            "Fetch a web page (http/https) and return it as markdown, plain text or raw HTML. Large pages are cut to fit the model's context.",
            &["url"],
            json!({
                "url": { "type": "string", "description": "The URL to fetch" },
                "format": { "type": "string", "enum": ["markdown", "text", "html"], "description": "Output format (default markdown)" },
                "timeout": { "type": "number", "description": "Seconds to wait (default 30, at most 120)" }
            }),
        ),
        spec(
            "TodoWrite",
            "Create or replace your todo list for this task. Send the whole list each time. Use it to plan multi-step work and mark items completed as you go; \
the user sees it as your plan.",
            &["todos"],
            json!({
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["content", "status"],
                        "properties": {
                            "id": { "type": "string" },
                            "content": { "type": "string", "description": "What to do" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] },
                            "priority": { "type": "string", "enum": ["high", "medium", "low"] }
                        }
                    }
                }
            }),
        ),
        spec("TodoRead", "Read your current todo list.", &[], json!({})),
    ]
}

// ---------------------------------------------------------------- shared helpers

fn text_arg(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

fn bool_arg(v: &Value, key: &str) -> bool {
    match &v[key] {
        Value::Bool(b) => *b,
        Value::String(s) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn absolute(path: &str) -> PathBuf {
    let p = Path::new(path.trim());
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
    }
}

// ---------------------------------------------------------------- Edit

/// A unified diff of `old` → `new` (3 lines of context). Falls back to one block when the files are large.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Op {
        Keep,
        Del,
        Add,
    }
    // A final newline ends the last line; it does not start another (an empty one) as split would have it.
    let a: Vec<&str> = old.strip_suffix('\n').unwrap_or(old).split('\n').collect();
    let b: Vec<&str> = new.strip_suffix('\n').unwrap_or(new).split('\n').collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    let (ma, mb) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    // The changed middle: a real line diff when it is small enough, else delete-all/add-all.
    let mut ops: Vec<(Op, &str)> = a[..prefix].iter().map(|l| (Op::Keep, *l)).collect();
    if ma.len() * mb.len() <= 4_000_000 {
        let (n, m) = (ma.len(), mb.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if ma[i] == mb[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && ma[i] == mb[j] {
                ops.push((Op::Keep, ma[i]));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
                // Removals before additions, as in any unified diff.
                ops.push((Op::Del, ma[i]));
                i += 1;
            } else {
                ops.push((Op::Add, mb[j]));
                j += 1;
            }
        }
    } else {
        ops.extend(ma.iter().map(|l| (Op::Del, *l)));
        ops.extend(mb.iter().map(|l| (Op::Add, *l)));
    }
    ops.extend(a[a.len() - suffix..].iter().map(|l| (Op::Keep, *l)));

    // Hunks: every change with 3 lines of context, overlapping ones merged.
    const CTX: usize = 3;
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, (op, _))| *op != Op::Keep).map(|(i, _)| i).collect();
    if changed.is_empty() {
        return String::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let (lo, hi) = (i.saturating_sub(CTX), (i + CTX + 1).min(ops.len()));
        match ranges.last_mut() {
            Some(last) if lo <= last.1 => last.1 = hi,
            _ => ranges.push((lo, hi)),
        }
    }
    let mut out = format!("--- {path}\n+++ {path}\n");
    let mut shown = 0usize;
    for (lo, hi) in ranges {
        let old_start = ops[..lo].iter().filter(|(op, _)| *op != Op::Add).count() + 1;
        let new_start = ops[..lo].iter().filter(|(op, _)| *op != Op::Del).count() + 1;
        let old_len = ops[lo..hi].iter().filter(|(op, _)| *op != Op::Add).count();
        let new_len = ops[lo..hi].iter().filter(|(op, _)| *op != Op::Del).count();
        out.push_str(&format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"));
        for (op, line) in &ops[lo..hi] {
            if shown >= 300 {
                out.push_str("... (diff truncated)\n");
                return out;
            }
            out.push(match op {
                Op::Keep => ' ',
                Op::Del => '-',
                Op::Add => '+',
            });
            out.push_str(line);
            out.push('\n');
            shown += 1;
        }
    }
    out
}

/// Replaces `old` with `new` in `content`. Exact text first; if that is not found, the same lines
/// compared without leading/trailing whitespace (models often get indentation slightly wrong).
fn replace_text(content: &str, old: &str, new: &str, replace_all: bool, path: &str) -> Result<String, String> {
    let exact = content.matches(old).count();
    if exact > 0 {
        if exact > 1 && !replace_all {
            return Err(format!(
                "Error: old_string was found {exact} times in {path}. Add more surrounding lines to make it unique, or set replace_all to change every occurrence."
            ));
        }
        return Ok(if replace_all { content.replace(old, new) } else { content.replacen(old, new, 1) });
    }

    let old_lines: Vec<&str> = old.trim_end_matches('\n').split('\n').collect();
    let nothing_to_match = old_lines.iter().all(|l| l.trim().is_empty());
    if !nothing_to_match {
        let file_lines: Vec<&str> = content.split('\n').collect();
        let n = old_lines.len();
        let starts: Vec<usize> = (0..file_lines.len().saturating_sub(n - 1))
            .filter(|&i| (0..n).all(|j| file_lines[i + j].trim() == old_lines[j].trim()))
            .collect();
        if starts.len() > 1 && !replace_all {
            return Err(format!(
                "Error: old_string matched {} places in {path} (ignoring indentation). Add more surrounding lines to make it unique, or set replace_all.",
                starts.len()
            ));
        }
        if !starts.is_empty() {
            let new_lines: Vec<&str> = new.trim_end_matches('\n').split('\n').collect();
            let mut result: Vec<&str> = Vec::new();
            let mut i = 0;
            while i < file_lines.len() {
                if starts.contains(&i) && (replace_all || starts[0] == i) {
                    result.extend(&new_lines);
                    i += n;
                } else {
                    result.push(file_lines[i]);
                    i += 1;
                }
            }
            return Ok(result.join("\n"));
        }
    }
    Err(format!(
        "Error: old_string was not found in {path}. Read the file again and copy the exact text, including whitespace."
    ))
}

pub fn edit(args: &Value) -> String {
    let path = text_arg(args, "file_path");
    let (old, new) = (text_arg(args, "old_string"), text_arg(args, "new_string"));
    let replace_all = bool_arg(args, "replace_all");
    if path.trim().is_empty() {
        return "Error: Edit requires file_path, old_string and new_string.".into();
    }
    if old == new {
        return "Error: old_string and new_string are identical, so there is nothing to change.".into();
    }
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && old.is_empty() => {
            if let Some(parent) = Path::new(&path).parent() {
                let _ = fs::create_dir_all(parent);
            }
            return match fs::write(&path, &new) {
                Ok(()) => format!("Created {path}\n{}", unified_diff(&path, "", &new)),
                Err(e) => format!("Error writing {path}: {e}"),
            };
        }
        Err(e) => return format!("Error reading {path}: {e}"),
    };
    if old.is_empty() {
        return format!("Error: old_string is empty but {path} already exists. Give the text to replace, or use Write to rewrite the whole file.");
    }
    let updated = match replace_text(&content, &old, &new, replace_all, &path) {
        Ok(u) => u,
        Err(message) => return message,
    };
    if let Err(e) = fs::write(&path, &updated) {
        return format!("Error writing {path}: {e}");
    }
    let diff = unified_diff(&path, &content, &updated);
    if diff.is_empty() {
        // Only the end-of-file newline can differ without a changed line.
        return format!("Edit applied to {path}; no line changed (only the newline at the end of the file).");
    }
    diff
}

// ---------------------------------------------------------------- Grep / Glob

const MAX_RESULTS: usize = 100;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false) // dot files count, as for ripgrep --hidden; .git itself is filtered below
        .require_git(false) // a .gitignore counts even in a plain folder
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != "node_modules"
        });
    builder
}

fn compile_glob(pattern: &str) -> Result<globset::GlobMatcher, String> {
    // `*` stays within one path segment (`src/*.ts` is not `src/deep/b.ts`); `**` crosses them.
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("Error: invalid glob `{pattern}`: {e}"))
}

/// A glob without a slash matches names at any depth; with one it matches the path under `root`.
fn glob_hits(matcher: &globset::GlobMatcher, pattern: &str, root: &Path, file: &Path) -> bool {
    if pattern.contains('/') {
        file.strip_prefix(root).map(|rel| matcher.is_match(rel)).unwrap_or(false)
    } else {
        file.file_name().map(|n| matcher.is_match(Path::new(n))).unwrap_or(false)
    }
}

pub fn grep(args: &Value) -> String {
    let pattern = text_arg(args, "pattern");
    if pattern.trim().is_empty() {
        return "Error: Grep requires a pattern.".into();
    }
    let re = match Regex::new(&pattern) {
        Ok(re) => re,
        Err(e) => return format!("Error: invalid regular expression: {e}"),
    };
    let root = absolute(args["path"].as_str().filter(|p| !p.trim().is_empty()).unwrap_or("."));
    if !root.exists() {
        return format!("Error: {} does not exist.", root.display());
    }
    let include = args["include"].as_str().map(str::trim).filter(|i| !i.is_empty());
    let include = match include.map(|i| compile_glob(i).map(|m| (m, i))) {
        Some(Err(message)) => return message,
        Some(Ok(pair)) => Some(pair),
        None => None,
    };

    let mut found: Vec<(PathBuf, Vec<(usize, String)>)> = Vec::new();
    let mut total = 0usize;
    'files: for entry in walker(&root).build().flatten() {
        let file = entry.path();
        if !file.is_file() {
            continue;
        }
        if let Some((matcher, raw)) = &include {
            let base = if root.is_file() { file.parent().unwrap_or(&root).to_path_buf() } else { root.clone() };
            if !glob_hits(matcher, raw, &base, file) {
                continue;
            }
        }
        if fs::metadata(file).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(bytes) = fs::read(file) else { continue };
        if bytes.iter().take(8000).any(|b| *b == 0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut hits = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let shown: String = line.chars().take(2000).collect();
                hits.push((i + 1, shown));
                total += 1;
                if total >= MAX_RESULTS {
                    found.push((file.to_path_buf(), hits));
                    break 'files;
                }
            }
        }
        if !hits.is_empty() {
            found.push((file.to_path_buf(), hits));
        }
    }
    if total == 0 {
        return "No matches found.".into();
    }
    let mut out = format!("Found {total} match{}", if total == 1 { "" } else { "es" });
    if total >= MAX_RESULTS {
        out.push_str(&format!(" (stopped at the first {MAX_RESULTS}; narrow the pattern, path or include)"));
    }
    out.push('\n');
    for (file, hits) in found {
        out.push_str(&format!("\n{}:\n", file.display()));
        for (n, line) in hits {
            out.push_str(&format!("  Line {n}: {line}\n"));
        }
    }
    out
}

pub fn glob(args: &Value) -> String {
    let pattern = text_arg(args, "pattern");
    if pattern.trim().is_empty() {
        return "Error: Glob requires a pattern.".into();
    }
    let matcher = match compile_glob(pattern.trim()) {
        Ok(m) => m,
        Err(message) => return message,
    };
    let root = absolute(args["path"].as_str().filter(|p| !p.trim().is_empty()).unwrap_or("."));
    if !root.is_dir() {
        return format!("Error: {} is not a directory.", root.display());
    }
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in walker(&root).build().flatten() {
        let file = entry.path();
        if file.is_file() && glob_hits(&matcher, pattern.trim(), &root, file) {
            let modified = fs::metadata(file).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
            files.push((modified, file.to_path_buf()));
        }
    }
    if files.is_empty() {
        return "No files found".into();
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = files.len() > MAX_RESULTS;
    let mut out: String = files.iter().take(MAX_RESULTS).map(|(_, p)| format!("{}\n", p.display())).collect();
    if truncated {
        out.push_str(&format!("\n(Results are truncated: showing the first {MAX_RESULTS} of {}. Use a more specific path or pattern.)\n", files.len()));
    }
    out
}

// ---------------------------------------------------------------- WebFetch

const WEBFETCH_MAX_BYTES: usize = 5 * 1024 * 1024;

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let decoded = tail.find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &tail[..end];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "copy" => Some('©'),
                "rarr" => Some('→'),
                "larr" => Some('←'),
                _ => name.strip_prefix('#').and_then(|num| {
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => num.parse::<u32>().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            };
            ch.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let at = lower.find(&format!("{name}="))?;
    let value = &tag[at + name.len() + 1..];
    let (quote, body) = match value.chars().next()? {
        q @ ('"' | '\'') => (Some(q), &value[1..]),
        _ => (None, value),
    };
    let end = match quote {
        Some(q) => body.find(q).unwrap_or(body.len()),
        None => body.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(body.len()),
    };
    Some(decode_entities(&body[..end]))
}

/// A small, dependency-free HTML → markdown (or plain text) converter: enough to make a page readable
/// to a model (headings, paragraphs, lists, links, emphasis, code), not a faithful renderer.
pub fn html_to_text(html: &str, markdown: bool) -> String {
    const SKIP: [&str; 6] = ["script", "style", "noscript", "template", "svg", "head"];
    const BLOCK: [&str; 18] = [
        "p", "div", "section", "article", "header", "footer", "main", "nav", "aside", "ul", "ol", "table", "tr", "blockquote", "form", "dl", "dt", "dd",
    ];
    let mut out = String::new();
    let mut in_pre = false;
    let mut hrefs: Vec<String> = Vec::new();
    let mut rest = html;

    let break_block = |out: &mut String| {
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
    };
    let break_line = |out: &mut String| {
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };

    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, &decode_entities(rest), in_pre);
            break;
        };
        push_text(&mut out, &decode_entities(&rest[..lt]), in_pre);
        let tail = &rest[lt..];
        if let Some(body) = tail.strip_prefix("<!--") {
            rest = body.find("-->").map(|e| &body[e + 3..]).unwrap_or("");
            continue;
        }
        // The tag ends at the first '>' outside quotes.
        let mut quote: Option<char> = None;
        let mut end = None;
        for (i, c) in tail.char_indices().skip(1) {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (None, '"' | '\'') => quote = Some(c),
                (None, '>') => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            push_text(&mut out, &decode_entities(tail), in_pre);
            break;
        };
        let tag = &tail[1..end];
        rest = &tail[end + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        let closing = tag.starts_with('/');
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
        if !closing && SKIP.contains(&name.as_str()) && !tag.ends_with('/') {
            // Drop everything up to the matching close tag.
            let close = format!("</{name}");
            rest = match rest.to_lowercase().find(&close) {
                Some(at) => rest[at..].find('>').map(|g| &rest[at + g + 1..]).unwrap_or(""),
                None => "",
            };
            continue;
        }
        match (name.as_str(), closing) {
            ("br", _) => out.push('\n'),
            ("hr", false) => {
                break_block(&mut out);
                out.push_str(if markdown { "---" } else { "" });
                break_block(&mut out);
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => {
                break_block(&mut out);
                if markdown {
                    out.push_str(&"#".repeat(name[1..].parse::<usize>().unwrap_or(1)));
                    out.push(' ');
                }
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => break_block(&mut out),
            ("li", false) => {
                break_line(&mut out);
                out.push_str("- ");
            }
            ("li", true) => break_line(&mut out),
            ("pre", false) => {
                break_block(&mut out);
                in_pre = true;
                if markdown {
                    out.push_str("```\n");
                }
            }
            ("pre", true) => {
                in_pre = false;
                if markdown {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```");
                }
                break_block(&mut out);
            }
            ("a", false) => {
                hrefs.push(attr(tag, "href").unwrap_or_default());
                if markdown {
                    out.push('[');
                }
            }
            ("a", true) => {
                let href = hrefs.pop().unwrap_or_default();
                if markdown {
                    out.push(']');
                    if !href.is_empty() && !href.starts_with('#') && !href.to_lowercase().starts_with("javascript:") {
                        out.push_str(&format!("({href})"));
                    }
                }
            }
            ("strong" | "b", _) if markdown => out.push_str("**"),
            ("em" | "i", _) if markdown => out.push('*'),
            ("code", _) if markdown && !in_pre => out.push('`'),
            ("img", false) => {
                let alt = attr(tag, "alt").unwrap_or_default();
                if markdown {
                    match attr(tag, "src") {
                        Some(src) if !src.is_empty() => out.push_str(&format!("![{alt}]({src})")),
                        _ => out.push_str(&alt),
                    }
                } else {
                    out.push_str(&alt);
                }
            }
            ("td" | "th", true) => out.push_str(" | "),
            (n, _) if BLOCK.contains(&n) => break_block(&mut out),
            _ => {}
        }
    }
    // Tidy: trailing spaces, runs of blank lines.
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    let mut tidy = String::new();
    let mut blank = 0;
    for line in lines {
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim().to_string()
}

fn push_text(out: &mut String, text: &str, in_pre: bool) {
    if in_pre {
        out.push_str(text);
        return;
    }
    // Words as they come; any run of whitespace becomes one space, never a leading one.
    for chunk in text.split_inclusive(char::is_whitespace) {
        let word = chunk.trim();
        if !word.is_empty() {
            out.push_str(word);
        }
        if chunk.ends_with(char::is_whitespace) && !out.is_empty() && !out.ends_with(['\n', ' ']) {
            out.push(' ');
        }
    }
}

pub async fn webfetch(args: &Value) -> String {
    let url = text_arg(args, "url");
    let parsed = match reqwest::Url::parse(url.trim()) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        Ok(u) => return format!("Error: WebFetch only fetches http or https URLs, not `{}`.", u.scheme()),
        Err(e) => return format!("Error: `{url}` is not a valid URL: {e}"),
    };
    let format = args["format"].as_str().map(|f| f.trim().to_lowercase()).filter(|f| !f.is_empty()).unwrap_or_else(|| "markdown".into());
    if !matches!(format.as_str(), "markdown" | "text" | "html") {
        return "Error: format must be markdown, text or html.".into();
    }
    let seconds = args["timeout"].as_f64().filter(|t| *t > 0.0).unwrap_or(30.0).min(120.0);
    let max_chars = std::env::var("WEBFETCH_MAX_CHARS").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(60_000);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs_f64(seconds))
        .user_agent("Mozilla/5.0 (compatible; claude-code-rust-playground)")
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("Error: could not start the request: {e}"),
    };
    let accept = match format.as_str() {
        "html" => "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
        "text" => "text/plain,text/html;q=0.8,*/*;q=0.5",
        _ => "text/markdown,text/html;q=0.9,text/plain;q=0.8,*/*;q=0.5",
    };
    let mut response = match client.get(parsed.clone()).header("Accept", accept).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error: request to {parsed} failed: {e}"),
    };
    if !response.status().is_success() {
        return format!("Error: {parsed} returned HTTP {}.", response.status());
    }
    let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    if content_type.starts_with("image/") && !content_type.contains("svg") {
        return format!("Error: {parsed} is an image ({content_type}); WebFetch returns text only.");
    }
    if response.content_length().is_some_and(|n| n as usize > WEBFETCH_MAX_BYTES) {
        return format!("Error: {parsed} is larger than 5 MB.");
    }
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > WEBFETCH_MAX_BYTES {
                    return format!("Error: {parsed} is larger than 5 MB.");
                }
            }
            Ok(None) => break,
            Err(e) => return format!("Error: reading {parsed} failed: {e}"),
        }
    }
    let raw = String::from_utf8_lossy(&body).to_string();
    let looks_html = content_type.contains("html") || {
        let head = raw.trim_start().to_lowercase();
        head.starts_with("<!doctype html") || head.starts_with("<html")
    };
    let mut text = match (format.as_str(), looks_html) {
        ("html", _) | (_, false) => raw,
        ("text", true) => html_to_text(&raw, false),
        (_, true) => html_to_text(&raw, true),
    };
    let total = text.chars().count();
    if total > max_chars {
        text = text.chars().take(max_chars).collect();
        text.push_str(&format!("\n\n[truncated: {} more characters; fetch a more specific page, or raise WEBFETCH_MAX_CHARS]", total - max_chars));
    }
    text
}

// ---------------------------------------------------------------- Todos

fn todo_memory() -> &'static Mutex<HashMap<String, Vec<Value>>> {
    static TODOS: OnceLock<Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();
    TODOS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn todo_file(dir: &Path, scope: &str) -> PathBuf {
    let safe: String = scope.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    dir.join(format!("todos-{safe}.json"))
}

fn normalize_status(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().replace(['-', ' '], "_").as_str() {
        "pending" | "todo" | "not_started" | "open" => Some("pending"),
        "in_progress" | "inprogress" | "active" | "doing" | "started" => Some("in_progress"),
        "completed" | "complete" | "done" | "finished" => Some("completed"),
        "cancelled" | "canceled" | "skipped" => Some("cancelled"),
        _ => None,
    }
}

fn normalize_priority(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "high" | "urgent" | "p0" | "p1" => "high",
        "low" | "p3" => "low",
        _ => "medium",
    }
}

/// The todo items in canonical form, or every problem with them. Items arrive as an array or as a JSON string of one.
pub fn parse_todos(args: &Value) -> Result<Vec<Value>, Vec<String>> {
    let items: Vec<Value> = match &args["todos"] {
        Value::Array(a) => a.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw.trim()) {
            Ok(Value::Array(a)) => a,
            _ => return Err(vec!["todos must be an array of { content, status } items".into()]),
        },
        _ => return Err(vec!["todos is required: an array of { content, status } items (send the whole list)".into()]),
    };
    let mut errors = Vec::new();
    let mut todos = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let content = ["content", "task", "text", "title"].iter().find_map(|k| item[*k].as_str()).map(str::trim).unwrap_or("");
        if content.is_empty() {
            errors.push(format!("todos[{i}] has no content"));
            continue;
        }
        let Some(status) = normalize_status(item["status"].as_str().unwrap_or("pending")) else {
            errors.push(format!("todos[{i}] status must be one of: pending, in_progress, completed, cancelled"));
            continue;
        };
        let id = item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{}", i + 1));
        todos.push(json!({
            "id": id,
            "content": content,
            "status": status,
            "priority": normalize_priority(item["priority"].as_str().unwrap_or("medium")),
        }));
    }
    if errors.is_empty() { Ok(todos) } else { Err(errors) }
}

/// The items of a `TodoWrite` call's list that are not finished (`pending`, `in_progress`), as `[status] content`.
pub fn unfinished_todos(args: &Value) -> Vec<String> {
    parse_todos(args)
        .map(|todos| {
            todos
                .iter()
                .filter(|t| matches!(t["status"].as_str(), Some("pending" | "in_progress")))
                .map(|t| format!("[{}] {}", t["status"].as_str().unwrap_or(""), t["content"].as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

/// The call's arguments with the todo list in canonical form (what the model is told and what the UI shows).
pub fn normalize_todo_args(args: Value) -> Value {
    match parse_todos(&args) {
        Ok(todos) => {
            let mut args = args;
            args["todos"] = Value::Array(todos);
            args
        }
        Err(_) => args,
    }
}

fn summary(todos: &[Value]) -> String {
    let count = |s: &str| todos.iter().filter(|t| t["status"] == s).count();
    format!(
        "{} item(s): {} completed, {} in progress, {} pending, {} cancelled",
        todos.len(),
        count("completed"),
        count("in_progress"),
        count("pending"),
        count("cancelled")
    )
}

/// Saves the list for `scope` (the task that owns it; `root` for the top-level run). With a store directory it is
/// also written to disk beside the task records, so it survives the next message (one process per message).
pub fn todowrite(scope: &str, args: &Value) -> String {
    write_todos(crate::delegation::store_path().as_deref(), scope, args)
}

pub fn todoread(scope: &str) -> String {
    read_todos(crate::delegation::store_path().as_deref(), scope)
}

fn write_todos(dir: Option<&Path>, scope: &str, args: &Value) -> String {
    let todos = match parse_todos(args) {
        Ok(t) => t,
        Err(errors) => return format!("Error: todo list rejected, nothing was saved:\n{}", errors.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")),
    };
    if let Some(dir) = dir {
        let _ = fs::create_dir_all(dir);
        let _ = fs::write(todo_file(dir, scope), Value::Array(todos.clone()).to_string());
    }
    todo_memory().lock().expect("todo table").insert(scope.to_string(), todos.clone());
    format!("Todo list updated: {}.\n{}", summary(&todos), serde_json::to_string_pretty(&todos).unwrap_or_default())
}

fn read_todos(dir: Option<&Path>, scope: &str) -> String {
    let current = todo_memory().lock().expect("todo table").get(scope).cloned().or_else(|| {
        let raw = fs::read_to_string(todo_file(dir?, scope)).ok()?;
        serde_json::from_str::<Vec<Value>>(&raw).ok()
    });
    match current {
        Some(todos) if !todos.is_empty() => format!("{}.\n{}", summary(&todos), serde_json::to_string_pretty(&todos).unwrap_or_default()),
        _ => "No todos yet.".into(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tools-{name}-{}-{}", std::process::id(), crate::delegation::new_task_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().to_string()
    }

    // ------------------------------------------------------------ Edit

    #[test]
    fn edit_replaces_a_unique_match_and_returns_a_diff() {
        let dir = tmp("edit1");
        let f = dir.join("a.txt");
        fs::write(&f, "one\ntwo\nthree\nfour\nfive\nsix\nseven\n").unwrap();
        let out = edit(&json!({ "file_path": s(&f), "old_string": "four", "new_string": "FOUR" }));
        assert_eq!(fs::read_to_string(&f).unwrap(), "one\ntwo\nthree\nFOUR\nfive\nsix\nseven\n");
        assert!(out.contains("@@ -1,7 +1,7 @@"), "{out}");
        assert!(out.contains("\n-four\n+FOUR\n"), "{out}");
        assert!(out.contains("\n three\n"), "context lines are kept: {out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_refuses_an_ambiguous_match_unless_replace_all() {
        let dir = tmp("edit2");
        let f = dir.join("a.txt");
        fs::write(&f, "x = 1\ny = 2\nx = 1\n").unwrap();
        let out = edit(&json!({ "file_path": s(&f), "old_string": "x = 1", "new_string": "x = 9" }));
        assert!(out.starts_with("Error: old_string was found 2 times"), "{out}");
        assert_eq!(fs::read_to_string(&f).unwrap(), "x = 1\ny = 2\nx = 1\n", "untouched");
        let out = edit(&json!({ "file_path": s(&f), "old_string": "x = 1", "new_string": "x = 9", "replace_all": true }));
        assert_eq!(fs::read_to_string(&f).unwrap(), "x = 9\ny = 2\nx = 9\n");
        assert!(out.matches("@@").count() >= 2 || out.contains("-x = 1"), "{out}");
        // The string "true" works too (weak models).
        fs::write(&f, "a\na\n").unwrap();
        edit(&json!({ "file_path": s(&f), "old_string": "a", "new_string": "b", "replace_all": "true" }));
        assert_eq!(fs::read_to_string(&f).unwrap(), "b\nb\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_tolerates_wrong_indentation_when_the_match_is_otherwise_unique() {
        let dir = tmp("edit3");
        let f = dir.join("a.rs");
        fs::write(&f, "fn main() {\n    let x = 1;\n    let y = 2;\n}\n").unwrap();
        // The model dropped the indentation.
        edit(&json!({ "file_path": s(&f), "old_string": "let x = 1;\nlet y = 2;", "new_string": "    let z = 3;" }));
        assert_eq!(fs::read_to_string(&f).unwrap(), "fn main() {\n    let z = 3;\n}\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_errors_are_specific_and_change_nothing() {
        let dir = tmp("edit4");
        let f = dir.join("a.txt");
        fs::write(&f, "hello\n").unwrap();
        let before = fs::read_to_string(&f).unwrap();
        assert!(edit(&json!({ "file_path": s(&f), "old_string": "nope", "new_string": "x" })).starts_with("Error: old_string was not found"));
        assert!(edit(&json!({ "file_path": s(&f), "old_string": "hello", "new_string": "hello" })).contains("identical"));
        assert!(edit(&json!({ "file_path": s(&f), "old_string": "", "new_string": "x" })).contains("already exists"));
        assert!(edit(&json!({ "file_path": s(&dir.join("missing.txt")), "old_string": "a", "new_string": "b" })).starts_with("Error reading"));
        assert!(edit(&json!({ "old_string": "a", "new_string": "b" })).starts_with("Error: Edit requires"));
        // Blank-only old_string must not match every blank line.
        fs::write(&f, "a\n\nb\n").unwrap();
        assert!(edit(&json!({ "file_path": s(&f), "old_string": "   ", "new_string": "x" })).starts_with("Error"));
        assert_eq!(fs::read_to_string(&f).unwrap(), "a\n\nb\n");
        assert_eq!(before, "hello\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_creates_a_new_file_from_an_empty_old_string() {
        let dir = tmp("edit5");
        let f = dir.join("sub/new.txt");
        let out = edit(&json!({ "file_path": s(&f), "old_string": "", "new_string": "fresh\n" }));
        assert_eq!(fs::read_to_string(&f).unwrap(), "fresh\n");
        assert!(out.starts_with("Created "), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_diff_groups_distant_changes_into_separate_hunks() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 3\n", "LINE 3\n").replace("line 38\n", "LINE 38\n");
        let d = unified_diff("f", &old, &new);
        assert_eq!(d.matches("\n@@ ").count(), 2, "{d}");
        assert!(!d.contains(" line 20\n"), "unchanged middle is not printed: {d}");
        assert_eq!(unified_diff("f", "same\n", "same\n"), "");
    }

    #[test]
    fn a_files_final_newline_is_not_an_extra_line() {
        let d = unified_diff("f", "a\nb\n", "a\nB\n");
        assert!(d.contains("@@ -1,2 +1,2 @@\n a\n-b\n+B\n"), "{d}");
        assert!(!d.ends_with("\n \n"), "no phantom blank context line: {d:?}");
        // No trailing newline at all behaves the same.
        assert!(unified_diff("f", "a\nb", "a\nB").contains("@@ -1,2 +1,2 @@"));
        // Adding only the final newline changes no line; Edit says so rather than returning nothing.
        let dir = tmp("nl");
        let f = dir.join("n.txt");
        fs::write(&f, "last line").unwrap();
        let out = edit(&json!({ "file_path": s(&f), "old_string": "last line", "new_string": "last line\n" }));
        assert!(out.contains("no line changed"), "{out}");
        assert_eq!(fs::read_to_string(&f).unwrap(), "last line\n");
        let _ = fs::remove_dir_all(dir);
    }

    // ------------------------------------------------------------ Grep / Glob

    fn project() -> PathBuf {
        let dir = tmp("proj");
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        fs::create_dir_all(dir.join("build")).unwrap();
        fs::write(dir.join(".gitignore"), "build/\n*.log\n").unwrap();
        fs::write(dir.join("src/a.ts"), "export const alpha = 1\nconst Beta = 2\n").unwrap();
        fs::write(dir.join("src/deep/b.ts"), "alpha again\n").unwrap();
        fs::write(dir.join("src/c.md"), "alpha in docs\n").unwrap();
        fs::write(dir.join("node_modules/x/i.js"), "alpha vendored\n").unwrap();
        fs::write(dir.join("build/out.js"), "alpha built\n").unwrap();
        fs::write(dir.join("trace.log"), "alpha log\n").unwrap();
        fs::write(dir.join("bin.dat"), b"alpha\0binary").unwrap();
        dir
    }

    #[test]
    fn grep_finds_matches_with_line_numbers_and_skips_ignored_and_binary_files() {
        let dir = project();
        let out = grep(&json!({ "pattern": "alpha", "path": s(&dir) }));
        assert!(out.starts_with("Found 3 matches\n"), "{out}");
        assert!(out.contains("a.ts:\n  Line 1: export const alpha = 1"), "{out}");
        assert!(out.contains("b.ts:\n  Line 1: alpha again"));
        assert!(out.contains("c.md:"));
        for hidden in ["node_modules", "build/out.js", "trace.log", "bin.dat"] {
            assert!(!out.contains(hidden), "{hidden} should be skipped: {out}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn grep_include_and_regex_and_errors() {
        let dir = project();
        let ts = grep(&json!({ "pattern": "alpha", "path": s(&dir), "include": "*.ts" }));
        assert!(ts.starts_with("Found 2 matches"), "{ts}");
        assert!(!ts.contains("c.md"));
        let nested = grep(&json!({ "pattern": "alpha", "path": s(&dir), "include": "src/deep/*.ts" }));
        assert!(nested.starts_with("Found 1 match\n") && nested.contains("b.ts"), "{nested}");
        let re = grep(&json!({ "pattern": "^const B[a-z]+ = \\d", "path": s(&dir) }));
        assert!(re.contains("Line 2: const Beta = 2"), "{re}");
        let ci = grep(&json!({ "pattern": "(?i)BETA", "path": s(&dir) }));
        assert!(ci.contains("Beta"), "{ci}");
        assert_eq!(grep(&json!({ "pattern": "zzz-nothing", "path": s(&dir) })), "No matches found.");
        assert!(grep(&json!({ "pattern": "(unclosed", "path": s(&dir) })).starts_with("Error: invalid regular expression"));
        assert!(grep(&json!({ "pattern": "a", "path": s(&dir.join("nope")) })).contains("does not exist"));
        assert!(grep(&json!({ "pattern": "" })).starts_with("Error: Grep requires"));
        // A single file works as the path.
        assert!(grep(&json!({ "pattern": "alpha", "path": s(&dir.join("src/a.ts")) })).starts_with("Found 1 match"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn grep_stops_at_a_hundred_matches_and_says_so() {
        let dir = tmp("many");
        let body: String = (0..150).map(|i| format!("hit {i}\n")).collect();
        fs::write(dir.join("big.txt"), body).unwrap();
        let out = grep(&json!({ "pattern": "hit", "path": s(&dir) }));
        assert!(out.starts_with("Found 100 matches (stopped at the first 100"), "{}", &out[..80]);
        assert_eq!(out.matches("  Line ").count(), 100);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_matches_names_at_any_depth_and_paths_with_a_slash() {
        let dir = project();
        let names = glob(&json!({ "pattern": "*.ts", "path": s(&dir) }));
        assert!(names.contains("a.ts") && names.contains("b.ts") && !names.contains("c.md"), "{names}");
        let deep = glob(&json!({ "pattern": "src/**/*.ts", "path": s(&dir) }));
        assert!(deep.contains("a.ts") && deep.contains("b.ts"), "{deep}");
        let only_top = glob(&json!({ "pattern": "src/*.ts", "path": s(&dir) }));
        assert!(only_top.contains("a.ts") && !only_top.contains("b.ts"), "{only_top}");
        let ignored = glob(&json!({ "pattern": "*.js", "path": s(&dir) }));
        assert_eq!(ignored, "No files found", "node_modules and build/ are skipped: {ignored}");
        assert!(glob(&json!({ "pattern": "[", "path": s(&dir) })).starts_with("Error: invalid glob"));
        assert!(glob(&json!({ "pattern": "*", "path": s(&dir.join("src/a.ts")) })).contains("not a directory"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_lists_the_newest_files_first() {
        let dir = tmp("mtime");
        let (old, new) = (dir.join("old.txt"), dir.join("new.txt"));
        fs::write(&old, "o").unwrap();
        fs::write(&new, "n").unwrap();
        let t = std::time::SystemTime::now();
        fs::File::options().write(true).open(&old).unwrap().set_modified(t - std::time::Duration::from_secs(3600)).unwrap();
        fs::File::options().write(true).open(&new).unwrap().set_modified(t).unwrap();
        let out = glob(&json!({ "pattern": "*.txt", "path": s(&dir) }));
        assert!(out.find("new.txt").unwrap() < out.find("old.txt").unwrap(), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    // ------------------------------------------------------------ WebFetch

    #[test]
    fn html_becomes_readable_markdown() {
        let html = r##"<!doctype html><html><head><title>T</title><style>p{color:red}</style></head><body>
<script>var secret = 1;</script><!-- a comment -->
<h1>Main &amp; Title</h1>
<p>Hello <strong>bold</strong> and <em>soft</em> with a <a href="https://example.com/x?a=1&amp;b=2">link</a> and <code>code()</code>.</p>
<ul><li>one</li><li>two <a href="#top">anchor</a></li></ul>
<pre>let a = 1;
  let b = 2;</pre>
<p>Line one<br>line two</p><img src="/i.png" alt="logo"><noscript>hidden</noscript>
</body></html>"##;
        let md = html_to_text(html, true);
        assert!(md.starts_with("# Main & Title"), "{md}");
        assert!(md.contains("Hello **bold** and *soft* with a [link](https://example.com/x?a=1&b=2) and `code()`."), "{md}");
        assert!(md.contains("- one\n- two [anchor]"), "in-page anchors lose their href: {md}");
        assert!(md.contains("```\nlet a = 1;\n  let b = 2;\n```"), "pre keeps its whitespace: {md}");
        assert!(md.contains("Line one\nline two"));
        assert!(md.contains("![logo](/i.png)"));
        for gone in ["secret", "color:red", "a comment", "hidden", "<"] {
            assert!(!md.contains(gone), "{gone} must not survive: {md}");
        }
        let text = html_to_text(html, false);
        assert!(text.starts_with("Main & Title") && !text.contains("**") && !text.contains("]("), "{text}");
        assert!(text.contains("link") && text.contains("- one"));
    }

    #[test]
    fn entities_decode_and_stray_ampersands_survive() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x41; &nbsp;x &mdash; AT&T &bogus; &"), "a & b <c> \"d\" 'e' A  x — AT&T &bogus; &");
    }

    pub(crate) async fn serve(responses: Vec<(&'static str, &'static str, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let responses = responses.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, ctype, body) = responses
                        .iter()
                        .find(|(p, _, _)| *p == path)
                        .map(|(_, c, b)| ("200 OK", *c, b.clone()))
                        .unwrap_or(("404 Not Found", "text/plain", b"missing".to_vec()));
                    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        base
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webfetch_returns_pages_in_the_requested_format() {
        let base = serve(vec![
            ("/page", "text/html; charset=utf-8", b"<html><body><h2>Hi</h2><p>a <a href=\"/x\">b</a></p></body></html>".to_vec()),
            ("/plain", "text/plain", b"just text".to_vec()),
            ("/pic", "image/png", vec![0x89, b'P', b'N', b'G']),
            ("/big", "text/plain", vec![b'x'; 70_000]),
        ])
        .await;
        assert_eq!(webfetch(&json!({ "url": format!("{base}/page") })).await, "## Hi\n\na [b](/x)");
        assert_eq!(webfetch(&json!({ "url": format!("{base}/page"), "format": "text" })).await, "Hi\n\na b");
        assert!(webfetch(&json!({ "url": format!("{base}/page"), "format": "html" })).await.starts_with("<html><body><h2>Hi</h2>"));
        assert_eq!(webfetch(&json!({ "url": format!("{base}/plain") })).await, "just text");
        assert!(webfetch(&json!({ "url": format!("{base}/pic") })).await.contains("is an image"));
        assert!(webfetch(&json!({ "url": format!("{base}/nope") })).await.contains("returned HTTP 404"));
        let big = webfetch(&json!({ "url": format!("{base}/big") })).await;
        assert!(big.contains("[truncated: 10000 more characters"), "capped to fit a model: {}", &big[big.len() - 120..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webfetch_only_fetches_web_urls_and_reports_failures() {
        assert!(webfetch(&json!({ "url": "file:///etc/passwd" })).await.starts_with("Error: WebFetch only fetches http or https"));
        assert!(webfetch(&json!({ "url": "ftp://example.com/x" })).await.contains("not `ftp`"));
        assert!(webfetch(&json!({ "url": "not a url" })).await.contains("is not a valid URL"));
        assert!(webfetch(&json!({ "url": "http://example.com", "format": "pdf" })).await.contains("format must be"));
        // Nothing listens on port 1.
        assert!(webfetch(&json!({ "url": "http://127.0.0.1:1/", "timeout": 2 })).await.starts_with("Error: request to"));
    }

    // ------------------------------------------------------------ Todos

    #[test]
    fn the_unfinished_items_of_a_todo_list_are_found_in_any_dialect() {
        let args = json!({ "todos": [
            { "content": "Investigate", "status": "doing" },
            { "content": "Draft the SCR", "status": "todo" },
            { "content": "Read the dialog", "status": "done" },
            { "content": "Old idea", "status": "cancelled" },
        ] });
        assert_eq!(unfinished_todos(&args), vec!["[in_progress] Investigate".to_string(), "[pending] Draft the SCR".to_string()]);
        assert!(unfinished_todos(&json!({ "todos": [{ "content": "x", "status": "completed" }] })).is_empty());
        assert!(unfinished_todos(&json!({ "nothing": 1 })).is_empty(), "a rejected list says nothing is open");
    }

    #[test]
    fn todos_are_normalized_and_bad_lists_are_rejected_whole() {
        let ok = parse_todos(&json!({ "todos": [
            { "content": "Write spec", "status": "in-progress", "priority": "HIGH" },
            { "task": "Ship it", "status": "Done" },
            { "content": "Later", "status": "todo", "id": "x9" },
        ]}))
        .unwrap();
        assert_eq!(ok[0], json!({ "id": "1", "content": "Write spec", "status": "in_progress", "priority": "high" }));
        assert_eq!(ok[1]["status"], "completed");
        assert_eq!(ok[1]["content"], "Ship it");
        assert_eq!(ok[2]["id"], "x9");
        assert_eq!(ok[2]["priority"], "medium");

        let bad = parse_todos(&json!({ "todos": [{ "content": "", "status": "pending" }, { "content": "x", "status": "weird" }] })).unwrap_err();
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(parse_todos(&json!({})).unwrap_err()[0].contains("todos is required"));
        // As a JSON string, and empty = clear.
        assert_eq!(parse_todos(&json!({ "todos": "[{\"content\":\"a\",\"status\":\"pending\"}]" })).unwrap().len(), 1);
        assert!(parse_todos(&json!({ "todos": [] })).unwrap().is_empty());
        // What the UI sees is the canonical form.
        let args = normalize_todo_args(json!({ "todos": [{ "content": "a", "status": "doing" }] }));
        assert_eq!(args["todos"][0]["status"], "in_progress");
    }

    #[test]
    fn todowrite_replaces_the_list_per_scope_and_todoread_returns_it() {
        let scope = format!("scope-{}", crate::delegation::new_task_id());
        assert_eq!(todoread(&scope), "No todos yet.");
        let out = todowrite(&scope, &json!({ "todos": [
            { "content": "a", "status": "completed" }, { "content": "b", "status": "in_progress" }, { "content": "c", "status": "pending" },
        ]}));
        assert!(out.starts_with("Todo list updated: 3 item(s): 1 completed, 1 in progress, 1 pending, 0 cancelled."), "{out}");
        assert!(todoread(&scope).contains("\"content\": \"b\""));
        // A rejected list saves nothing and keeps the old one.
        assert!(todowrite(&scope, &json!({ "todos": [{ "content": "", "status": "pending" }] })).starts_with("Error: todo list rejected, nothing was saved"));
        assert!(todoread(&scope).starts_with("3 item(s)"));
        // Another scope is separate; a full replace clears.
        assert_eq!(todoread("some-other-scope"), "No todos yet.");
        todowrite(&scope, &json!({ "todos": [] }));
        assert_eq!(todoread(&scope), "No todos yet.");
    }

    #[test]
    fn todos_survive_a_restart_when_a_store_is_configured() {
        let dir = tmp("todostore");
        let scope = format!("persist-{}", crate::delegation::new_task_id());
        write_todos(Some(&dir), &scope, &json!({ "todos": [{ "content": "kept", "status": "pending" }] }));
        assert!(dir.join(format!("todos-{scope}.json")).exists(), "written beside the task records");
        // A fresh process has no memory: forget it and read from disk.
        todo_memory().lock().unwrap().remove(&scope);
        assert!(read_todos(Some(&dir), &scope).contains("kept"));
        // A scope id cannot escape the directory.
        write_todos(Some(&dir), "../../evil", &json!({ "todos": [{ "content": "x", "status": "pending" }] }));
        assert!(dir.join("todos-______evil.json").exists());
        let _ = fs::remove_dir_all(dir);
    }
}
