//! Every AI provider configured for OpenCode / CodeNomad, and the endpoint each `provider/model` runs on.
//!
//! The engine talks OpenAI chat-completions. Ollama (`OLLAMA_BASE_URL`, see config.rs) is the default endpoint: a bare model name
//! (`gemma4:26b`) or an `ollama/…` one runs there, as before. A model written `provider/model` whose provider is known here runs on that
//! provider's own endpoint, with its own key, under the provider's id for the model.
//!
//! Where providers come from, later sources filling what earlier ones left out:
//!
//! 1. the `provider` section of every opencode.json(c) found: `OPENCODE_CONFIG`, CodeNomad's `.opencode/opencode.jsonc`, StarWorld's
//!    `opencode.json` and `.opencode/opencode.json`, and the global `~/.config/opencode/opencode.json(c)`. An entry gives
//!    `options.baseURL`, `options.apiKey` (`{env:VAR}` and `{file:path}` are expanded), `env` (key variables) and `models`
//!    (`{ "<id>": { "id": "<upstream id>", "name": …, "limit": { "context": … } } }`);
//! 2. OpenCode's stored keys (`~/.local/share/opencode/auth.json`, entries of `"type": "api"`);
//! 3. the well-known OpenAI-compatible providers below (base URL and key variable), so `openrouter/…` works with only
//!    `OPENROUTER_API_KEY` set.
//!
//! A provider is *ready* when it has a base URL and a key (or is a local endpoint, which needs none). A `provider/model` whose provider
//! is not ready is not routed: opencode_config.rs then puts the fork model in its place, as it always did, and says why on stderr.
use crate::opencode_config::strip_jsonc;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The providers that are the default Ollama endpoint: their models are not routed anywhere else.
pub const DEFAULT_PROVIDER_IDS: &[&str] = &["ollama"];

/// Well-known OpenAI-compatible providers: id, display name, base URL, key variables.
const BUILTIN: &[(&str, &str, &str, &[&str])] = &[
    ("openai", "OpenAI", "https://api.openai.com/v1", &["OPENAI_API_KEY"]),
    ("anthropic", "Anthropic", "https://api.anthropic.com/v1", &["ANTHROPIC_API_KEY"]),
    ("openrouter", "OpenRouter", "https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"]),
    ("opencode", "OpenCode Zen", "https://opencode.ai/zen/v1", &["OPENCODE_API_KEY"]),
    ("groq", "Groq", "https://api.groq.com/openai/v1", &["GROQ_API_KEY"]),
    ("deepseek", "DeepSeek", "https://api.deepseek.com/v1", &["DEEPSEEK_API_KEY"]),
    ("mistral", "Mistral", "https://api.mistral.ai/v1", &["MISTRAL_API_KEY"]),
    ("xai", "xAI", "https://api.x.ai/v1", &["XAI_API_KEY"]),
    ("google", "Google", "https://generativelanguage.googleapis.com/v1beta/openai", &["GEMINI_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"]),
    ("togetherai", "Together AI", "https://api.together.xyz/v1", &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"]),
    ("fireworks", "Fireworks", "https://api.fireworks.ai/inference/v1", &["FIREWORKS_API_KEY"]),
    ("cerebras", "Cerebras", "https://api.cerebras.ai/v1", &["CEREBRAS_API_KEY"]),
];

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderModel {
    /// The id OpenCode uses after `provider/`.
    pub id: String,
    /// The id the provider's API wants (`models.<id>.id`, else `id`).
    pub upstream: String,
    pub name: String,
    pub context: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// The variables a key is read from (for the catalog, and to say which one is missing).
    pub key_env: Vec<String>,
    pub models: Vec<ProviderModel>,
    /// Where it was found: config paths, `auth.json`, `builtin`.
    pub sources: Vec<String>,
}

impl Provider {
    pub fn is_default(&self) -> bool {
        DEFAULT_PROVIDER_IDS.contains(&self.id.as_str())
    }

    /// A local endpoint (localhost, 127.0.0.1, a LAN name without dots…) needs no key.
    fn is_local(&self) -> bool {
        self.base_url.as_deref().is_some_and(|url| {
            let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or("");
            host == "localhost" || host.starts_with("127.") || host == "0.0.0.0" || host == "[::1]" || !host.contains('.')
        })
    }

    pub fn ready(&self) -> bool {
        !self.is_default() && self.base_url.is_some() && (self.api_key.is_some() || self.is_local())
    }

