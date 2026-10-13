use std::env;

/// Prefer first non-empty env var among `keys`, else `default`.
pub fn env_first(keys: &[&str], default: &str) -> String {
    for key in keys {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    default.to_string()
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub base_url: String,
    pub fallback_base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
}

impl LlmConfig {
    pub fn with_model(&self, model: impl Into<String>) -> Self {
        let mut cloned = self.clone();
        cloned.model = model.into();
        cloned
    }
}

impl LlmConfig {
    pub fn from_env() -> Self {
        // Primary: Mac Studio Ollama via Cloudflare tunnel (OpenAI-compatible /v1).
        // Fallback: local daemon on the Studio (same models, including *:cloud).
        let base_url = env_first(
            &[
                "OLLAMA_BASE_URL",
                "LLM_BASE_URL",
                "OPENROUTER_BASE_URL", // legacy alias
            ],
            "https://ollama.tokenizin.com/v1",
        );
        let fallback_base_url = env_first(
            &["OLLAMA_FALLBACK_BASE_URL", "LLM_FALLBACK_BASE_URL"],
            "http://127.0.0.1:11434/v1",
        );
        let api_key = env_first(
            &["OLLAMA_API_KEY", "LLM_API_KEY", "OPENROUTER_API_KEY"],
            "ollama",
        );
        let model = env_first(
            &["OLLAMA_MODEL", "LLM_MODEL", "OPENROUTER_MODEL"],
            "gemma4:31b-cloud",
        );
        let max_tokens = env_first(
            &["OLLAMA_MAX_TOKENS", "LLM_MAX_TOKENS", "OPENROUTER_MAX_TOKENS"],
            "2048",
        )
        .parse()
        .unwrap_or(2048);

        Self {
            base_url: normalize_v1_base(&base_url),
            fallback_base_url: normalize_v1_base(&fallback_base_url),
            api_key,
            model,
            max_tokens,
        }
    }
}

fn normalize_v1_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_v1() {
        assert_eq!(
            normalize_v1_base("https://ollama.tokenizin.com"),
            "https://ollama.tokenizin.com/v1"
        );
        assert_eq!(
            normalize_v1_base("http://127.0.0.1:11434/v1/"),
            "http://127.0.0.1:11434/v1"
        );
    }
}
