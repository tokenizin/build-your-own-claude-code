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

/// Takes the models the endpoint says it does not have (`missing`, see `context::missing_among`) out of a catalog (a stale or invented name cannot be
/// run, and the pickers must not offer it), lists them under `notServed`, and puts `fallback` in their place as the default model or as an agent's
/// model. A model that is only absent from the model list is NOT missing: a cloud model can be served and not listed.
pub fn drop_missing(catalog: &mut Value, missing: &[String], fallback: &str) {
    if missing.is_empty() {
        return;
    }
    let is_missing = |m: &str| missing.iter().any(|x| x == m);
    if let Some(models) = catalog["models"].as_array_mut() {
        models.retain(|m| !m["id"].as_str().is_some_and(is_missing));
    }
    catalog["notServed"] = json!(missing);
    if catalog["model"].as_str().is_some_and(is_missing) {
        catalog["model"] = json!(fallback);
    }
    if let Some(agents) = catalog["agents"].as_array_mut() {
        for agent in agents {
            if agent["model"].as_str().is_some_and(is_missing) {
                agent["model"] = json!(fallback);
            }
        }
    }
}

/// Takes the models that cannot serve the agent out of a catalog: the agent sends its tools with every request, so a model whose `/api/show` answer lists
/// capabilities without `tools` (a vision-only or text-only model) is refused by the endpoint every time, and an embedding model does not chat. They are
/// listed under `unsupported` with the reason, and `fallback` replaces one as the default model or as an agent's model. A model that did not answer
/// (a cloud model, another kind of endpoint) is kept: nothing says it cannot.
pub fn drop_unsupported(catalog: &mut Value, shows: &HashMap<String, Value>, fallback: &str) {
    let why = |id: &str| -> Option<&'static str> {
        let caps = shows.get(id)?["capabilities"].as_array()?.iter().filter_map(|c| c.as_str()).collect::<Vec<_>>();
        if caps.is_empty() {
            return None;
        }
        if caps.contains(&"embedding") {
            Some("an embedding model")
        } else if !caps.contains(&"tools") {
            Some("no tool calling")
        } else {
            None
        }
    };
    let dropped: Vec<(String, &'static str)> = catalog["models"]
        .as_array()
        .map(|models| models.iter().filter_map(|m| m["id"].as_str()).filter_map(|id| why(id).map(|reason| (id.to_string(), reason))).collect())
        .unwrap_or_default();
    let agent_or_default_unsupported = catalog["model"].as_str().is_some_and(|m| why(m).is_some())
        || catalog["agents"].as_array().is_some_and(|agents| agents.iter().any(|a| a["model"].as_str().is_some_and(|m| why(m).is_some())));
    if dropped.is_empty() && !agent_or_default_unsupported {
        return;
    }
    if let Some(models) = catalog["models"].as_array_mut() {
        models.retain(|m| !m["id"].as_str().is_some_and(|id| why(id).is_some()));
    }
    catalog["unsupported"] = json!(dropped.iter().map(|(id, reason)| json!({ "id": id, "why": reason })).collect::<Vec<_>>());
    if catalog["model"].as_str().is_some_and(|m| why(m).is_some()) {
        catalog["model"] = json!(fallback);
    }
    if let Some(agents) = catalog["agents"].as_array_mut() {
        for agent in agents {
            if agent["model"].as_str().is_some_and(|m| why(m).is_some()) {
                agent["model"] = json!(fallback);
            }
        }
    }
}

/// The thinking options a model offers, from its `/api/show` answer (`capabilities` has `thinking`): `off` and `on` for a model that can think or not
/// (Ollama reads low, medium and high alike there, so they are not offered), `low` / `medium` / `high` for gpt-oss (the one family where the level
/// changes the depth, and that cannot be switched off), none for a model without a thinking mode or one that always thinks (`deepseek-r1`).
pub fn thinking_options(model_id: &str, show: &Value) -> Vec<&'static str> {
    let can_think = show["capabilities"].as_array().is_some_and(|caps| caps.iter().any(|c| c == "thinking"));
    if !can_think {
        return Vec::new();
    }
    let name = model_id.rsplit('/').next().unwrap_or(model_id).to_ascii_lowercase();
    if name.starts_with("gpt-oss") {
        vec!["low", "medium", "high"]
    } else if name.starts_with("deepseek-r1") {
        // Measured on the endpoint: it reasons with `reasoning_effort: none` too, so an "Off" would be a lie. Only the default.
        Vec::new()
    } else {
        vec!["off", "on"]
    }
}