    /// Why it is not ready, for stderr and the catalog.
    pub fn not_ready_reason(&self) -> Option<String> {
        if self.is_default() || self.ready() {
            return None;
        }
        if self.base_url.is_none() {
            return Some(format!("provider `{}` has no base URL (set options.baseURL in opencode.json)", self.id));
        }
        Some(if self.key_env.is_empty() {
            format!("provider `{}` has no API key (set options.apiKey in opencode.json)", self.id)
        } else {
            format!("provider `{}` has no API key (set {})", self.id, self.key_env.join(" or "))
        })
    }

    fn model(&self, id: &str) -> Option<&ProviderModel> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// Where one request goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    /// The model id sent in the request.
    pub model: String,
    pub context: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub providers: BTreeMap<String, Provider>,
}

impl Registry {
    /// Where `model` runs, when it is `provider/model` and that provider is ready. `None`: the default endpoint.
    pub fn route(&self, model: &str) -> Option<Route> {
        let (prefix, rest) = model.trim().split_once('/')?;
        let provider = self.providers.get(prefix).filter(|p| p.ready())?;
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        let known = provider.model(rest);
        Some(Route {
            provider: provider.id.clone(),
            base_url: provider.base_url.clone()?,
            // A local endpoint is sent a placeholder: some servers reject a request with no Authorization header at all.
            api_key: provider.api_key.clone().unwrap_or_else(|| "none".into()),
            model: known.map(|m| m.upstream.clone()).unwrap_or_else(|| rest.to_string()),
            context: known.and_then(|m| m.context),
        })
    }

    /// The provider named by `model`'s prefix, when there is one and it is not the default endpoint.
    pub fn provider_of(&self, model: &str) -> Option<&Provider> {
        let (prefix, _) = model.trim().split_once('/')?;
        self.providers.get(prefix).filter(|p| !p.is_default())
    }

    /// Adds `incoming`, keeping what is already known and filling what is not.
    pub fn merge(&mut self, incoming: Provider) {
        let entry = self.providers.entry(incoming.id.clone()).or_insert_with(|| Provider { id: incoming.id.clone(), ..Provider::default() });
        if (entry.name.is_empty() || entry.name == entry.id) && !incoming.name.is_empty() {
            entry.name = incoming.name;
        }
        if entry.base_url.is_none() {
            entry.base_url = incoming.base_url;
        }
        if entry.api_key.is_none() {
            entry.api_key = incoming.api_key;
        }
        for var in incoming.key_env {
            if !entry.key_env.contains(&var) {
                entry.key_env.push(var);
            }
        }
        for model in incoming.models {
            if entry.model(&model.id).is_none() {
                entry.models.push(model);
            }
        }
        for source in incoming.sources {
            if !entry.sources.contains(&source) {
                entry.sources.push(source);
            }
        }
    }

