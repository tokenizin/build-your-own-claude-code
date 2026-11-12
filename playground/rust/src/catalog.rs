//! `--catalog`: one JSON object describing models, agents and slash commands.

use crate::skills::Skill;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Parse an OpenAI list response (`data[].id`).
pub fn parse_model_list(body: &Value) -> Result<Vec<String>, String> {
    let data = body["data"]
        .as_array()
        .ok_or("models response has no `data` array")?;
    Ok(data
        .iter()
        .filter_map(|m| m["id"].as_str())
        .map(str::to_string)
        .collect())
}

/// Default model first, then listed models, then skill/agent overrides; deduped.
pub fn merge_models(default_model: &str, listed: &[String], skills: &[Skill]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let candidates = std::iter::once(default_model.to_string())
        .chain(listed.iter().cloned())
        .chain(skills.iter().filter_map(|s| s.model.clone()));
    for id in candidates {
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// The context window an Ollama `/api/show` answer reports: the `num_ctx` the model is set to run with, else the
/// architecture's `<arch>.context_length`.
pub fn context_from_show(body: &Value) -> Option<u64> {
    let configured = body["parameters"].as_str().and_then(|text| {
        text.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("num_ctx")).then(|| words.next()?.parse::<u64>().ok()).flatten()
        })
    });
    let trained = body["model_info"]
        .as_object()
        .and_then(|info| info.iter().find(|(k, _)| k.ends_with(".context_length")).and_then(|(_, v)| v.as_u64()));
    configured.or(trained).filter(|n| *n > 0)
}