/// Puts each model's thinking options (see `thinking_options`) in the catalog as `thinking`; a model with none gets no field.
pub fn add_thinking(catalog: &mut Value, shows: &HashMap<String, Value>) {
    if let Some(models) = catalog["models"].as_array_mut() {
        for model in models {
            let Some(id) = model["id"].as_str().map(str::to_string) else { continue };
            let options = shows.get(&id).map(|show| thinking_options(&id, show)).unwrap_or_default();
            if !options.is_empty() {
                model["thinking"] = json!(options);
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
    fn a_model_the_endpoint_says_it_does_not_have_is_not_offered_and_agents_move_to_the_fallback() {
        let skills = vec![skill("ui_ux_designer", true, Some("ornith-31b-dense:latest")), skill("developer", true, Some("gemma4:26b")), skill("cto", true, Some("gemma4:31b-cloud"))];
        let listed = vec!["gemma4:26b".to_string(), "ornith-31b-dense:latest".to_string()];
        let mut cat = build_catalog("gemma4:31b-cloud", "http://x/v1", &skills, Ok(listed), &HashMap::new());
        drop_missing(&mut cat, &["ornith-31b-dense:latest".to_string()], "gemma4:26b");
        let ids: Vec<&str> = cat["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(!ids.contains(&"ornith-31b-dense:latest"), "{ids:?}");
        assert!(ids.contains(&"gemma4:31b-cloud"), "a served cloud model that is not in the list stays: {ids:?}");
        assert_eq!(cat["notServed"][0], "ornith-31b-dense:latest");
        assert_eq!(cat["model"], "gemma4:31b-cloud", "the default is served, so it stays");
        let model_of = |name: &str| cat["agents"].as_array().unwrap().iter().find(|a| a["name"] == name).unwrap()["model"].clone();
        assert_eq!(model_of("ui_ux_designer"), "gemma4:26b", "the agent pinned to a model that is not there moves to the fallback");
        assert_eq!(model_of("cto"), "gemma4:31b-cloud");
        // Nothing missing: the catalog is left alone.
        let mut untouched = build_catalog("gemma4:26b", "http://x/v1", &skills, Ok(vec![]), &HashMap::new());
        drop_missing(&mut untouched, &[], "gemma4:26b");
        assert!(untouched.get("notServed").is_none());
    }

    #[test]
    fn a_model_that_can_think_offers_off_and_on_and_one_that_cannot_offers_nothing() {
        let thinker = json!({ "capabilities": ["completion", "tools", "thinking"] });
        let plain = json!({ "capabilities": ["completion", "tools"] });
        assert_eq!(thinking_options("gemma4:26b", &thinker), vec!["off", "on"]);
        assert_eq!(thinking_options("gpt-oss:20b", &thinker), vec!["low", "medium", "high"], "the family where the level matters");
        assert_eq!(thinking_options("ollama/gpt-oss:120b", &thinker), vec!["low", "medium", "high"]);
        assert!(thinking_options("deepseek-r1:32b", &thinker).is_empty(), "it always thinks: no Off to offer");
        assert!(thinking_options("hermes3:latest", &plain).is_empty());
        assert!(thinking_options("x", &json!({})).is_empty(), "no capabilities answer: no options");

        let skills: Vec<Skill> = Vec::new();
        let mut cat = build_catalog("gemma4:26b", "http://x/v1", &skills, Ok(vec!["gemma4:26b".into(), "hermes3:latest".into(), "unknown:1b".into()]), &HashMap::new());
        add_thinking(&mut cat, &HashMap::from([("gemma4:26b".to_string(), thinker), ("hermes3:latest".to_string(), plain)]));
        let get = |id: &str| cat["models"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap().clone();
        assert_eq!(get("gemma4:26b")["thinking"], json!(["off", "on"]));
        assert!(get("hermes3:latest").get("thinking").is_none() && get("unknown:1b").get("thinking").is_none());
    }

    #[test]
    fn a_model_without_tool_calling_or_an_embedding_model_is_not_offered() {
        let skills = vec![skill("designer", true, Some("qwen2.5vl:7b")), skill("dev", true, Some("gemma4:26b"))];
        let listed: Vec<String> = ["gemma4:26b", "qwen2.5vl:7b", "qwen3-embedding:0.6b", "gemma4:31b-cloud"].iter().map(|s| s.to_string()).collect();
        let shows = HashMap::from([
            ("gemma4:26b".to_string(), json!({ "capabilities": ["completion", "vision", "tools", "thinking"] })),
            ("qwen2.5vl:7b".to_string(), json!({ "capabilities": ["completion", "vision"] })),
            ("qwen3-embedding:0.6b".to_string(), json!({ "capabilities": ["tools", "thinking", "embedding"] })),
        ]);
        let mut cat = build_catalog("gemma4:26b", "http://x/v1", &skills, Ok(listed), &HashMap::new());
        drop_unsupported(&mut cat, &shows, "gemma4:26b");
        let mut ids: Vec<&str> = cat["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["gemma4:26b", "gemma4:31b-cloud"], "a model that did not answer is kept");
        let why: Vec<(String, String)> = cat["unsupported"].as_array().unwrap().iter().map(|u| (u["id"].as_str().unwrap().into(), u["why"].as_str().unwrap().into())).collect();
        assert!(why.contains(&("qwen2.5vl:7b".into(), "no tool calling".into())) && why.contains(&("qwen3-embedding:0.6b".into(), "an embedding model".into())), "{why:?}");
        let model_of = |name: &str| cat["agents"].as_array().unwrap().iter().find(|a| a["name"] == name).unwrap()["model"].clone();
        assert_eq!(model_of("designer"), "gemma4:26b", "an agent on a model that cannot do tools moves to the fallback");
        assert_eq!(model_of("dev"), "gemma4:26b");
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
