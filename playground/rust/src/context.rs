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

/// What each model's window was last found to be (`None` = it did not say), so `/api/show` is asked once per model per process.
static KNOWN: Mutex<Option<HashMap<String, Option<u64>>>> = Mutex::new(None);

pub async fn context_of(cfg: &LlmConfig, model: &str) -> Option<u64> {
    if min_context() == 0 {
        return None;
    }
    if let Some(hit) = KNOWN.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(model).copied())) {
        return hit;
    }
    let found = crate::fetch_contexts(&cfg.base_url, &cfg.api_key, &[model.to_string()]).await.get(model).copied();
    if let Ok(mut guard) = KNOWN.lock() {
        guard.get_or_insert_with(HashMap::new).insert(model.to_string(), found);
    }
    found
}

fn too_small_message(model: &str, context: Option<u64>) -> String {
    format!(
        "model {model} has a context window of {} tokens, below the {} an agent needs (its prompt, tool results and conversation do not fit: the context is \
trimmed and reloaded again and again, and the agent drifts and invents things). Use a model with a larger window, or set NOMAD_MIN_CONTEXT=0 to allow it.",
        context.unwrap_or(0),
        min_context()
    )
}

/// The model a delegated agent or a fork should run on: `wanted`, or the fallback model when `wanted`'s window is below the floor.
/// An error only when the fallback is too small as well.
pub async fn usable_model(cfg: &LlmConfig, wanted: &str) -> Result<String, String> {
    let context = context_of(cfg, wanted).await;
    if !too_small(context) {
        return Ok(wanted.to_string());
    }
    let fallback = crate::opencode_config::load_adapter().default_fork_model;
    if fallback != wanted {
        let fallback_context = context_of(cfg, &fallback).await;
        if !too_small(fallback_context) {
            eprintln!(
                "context: model {wanted} has a window of {} tokens (< {}); running on {fallback} ({} tokens) instead",
                context.unwrap_or(0),
                min_context(),
                fallback_context.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
            );
            return Ok(fallback);
        }
    }
    Err(too_small_message(wanted, context))
}

/// For the model a run was asked to use (a person chose it, or it is the configured default): not replaced, refused.
pub async fn refuse_if_small(cfg: &LlmConfig, model: &str) -> Result<(), String> {
    let context = context_of(cfg, model).await;
    if too_small(context) { Err(too_small_message(model, context)) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_known_small_window_is_too_small() {
        assert!(too_small_for(32_769, Some(32_768)), "the 33K models are out");
        assert!(!too_small_for(32_769, Some(40_960)));
        assert!(!too_small_for(32_769, Some(262_144)));
        assert!(!too_small_for(32_769, None), "unknown is let through");
        assert!(!too_small_for(0, Some(2_048)), "0 switches the rule off");
    }
}
