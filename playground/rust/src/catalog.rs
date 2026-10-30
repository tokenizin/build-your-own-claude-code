//! `--catalog`: one JSON object describing models, agents and slash commands.

use crate::skills::Skill;
use serde_json::{Value, json};

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

pub fn is_agent(skill: &Skill) -> bool {
    skill.context.as_deref() == Some("fork")
}

pub fn build_catalog(
    default_model: &str,
    base_url: &str,
    skills: &[Skill],
    listed: Result<Vec<String>, String>,
) -> Value {
    let (listed, models_error) = match listed {
        Ok(list) => (list, Value::Null),
        Err(err) => (Vec::new(), Value::String(err)),
    };
    let models: Vec<Value> = merge_models(default_model, &listed, skills)
        .into_iter()
        .map(|id| json!({ "id": id, "name": id }))
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
        );
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
        let cat = build_catalog("m", "http://x/v1", &skills, Err("boom".into()));
        assert_eq!(cat["modelsError"], "boom");
        assert_eq!(cat["models"].as_array().unwrap().len(), 2);
        assert_eq!(cat["models"][0]["id"], "m");
        assert_eq!(cat["agents"][0]["model"], "qwen3:8b");
    }
}
