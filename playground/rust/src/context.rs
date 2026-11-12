//! A floor under the context window of the models agents run on.
//!
//! A model with a window of 32,768 tokens (the "33K" models, `qwen2.5:14b` for one) cannot hold an agent's prompt (the Product
//! Manager's is ~16K tokens), its tool results and the conversation: the context is trimmed and reloaded again and again to stay near
//! the limit, and the agent drifts and invents things. `NOMAD_MIN_CONTEXT` (tokens, default 32769, `0` switches the rule off) is the
//! smallest window an agent may run on:
//!
//! * a delegated agent or a fork whose model is below it runs on the fallback model instead (`defaultForkModel`, `gemma4:26b`);
//! * a run whose own model is below it stops with a message that says so, rather than quietly running on something else;
//! * the catalog does not offer such models (see `catalog::drop_small_contexts`).
//!
//! Only a window that is KNOWN and small counts: a model that did not answer `/api/show` (a cloud model, an endpoint that is not Ollama)
//! is let through, as there is nothing to go on.
use crate::config::LlmConfig;
use std::{collections::HashMap, env, sync::Mutex};

pub const DEFAULT_MIN_CONTEXT: u64 = 32_769;

pub fn min_context() -> u64 {
    env::var("NOMAD_MIN_CONTEXT").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_MIN_CONTEXT)
}

pub fn too_small_for(floor: u64, context: Option<u64>) -> bool {
    floor > 0 && context.is_some_and(|c| c < floor)
}

pub fn too_small(context: Option<u64>) -> bool {
    too_small_for(min_context(), context)
}

/// What the endpoint said about a model: it does not have it, or it has it with this window (`None` = it did not say).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Seen {
    Missing,
    Window(Option<u64>),
}

/// What each model was last found to be, so `/api/show` is asked once per model per process.
static KNOWN: Mutex<Option<HashMap<String, Seen>>> = Mutex::new(None);

/// Ollama's answer to `/api/show` for a model it does not have: HTTP 404 and `{"error":"model \"x\" not found, try pulling it first"}`. A 404
/// that is not that (an endpoint with no `/api/show` at all answers `404 page not found` for everything) says nothing about the model.
pub fn says_model_missing(status: u16, body: &str) -> bool {
    let lower = body.to_lowercase();
    status == 404 && lower.contains("model") && lower.contains("not found")
}

async fn seen(cfg: &LlmConfig, model: &str) -> Seen {
    if let Some(hit) = KNOWN.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(model).copied())) {
        return hit;
    }
    let root = cfg.base_url.trim_end_matches('/').trim_end_matches("/v1").to_string();
    let answer = reqwest::Client::new()
        .post(format!("{root}/api/show"))
        .bearer_auth(&cfg.api_key)
        .json(&serde_json::json!({ "model": model }))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await;
    let found = match answer {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if says_model_missing(status, &text) {
                Seen::Missing
            } else if status < 300 {
                Seen::Window(serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|body| crate::catalog::context_from_show(&body)))
            } else {
                Seen::Window(None)
            }
        }
        Err(_) => Seen::Window(None),
    };
    if let Ok(mut guard) = KNOWN.lock() {
        guard.get_or_insert_with(HashMap::new).insert(model.to_string(), found);
    }
    found
}

/// Of `models`, those the endpoint says it does not have (`/api/show` answers "not found"): not those that are merely absent from the model
/// list (a cloud model can be served and not listed) and not those it did not answer for.
pub async fn missing_among(cfg: &LlmConfig, models: &[String]) -> Vec<String> {
    use futures_util::StreamExt;
    futures_util::stream::iter(models.iter().take(80).cloned())
        .map(|model| async move { (seen(cfg, &model).await == Seen::Missing).then_some(model) })
        .buffer_unordered(8)
        .filter_map(|found| async move { found })
        .collect()
        .await
}

fn unusable(seen: Seen) -> bool {
    match seen {
        Seen::Missing => true,
        Seen::Window(context) => too_small(context),
    }
}

fn refusal(model: &str, seen: Seen) -> String {
    match seen {
        Seen::Missing => format!(
            "model {model} is not served by the endpoint (it answered that the model was not found). Use a model the endpoint lists; a stale or invented name \
(for example a legacy alias that no longer exists) fails every request."
        ),
        Seen::Window(context) => format!(
            "model {model} has a context window of {} tokens, below the {} an agent needs (its prompt, tool results and conversation do not fit: the context is \
trimmed and reloaded again and again, and the agent drifts and invents things). Use a model with a larger window, or set NOMAD_MIN_CONTEXT=0 to allow it.",
            context.unwrap_or(0),
            min_context()
        ),
    }
}

/// The model a delegated agent or a fork should run on: `wanted`, or the fallback model when `wanted` is not served by the endpoint or its window
/// is below the floor. An error only when the fallback is unusable as well.
pub async fn usable_model(cfg: &LlmConfig, wanted: &str) -> Result<String, String> {
    let first = seen(cfg, wanted).await;
    if !unusable(first) {
        return Ok(wanted.to_string());
    }
    let fallback = crate::opencode_config::load_adapter().default_fork_model;
    if fallback != wanted {
        let second = seen(cfg, &fallback).await;
        if !unusable(second) {
            let why = match first {
                Seen::Missing => "is not served by the endpoint".to_string(),
                Seen::Window(context) => format!("has a window of {} tokens (< {})", context.unwrap_or(0), min_context()),
            };
            eprintln!("context: model {wanted} {why}; running on {fallback} instead");
            return Ok(fallback);
        }
    }
    Err(refusal(wanted, first))
}

/// For the model a run was asked to use (a person chose it, or it is the configured default): not replaced, refused.
pub async fn refuse_if_unusable(cfg: &LlmConfig, model: &str) -> Result<(), String> {
    let found = seen(cfg, model).await;
    if unusable(found) { Err(refusal(model, found)) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ollamas_own_not_found_answer_counts_as_a_missing_model() {
        assert!(says_model_missing(404, "{\"error\":\"model \\\"ornith-31b-dense:latest\\\" not found, try pulling it first\"}"));
        assert!(!says_model_missing(404, "404 page not found"), "an endpoint with no /api/show says that about everything");
        assert!(!says_model_missing(200, "{\"error\":\"model not found\"}"));
        assert!(!says_model_missing(500, "model not found"));
    }

    #[test]
    fn only_a_known_small_window_is_too_small() {
        assert!(too_small_for(32_769, Some(32_768)), "the 33K models are out");
        assert!(!too_small_for(32_769, Some(40_960)));
        assert!(!too_small_for(32_769, Some(262_144)));
        assert!(!too_small_for(32_769, None), "unknown is let through");
        assert!(!too_small_for(0, Some(2_048)), "0 switches the rule off");
    }
}
