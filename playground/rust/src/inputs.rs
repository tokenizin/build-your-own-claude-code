//! CLI inputs for the CodeNomad engine contract: `--image` attachments and `--history`.

use base64::Engine as _;
use serde_json::{Value, json};
use std::path::Path;

/// Mime type for a supported image extension (case-insensitive).
pub fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// OpenAI `image_url` content part with a base64 data URL.
pub fn image_part_from_bytes(mime: &str, bytes: &[u8]) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime};base64,{b64}") }
    })
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Read + encode one image. `Err` carries a human-readable reason.
pub fn load_image_part(path: &Path) -> Result<Value, String> {
    let mime = image_mime(path).ok_or_else(|| {
        format!(
            "unsupported image type for {} (png, jpg, jpeg, gif, webp)",
            path.display()
        )
    })?;
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(image_part_from_bytes(mime, &bytes))
}

/// Build user content parts: text first, then one part per image (or a text note on failure).
/// `loaded` pairs each path with its load result; failures are reported via `warn`.
pub fn build_image_content(
    text: &str,
    loaded: &[(String, Result<Value, String>)],
    warn: &mut dyn FnMut(&str),
) -> Value {
    let mut parts = vec![json!({ "type": "text", "text": text })];
    for (name, result) in loaded {
        match result {
            Ok(part) => parts.push(part.clone()),
            Err(reason) => {
                warn(&format!("image: {reason}"));
                parts.push(json!({
                    "type": "text",
                    "text": format!("[image {name} could not be attached]")
                }));
            }
        }
    }
    Value::Array(parts)
}

/// Attach images to the last user message in `messages` (the message the run starts with).
pub fn attach_images_to_messages(
    messages: &mut [Value],
    loaded: &[(String, Result<Value, String>)],
    warn: &mut dyn FnMut(&str),
) {
    if loaded.is_empty() {
        return;
    }
    let Some(msg) = messages
        .iter_mut()
        .rev()
        .find(|m| m["role"].as_str() == Some("user"))
    else {
        return;
    };
    let text = match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    msg["content"] = build_image_content(&text, loaded, warn);
}

/// Load every `--image` path (pairs of display name + result).
pub fn load_images(paths: &[std::path::PathBuf]) -> Vec<(String, Result<Value, String>)> {
    paths
        .iter()
        .map(|p| (display_name(p), load_image_part(p)))
        .collect()
}

/// Parse `--history` JSON: array of `{role: user|assistant, content: string}`.
pub fn parse_history(text: &str) -> Result<Vec<Value>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("invalid --history JSON: {e}"))?;
    let items = value
        .as_array()
        .ok_or("invalid --history JSON: expected an array")?;
    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        let role = item["role"].as_str().unwrap_or("");
        if role != "user" && role != "assistant" {
            return Err(format!(
                "invalid --history entry {idx}: role must be \"user\" or \"assistant\""
            ));
        }
        let content = item["content"]
            .as_str()
            .ok_or_else(|| format!("invalid --history entry {idx}: content must be a string"))?;
        out.push(json!({ "role": role, "content": content }));
    }
    Ok(out)
}

/// Insert prior turns after any leading system messages, before the new user message(s).
pub fn insert_history(messages: &mut Vec<Value>, history: Vec<Value>) {
    if history.is_empty() {
        return;
    }
    let at = messages
        .iter()
        .position(|m| m["role"].as_str() != Some("system"))
        .unwrap_or(messages.len());
    messages.splice(at..at, history);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn mime_from_extension() {
        assert_eq!(image_mime(Path::new("a.png")), Some("image/png"));
        assert_eq!(image_mime(Path::new("a.JPG")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("a.gif")), Some("image/gif"));
        assert_eq!(image_mime(Path::new("a.webp")), Some("image/webp"));
        assert_eq!(image_mime(Path::new("a.bmp")), None);
        assert_eq!(image_mime(Path::new("noext")), None);
    }

    #[test]
    fn image_part_is_data_url() {
        let part = image_part_from_bytes("image/png", b"hi");
        assert_eq!(part["type"], "image_url");
        assert_eq!(part["image_url"]["url"], "data:image/png;base64,aGk=");
    }

    #[test]
    fn content_parts_with_failure_note() {
        let loaded = vec![
            ("ok.png".to_string(), Ok(image_part_from_bytes("image/png", b"x"))),
            ("bad.bmp".to_string(), Err("unsupported".to_string())),
        ];
        let mut warnings = Vec::new();
        let content = build_image_content("look", &loaded, &mut |w| warnings.push(w.to_string()));
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], json!({"type":"text","text":"look"}));
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[2]["text"], "[image bad.bmp could not be attached]");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn attaches_to_last_user_message() {
        let mut messages = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"first"}),
            json!({"role":"user","content":"second"}),
        ];
        let loaded = vec![("a.png".to_string(), Ok(image_part_from_bytes("image/png", b"x")))];
        attach_images_to_messages(&mut messages, &loaded, &mut |_| {});
        assert_eq!(messages[1]["content"], "first");
        assert_eq!(messages[2]["content"][0]["text"], "second");
        assert_eq!(messages[2]["content"][1]["type"], "image_url");
    }

    #[test]
    fn missing_file_is_error() {
        let r = load_image_part(&PathBuf::from("/nonexistent/x.png"));
        assert!(r.is_err());
    }

    #[test]
    fn history_parse_and_validate() {
        let h = parse_history(r#"[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]"#)
            .unwrap();
        assert_eq!(h.len(), 2);
        assert!(parse_history("[]").unwrap().is_empty());
        assert!(parse_history("{").is_err());
        assert!(parse_history(r#"{"role":"user"}"#).is_err());
        assert!(parse_history(r#"[{"role":"system","content":"x"}]"#).is_err());
        assert!(parse_history(r#"[{"role":"user","content":5}]"#).is_err());
    }

    #[test]
    fn history_inserted_after_systems() {
        let mut messages = vec![
            json!({"role":"system","content":"s1"}),
            json!({"role":"system","content":"s2"}),
            json!({"role":"user","content":"new"}),
        ];
        insert_history(
            &mut messages,
            vec![
                json!({"role":"user","content":"old"}),
                json!({"role":"assistant","content":"reply"}),
            ],
        );
        let roles: Vec<_> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "system", "user", "assistant", "user"]);
        assert_eq!(messages[2]["content"], "old");
        assert_eq!(messages[4]["content"], "new");

        let mut no_system = vec![json!({"role":"user","content":"new"})];
        insert_history(&mut no_system, vec![json!({"role":"user","content":"old"})]);
        assert_eq!(no_system[0]["content"], "old");
    }
}