/// Takes the models whose context window is known and below `floor` out of a catalog (the pickers must not offer them), lists them under
/// `tooSmall`, and puts `fallback` in place of one as the default model or as an agent's model. `floor` 0 leaves the catalog as it is.
pub fn drop_small_contexts(catalog: &mut Value, floor: u64, fallback: &str) {
    if floor == 0 {
        return;
    }
    let small: Vec<String> = catalog["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter(|m| m["context"].as_u64().is_some_and(|c| c < floor))
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if small.is_empty() {
        return;
    }
    let dropped: Vec<Value> = catalog["models"]
        .as_array()
        .map(|models| models.iter().filter(|m| m["id"].as_str().is_some_and(|id| small.iter().any(|s| s == id))).cloned().collect())
        .unwrap_or_default();
    if let Some(models) = catalog["models"].as_array_mut() {
        models.retain(|m| !m["id"].as_str().is_some_and(|id| small.iter().any(|s| s == id)));
    }
    catalog["tooSmall"] = json!({ "floor": floor, "models": dropped });
    if catalog["model"].as_str().is_some_and(|m| small.iter().any(|s| s == m)) {
        catalog["model"] = json!(fallback);
    }
    if let Some(agents) = catalog["agents"].as_array_mut() {
        for agent in agents {
            if agent["model"].as_str().is_some_and(|m| small.iter().any(|s| s == m)) {
                agent["model"] = json!(fallback);
            }
        }
    }
}

pub fn is_agent(skill: &Skill) -> bool {
    skill.context.as_deref() == Some("fork")
}

pub fn build_catalog(
    default_model: &str,
    base_url: &str,
    skills: &[Skill],
    listed: Result<Vec<String>, String>,
    contexts: &HashMap<String, u64>,
) -> Value {
    let (listed, models_error) = match listed {
        Ok(list) => (list, Value::Null),
        Err(err) => (Vec::new(), Value::String(err)),
    };
    let models: Vec<Value> = merge_models(default_model, &listed, skills)
        .into_iter()
        .map(|id| match contexts.get(&id) {
            Some(context) => json!({ "id": id, "name": id, "context": context }),
            None => json!({ "id": id, "name": id }),
        })
        .collect();

    let mut sorted: Vec<&Skill> = skills.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let agents: Vec<Value> = sorted
        .iter()
        .filter(|s| is_agent(s))
        .map(|s| json!({ "name": s.name, "description": s.description, "model": s.model }))
        .collect();
    let commands: Vec<Value> = sorted
        .iter()
        .map(|s| json!({ "name": s.name, "description": s.description, "agent": is_agent(s) }))
        .collect();

    json!({
        "version": 1,
        "model": default_model,
        "baseUrl": base_url,
        "models": models,
        "modelsError": models_error,
        "agents": agents,
        "commands": commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, fork: bool, model: Option<&str>) -> Skill {
        Skill {
            name: name.into(),
            description: format!("{name} desc"),
            body: String::new(),
            directory: PathBuf::from("/tmp"),
            context: fork.then(|| "fork".to_string()),
            policies: Vec::new(),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn parses_openai_list() {
        let body = json!({"object":"list","data":[{"id":"a"},{"id":"b"}]});
        assert_eq!(parse_model_list(&body).unwrap(), vec!["a", "b"]);
        assert!(parse_model_list(&json!({"x":1})).is_err());
    }

    #[test]
    fn a_model_below_the_floor_is_not_offered_and_agents_move_to_the_fallback() {
        let skills = vec![skill("developer", true, Some("qwen2.5:14b")), skill("pma", true, Some("qwen3:8b")), skill("cto", true, Some("gemma4:26b"))];
        let contexts = HashMap::from([("qwen2.5:14b".to_string(), 32_768), ("qwen3:8b".to_string(), 40_960), ("gemma4:26b".to_string(), 262_144)]);
        let mut cat = build_catalog("qwen2.5:14b", "http://x/v1", &skills, Ok(vec!["qwen2.5:14b".into(), "qwen3:8b".into(), "gemma4:26b".into()]), &contexts);
        drop_small_contexts(&mut cat, 32_769, "gemma4:26b");
        let mut ids: Vec<&str> = cat["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["gemma4:26b", "qwen3:8b"], "the 33K model is gone, the other two stay");
        assert_eq!(cat["tooSmall"]["models"][0]["id"], "qwen2.5:14b");
        assert_eq!(cat["model"], "gemma4:26b", "the default model was a small one");
        let model_of = |name: &str| cat["agents"].as_array().unwrap().iter().find(|a| a["name"] == name).unwrap()["model"].clone();
        assert_eq!(model_of("developer"), "gemma4:26b", "an agent on a small model moves to the fallback");
        assert_eq!(model_of("pma"), "qwen3:8b", "40,960 is above the floor");
        // Floor 0: the catalog is left alone.
        let mut all = build_catalog("qwen2.5:14b", "http://x/v1", &skills, Ok(vec!["qwen2.5:14b".into()]), &contexts);
        drop_small_contexts(&mut all, 0, "gemma4:26b");
        assert!(all.get("tooSmall").is_none() && all["model"] == "qwen2.5:14b");
    }

    #[test]
    fn catalog_shape() {
        let skills = vec![
            skill("zeta", false, None),
            skill("pma", true, Some("qwen3:8b")),
            skill("alpha", false, Some("gemma4:31b-cloud")),
        ];
        let cat = build_catalog(
            "gemma4:31b-cloud",
            "http://x/v1",
            &skills,
            Ok(vec!["llava:13b".into(), "gemma4:31b-cloud".into()]),
            &HashMap::from([("llava:13b".to_string(), 4096)]),
        );
        assert_eq!(cat["models"][1]["context"], 4096);
        assert!(cat["models"][0].get("context").is_none(), "unknown stays unknown");
        assert_eq!(cat["version"], 1);
        assert_eq!(cat["modelsError"], Value::Null);
        let ids: Vec<_> = cat["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["gemma4:31b-cloud", "llava:13b", "qwen3:8b"]);
        assert_eq!(cat["models"][0]["name"], "gemma4:31b-cloud");
        assert_eq!(cat["agents"].as_array().unwrap().len(), 1);
        assert_eq!(cat["agents"][0]["name"], "pma");
        assert_eq!(cat["agents"][0]["model"], "qwen3:8b");
        let names: Vec<_> = cat["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "pma", "zeta"]);
        assert_eq!(cat["commands"][1]["agent"], true);
        assert_eq!(cat["commands"][0]["agent"], false);
    }

    #[test]
    fn catalog_models_error_keeps_defaults() {
        let skills = vec![skill("pma", true, Some("qwen3:8b"))];
        let cat = build_catalog("m", "http://x/v1", &skills, Err("boom".into()), &HashMap::new());
        assert_eq!(cat["modelsError"], "boom");
        assert_eq!(cat["models"].as_array().unwrap().len(), 2);
        assert_eq!(cat["models"][0]["id"], "m");
        assert_eq!(cat["agents"][0]["model"], "qwen3:8b");
    }

    #[test]
    fn context_window_from_ollama_show() {
        let trained = json!({ "model_info": { "general.architecture": "qwen2", "qwen2.context_length": 32768 }, "parameters": "stop \"<|im_end|>\"" });
        assert_eq!(context_from_show(&trained), Some(32768));
        let configured = json!({ "model_info": { "llama.context_length": 131072 }, "parameters": "num_ctx                        16384\nstop \"x\"" });
        assert_eq!(context_from_show(&configured), Some(16384), "the configured window is what the model runs with");
        assert_eq!(context_from_show(&json!({ "model_info": {} })), None);
        assert_eq!(context_from_show(&json!({ "error": "model not found" })), None);
    }
}