    /// Reads a key from `key_env` for each provider that has none yet.
    fn fill_keys_from_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        for provider in self.providers.values_mut() {
            if provider.api_key.is_none() {
                provider.api_key = provider.key_env.iter().find_map(|var| env(var));
            }
        }
    }

    /// `provider/model` for every model of every provider that is not the default endpoint.
    pub fn routable_models(&self) -> Vec<(String, &Provider, &ProviderModel)> {
        self.providers
            .values()
            .filter(|p| p.ready())
            .flat_map(|p| p.models.iter().map(move |m| (format!("{}/{}", p.id, m.id), p, m)))
            .collect()
    }

    /// The `providers` list of `--catalog`.
    pub fn catalog_entries(&self) -> Vec<Value> {
        self.providers
            .values()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": if p.name.is_empty() { p.id.clone() } else { p.name.clone() },
                    "baseUrl": p.base_url,
                    "default": p.is_default(),
                    "ready": p.ready() || p.is_default(),
                    "why": p.not_ready_reason(),
                    "keyEnv": p.key_env,
                    "sources": p.sources,
                    "models": p.models.iter().map(|m| format!("{}/{}", p.id, m.id)).collect::<Vec<_>>(),
                })
            })
            .collect()
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `{env:VAR}` and `{file:path}` in an OpenCode option value; `None` when what it names is not there.
pub fn expand_value(raw: &str, config_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else { break };
        let token = &rest[start + 1..start + len];
        let value = if let Some(var) = token.strip_prefix("env:") {
            env(var.trim())?
        } else if let Some(path) = token.strip_prefix("file:") {
            let path = path.trim();
            let path = match path.strip_prefix("~/") {
                Some(tail) => PathBuf::from(env("HOME")?).join(tail),
                None if Path::new(path).is_absolute() => PathBuf::from(path),
                None => config_dir.join(path),
            };
            std::fs::read_to_string(path).ok()?.trim().to_string()
        } else {
            out.push_str(&rest[..start + len + 1]);
            rest = &rest[start + len + 1..];
            continue;
        };
        out.push_str(&rest[..start]);
        out.push_str(&value);
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// The variables named by `{env:VAR}` in `raw`.
fn env_refs(raw: &str) -> Vec<String> {
    raw.match_indices("{env:")
        .filter_map(|(at, _)| raw[at + 5..].split_once('}').map(|(var, _)| var.trim().to_string()))
        .collect()
}

fn normalize_base(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// The `provider` section of one opencode.json(c) document.
pub fn parse_provider_section(root: &Value, config_dir: &Path, source: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<Provider> {
    let Some(section) = root.get("provider").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, entry) in section {
        let options = entry.get("options").cloned().unwrap_or(Value::Null);
        let text = |v: &Value, key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
        let base_url = text(&options, "baseURL")
            .or_else(|| text(&options, "baseUrl"))
            .or_else(|| text(entry, "api"))
            .and_then(|raw| expand_value(&raw, config_dir, env))
            .map(|url| normalize_base(&url));
        let raw_key = text(&options, "apiKey");
        let mut key_env: Vec<String> = raw_key.as_deref().map(env_refs).unwrap_or_default();
        if let Some(vars) = entry.get("env").and_then(|e| e.as_array()) {
            key_env.extend(vars.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
        let api_key = raw_key.and_then(|raw| expand_value(&raw, config_dir, env));
        let models = entry
            .get("models")
            .and_then(|m| m.as_object())
            .map(|models| {
                models
                    .iter()
                    .map(|(model_id, spec)| ProviderModel {
                        id: model_id.clone(),
                        upstream: text(spec, "id").unwrap_or_else(|| model_id.clone()),
                        name: text(spec, "name").unwrap_or_else(|| model_id.clone()),
                        context: spec["limit"]["context"].as_u64().filter(|n| *n > 0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Provider {
            id: id.clone(),
            name: text(entry, "name").unwrap_or_else(|| id.clone()),
            base_url,
            api_key,
            key_env,
            models,
            sources: vec![source.to_string()],
        });
    }
    out
}

/// OpenCode's stored API keys (`opencode auth login`): `{ "<provider>": { "type": "api", "key": "…" } }`.
pub fn parse_auth_json(root: &Value) -> Vec<Provider> {
    let Some(entries) = root.as_object() else { return Vec::new() };
    entries
        .iter()
        .filter(|(_, v)| v["type"] == "api")
        .filter_map(|(id, v)| {
            let key = v["key"].as_str().map(str::trim).filter(|k| !k.is_empty())?;
            Some(Provider { id: id.clone(), name: id.clone(), api_key: Some(key.to_string()), sources: vec!["auth.json".into()], ..Provider::default() })
        })
        .collect()
}

fn builtins() -> Vec<Provider> {
    BUILTIN
        .iter()
        .map(|(id, name, base, vars)| Provider {
            id: (*id).into(),
            name: (*name).into(),
            base_url: Some((*base).into()),
            key_env: vars.iter().map(|v| (*v).to_string()).collect(),
            sources: vec!["builtin".into()],
            ..Provider::default()
        })
        .collect()
}

/// Every opencode.json(c) to read providers from, most specific first, each once.
pub fn config_paths() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = env_var("OPENCODE_CONFIG") {
        candidates.push(PathBuf::from(p));
    }
    if let Some(sw) = env_var("STARWORLD") {
        let sw = PathBuf::from(sw);
        candidates.push(sw.join("CodeNomad/.opencode/opencode.jsonc"));
        candidates.push(sw.join("CodeNomad/.opencode/opencode.json"));
        candidates.push(sw.join("opencode.json"));
        candidates.push(sw.join("opencode.jsonc"));
        candidates.push(sw.join(".opencode/opencode.json"));
        candidates.push(sw.join(".opencode/opencode.jsonc"));
    }
    let config_home = env_var("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| env_var("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(dir) = config_home {
        candidates.push(dir.join("opencode/opencode.json"));
        candidates.push(dir.join("opencode/opencode.jsonc"));
        candidates.push(dir.join("opencode/config.json"));
    }
    let mut seen = Vec::new();
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| p.canonicalize().unwrap_or(p))
        .filter(|p| {
            let fresh = !seen.contains(p);
            seen.push(p.clone());
            fresh
        })
        .collect()
}

fn auth_path() -> Option<PathBuf> {
    let data_home = env_var("XDG_DATA_HOME").map(PathBuf::from).or_else(|| env_var("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(data_home.join("opencode/auth.json"))
}

/// The registry from the given config files, auth file and environment.
pub fn load_from(paths: &[PathBuf], auth: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Registry {
    let mut registry = Registry::default();
    for path in paths {
        let Ok(raw) = std::fs::read_to_string(path) else { continue };
        let Ok(root) = serde_json::from_str::<Value>(&strip_jsonc(&raw)) else {
            eprintln!("providers: cannot parse {}", path.display());
            continue;
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        for provider in parse_provider_section(&root, &dir, &path.display().to_string(), env) {
            registry.merge(provider);
        }
    }
    if let Some(root) = auth.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) {
        for provider in parse_auth_json(&root) {
            registry.merge(provider);
        }
    }
    for provider in builtins() {
        // A built-in is listed only when configured or when its key is set: twelve unconfigured providers are noise.
        let known = registry.providers.contains_key(&provider.id);
        let keyed = provider.key_env.iter().any(|v| env(v).is_some());
        if known || keyed {
            registry.merge(provider);
        }
    }
    registry.fill_keys_from_env(env);
    registry
}

static REGISTRY: RwLock<Option<Arc<Registry>>> = RwLock::new(None);

/// The process's registry, read once.
pub fn registry() -> Arc<Registry> {
    if let Some(found) = REGISTRY.read().ok().and_then(|g| g.clone()) {
        return found;
    }
    let loaded = Arc::new(load_from(&config_paths(), auth_path().as_deref(), &env_var));
    if std::env::var("SKILLS_DEBUG").is_ok_and(|v| v != "0") {
        for p in loaded.providers.values() {
            eprintln!("providers: {} ({}) {} model(s), {}", p.id, p.base_url.as_deref().unwrap_or("-"), p.models.len(), if p.ready() { "ready" } else { "not ready" });
        }
    }
    let mut guard = REGISTRY.write().expect("provider registry");
    guard.get_or_insert(loaded).clone()
}

/// Where `model` runs (see [`Registry::route`]).
pub fn route(model: &str) -> Option<Route> {
    registry().route(model)
}

/// Adds a provider to the process's registry (tests: a mock endpoint as a provider).
#[cfg(test)]
pub fn install_for_test(provider: Provider) {
    let current = registry();
    let mut next = (*current).clone();
    next.providers.insert(provider.id.clone(), provider);
    *REGISTRY.write().expect("provider registry") = Some(Arc::new(next));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    fn sample_config() -> Value {
        json!({
            "provider": {
                "openrouter-fallback": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "OpenRouter (fallback)",
                    "options": { "baseURL": "https://openrouter.ai/api/v1/", "apiKey": "{env:OPENROUTER_FALLBACK_KEY}" },
                    "models": { "auto": { "id": "openrouter/auto", "name": "Auto", "limit": { "context": 200000 } } }
                },
                "lmstudio": {
                    "options": { "baseURL": "http://localhost:1234/v1" },
                    "models": { "qwen3-coder-30b": {} }
                },
                "ollama": {
                    "options": { "baseURL": "http://127.0.0.1:11434/v1" },
                    "models": { "qwen3:8b": {} }
                },
                "opencode-go": { "models": { "mimo-v2.5": {} } }
            }
        })
    }

    #[test]
    fn the_provider_section_gives_base_url_key_and_models() {
        let env = env_of(&[("OPENROUTER_FALLBACK_KEY", "sk-or-1")]);
        let providers = parse_provider_section(&sample_config(), Path::new("/tmp"), "test", &env);
        let fallback = providers.iter().find(|p| p.id == "openrouter-fallback").unwrap();
        assert_eq!(fallback.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(fallback.api_key.as_deref(), Some("sk-or-1"));
        assert_eq!(fallback.key_env, vec!["OPENROUTER_FALLBACK_KEY"]);
        assert_eq!(fallback.models[0].upstream, "openrouter/auto", "the model's own `id` is what the API is sent");
        assert_eq!(fallback.models[0].context, Some(200_000));
        assert_eq!(fallback.name, "OpenRouter (fallback)");
    }

    #[test]
    fn a_missing_env_key_leaves_the_provider_not_ready_and_says_which_variable() {
        let env = env_of(&[]);
        let providers = parse_provider_section(&sample_config(), Path::new("/tmp"), "test", &env);
        let fallback = providers.iter().find(|p| p.id == "openrouter-fallback").unwrap();
        assert!(fallback.api_key.is_none() && !fallback.ready());
        assert!(fallback.not_ready_reason().unwrap().contains("OPENROUTER_FALLBACK_KEY"));
    }

    #[test]
    fn routes_go_to_each_providers_endpoint_and_ollama_stays_default() {
        let dir = std::env::temp_dir().join(format!("providers-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.json");
        std::fs::write(&path, sample_config().to_string()).unwrap();
        let env = env_of(&[("OPENROUTER_FALLBACK_KEY", "sk-or-1"), ("OPENAI_API_KEY", "sk-oa")]);
        let registry = load_from(&[path], None, &env);

        let route = registry.route("openrouter-fallback/auto").expect("routed");
        assert_eq!((route.base_url.as_str(), route.api_key.as_str(), route.model.as_str()), ("https://openrouter.ai/api/v1", "sk-or-1", "openrouter/auto"));

        let local = registry.route("lmstudio/qwen3-coder-30b").expect("a local endpoint needs no key");
        assert_eq!(local.base_url, "http://localhost:1234/v1");

        let openai = registry.route("openai/gpt-5").expect("a built-in with its key set, model not listed: sent as written");
        assert_eq!((openai.base_url.as_str(), openai.model.as_str()), ("https://api.openai.com/v1", "gpt-5"));

        assert!(registry.route("ollama/qwen3:8b").is_none(), "ollama is the default endpoint");
        assert!(registry.route("gemma4:26b").is_none(), "a bare name is the default endpoint");
        assert!(registry.route("opencode-go/mimo-v2.5").is_none(), "no base URL: not routed");
        assert!(registry.route("nobody/model").is_none());
        assert!(!registry.providers.contains_key("groq"), "a built-in that is neither configured nor keyed is left out");

        let ids: Vec<String> = registry.routable_models().into_iter().map(|(id, _, _)| id).collect();
        assert!(ids.contains(&"openrouter-fallback/auto".to_string()) && ids.contains(&"lmstudio/qwen3-coder-30b".to_string()), "{ids:?}");
        assert!(!ids.iter().any(|id| id.starts_with("ollama/")), "{ids:?}");
        let entries = registry.catalog_entries();
        let go = entries.iter().find(|e| e["id"] == "opencode-go").unwrap();
        assert_eq!(go["ready"], false);
        assert!(go["why"].as_str().unwrap().contains("base URL"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_earlier_config_wins_and_a_later_one_fills_the_gaps() {
        let env = env_of(&[]);
        let mut registry = Registry::default();
        let codenomad = json!({ "provider": { "openrouter": { "models": { "a": {} } } } });
        let starworld = json!({ "provider": { "openrouter": { "options": { "apiKey": "sk-sw", "baseURL": "https://proxy.example/v1" }, "models": { "a": { "id": "zzz" }, "b": {} } } } });
        for p in parse_provider_section(&codenomad, Path::new("/"), "codenomad", &env) {
            registry.merge(p);
        }
        for p in parse_provider_section(&starworld, Path::new("/"), "starworld", &env) {
            registry.merge(p);
        }
        for p in builtins() {
            registry.merge(p);
        }
        let or = &registry.providers["openrouter"];
        assert_eq!(or.base_url.as_deref(), Some("https://proxy.example/v1"), "the config's base URL beats the built-in");
        assert_eq!(or.api_key.as_deref(), Some("sk-sw"));
        assert_eq!(or.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(or.models[0].upstream, "a", "the first definition of a model wins");
        assert_eq!(or.sources, vec!["codenomad", "starworld", "builtin"]);
    }

    #[test]
    fn stored_opencode_keys_are_used() {
        let auth = json!({ "anthropic": { "type": "api", "key": "sk-ant" }, "github-copilot": { "type": "oauth", "refresh": "x" } });
        let mut registry = Registry::default();
        for p in parse_auth_json(&auth) {
            registry.merge(p);
        }
        for p in builtins() {
            if registry.providers.contains_key(&p.id) {
                registry.merge(p);
            }
        }
        assert!(!registry.providers.contains_key("github-copilot"), "oauth logins are not API keys");
        let route = registry.route("anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!((route.base_url.as_str(), route.api_key.as_str()), ("https://api.anthropic.com/v1", "sk-ant"));
    }

    #[test]
    fn option_values_expand_env_and_file_references() {
        let dir = std::env::temp_dir().join(format!("providers-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("key.txt"), "sk-file\n").unwrap();
        let env = env_of(&[("HOST", "api.example")]);
        assert_eq!(expand_value("{file:key.txt}", &dir, &env).as_deref(), Some("sk-file"));
        assert_eq!(expand_value("https://{env:HOST}/v1", &dir, &env).as_deref(), Some("https://api.example/v1"));
        assert_eq!(expand_value("{env:MISSING}", &dir, &env), None);
        assert_eq!(expand_value("plain-key", &dir, &env).as_deref(), Some("plain-key"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
