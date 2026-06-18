use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

use crate::cap::Filesystem;

// ── Defaults & env-var convention ───────────────────────────────────────
//
// OpenRouter defaults the loader falls back to when a tier is absent from
// the user's config. These are private to the loader — callers go through
// `native_high/mid/low()` which fills them in. They are also used to
// populate the starter template the user sees on first start.
//
// Auth env-var convention: per-tier, no provider name. The convention is
// generic on purpose — it works for any OpenAI-protocol endpoint, not just
// OpenRouter. Empty or unset means "no Authorization header" (what local
// model servers expect).
pub const HIGH_API_KEY_ENV: &str = "TINKER_HIGH_API_KEY";
pub const MID_API_KEY_ENV: &str = "TINKER_MID_API_KEY";
pub const LOW_API_KEY_ENV: &str = "TINKER_LOW_API_KEY";

/// OpenRouter endpoint and models used as the starter-template defaults.
/// `backends` keeps them here as the single source of truth; the per-tier
/// accessors fill them in when the user config is absent.
pub const DEFAULT_HIGH_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const DEFAULT_HIGH_MODEL: &str = "google/gemini-3.1-pro-preview";
pub const DEFAULT_MID_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const DEFAULT_MID_MODEL: &str = "deepseek/deepseek-v4-flash";
pub const DEFAULT_LOW_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const DEFAULT_LOW_MODEL: &str = "google/gemini-3.1-flash-lite-preview";

/// Per-tier block: an OpenAI-protocol endpoint URL and a model identifier.
/// Either may be absent independently; absent fields fall back to the
/// loader's OpenRouter defaults. Auth is sourced from per-tier
/// environment variables by `backends` — never stored in this file.
#[derive(Debug, Default, Deserialize)]
pub struct NativeTierConfig {
    pub endpoint: Option<String>,
    pub model: Option<String>,
}

/// Three per-tier blocks, one for each model tier. All three are optional;
/// an absent tier block falls back to the OpenRouter defaults above.
#[derive(Debug, Default, Deserialize)]
pub struct NativeModelConfig {
    pub high: Option<NativeTierConfig>,
    pub mid: Option<NativeTierConfig>,
    pub low: Option<NativeTierConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelConfig {
    pub native: Option<NativeModelConfig>,
}

/// Owned view of a resolved tier: each field is either the value from
/// the user's config or the loader's fallback. `TierConfig` carries only
/// the TOML-derived fields — auth is *not* part of the config (secrets
/// stay in env vars by spec), so it's resolved through a separate
/// accessor on `ModelConfig` rather than mixed into this struct.
#[derive(Debug, Clone)]
pub struct TierConfig {
    pub endpoint: String,
    pub model: String,
}

impl ModelConfig {
    pub fn native_high(&self) -> TierConfig {
        resolve_tier(
            self.native.as_ref().and_then(|n| n.high.as_ref()),
            DEFAULT_HIGH_ENDPOINT,
            DEFAULT_HIGH_MODEL,
        )
    }

    pub fn native_mid(&self) -> TierConfig {
        resolve_tier(
            self.native.as_ref().and_then(|n| n.mid.as_ref()),
            DEFAULT_MID_ENDPOINT,
            DEFAULT_MID_MODEL,
        )
    }

    pub fn native_low(&self) -> TierConfig {
        resolve_tier(
            self.native.as_ref().and_then(|n| n.low.as_ref()),
            DEFAULT_LOW_ENDPOINT,
            DEFAULT_LOW_MODEL,
        )
    }

    /// Per-tier auth resolution from the env-var convention
    /// (`TINKER_HIGH_API_KEY` / `TINKER_MID_API_KEY` / `TINKER_LOW_API_KEY`).
    /// Empty or unset → None → no Authorization header (the local-server
    /// path). Auth never comes from the TOML — the model-config goal
    /// says explicitly "Auth is not stored in this file".
    pub fn native_high_api_key(&self) -> Option<String> {
        read_api_key_env(HIGH_API_KEY_ENV)
    }

    pub fn native_mid_api_key(&self) -> Option<String> {
        read_api_key_env(MID_API_KEY_ENV)
    }

    pub fn native_low_api_key(&self) -> Option<String> {
        read_api_key_env(LOW_API_KEY_ENV)
    }
}

/// Pulls (endpoint, model) out of an optional tier block, falling
/// back to the loader's defaults when the block or any of its fields
/// are absent. Auth is sourced separately via the per-tier env var
/// accessors — never from this struct, by spec.
fn resolve_tier(
    tier: Option<&NativeTierConfig>,
    default_endpoint: &str,
    default_model: &str,
) -> TierConfig {
    let (endpoint, model) = match tier {
        Some(t) => (
            t.endpoint.clone().unwrap_or_else(|| default_endpoint.to_string()),
            t.model.clone().unwrap_or_else(|| default_model.to_string()),
        ),
        None => (default_endpoint.to_string(), default_model.to_string()),
    };
    TierConfig { endpoint, model }
}

/// Read a per-tier auth env var. Empty/unset → None. The TOML never
/// carries auth; this is the single source of truth for the resolution.
fn read_api_key_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|k| !k.is_empty())
}

// Returns ModelConfig::default() silently when the file is absent or unparseable.
pub fn load_model_config(fs: &dyn Filesystem, path: &Path) -> ModelConfig {
    let Ok(text) = fs.read_to_string(path) else {
        return ModelConfig::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

// Writes a commented-out starter config; skips silently if the file already exists.
//
// The starter template uses the loader's OpenRouter defaults for all
// three tiers, so the file is a no-op on first use but documents what
// each slot defaults to and where the auth env-var convention lives.
pub fn write_starter_template(fs: &dyn Filesystem, path: &Path) -> Result<()> {
    if fs.read_to_string(path).is_ok() {
        return Ok(());
    }
    let template = crate::prompts::config_starter_template(
        DEFAULT_HIGH_ENDPOINT,
        DEFAULT_HIGH_MODEL,
        DEFAULT_MID_ENDPOINT,
        DEFAULT_MID_MODEL,
        DEFAULT_LOW_ENDPOINT,
        DEFAULT_LOW_MODEL,
    );
    fs.write(path, &template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockFs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn config_path() -> PathBuf {
        PathBuf::from("/project/.tinker/config.toml")
    }

    // Process-wide mutex serializing env-var reads/writes in this test
    // module. `std::env::set_var` and `std::env::var` are not thread-safe
    // in Rust 2024 — concurrent mutations and reads across threads
    // produce undefined behavior — so every env-mutating test in this
    // module must acquire this lock before touching the env. The lock
    // is held only for the duration of the test body, never across
    // awaits or long-running work. Poisoning on panic is fine: a
    // panicking test has already failed and any peer that finds the
    // lock poisoned just sees a `None`/current state and continues.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // Helper: clear the per-tier auth env vars so tests don't leak state
    // between each other or read whatever the developer's shell happened
    // to set. Tests that want to exercise auth resolution explicitly
    // (re-)set them after calling this.
    fn clear_auth_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::remove_var(HIGH_API_KEY_ENV);
            std::env::remove_var(MID_API_KEY_ENV);
            std::env::remove_var(LOW_API_KEY_ENV);
        }
    }

    // spec (model-config): when no config file exists, load returns a default
    // ModelConfig so all accessor calls fall back to the loader's OpenRouter
    // defaults.
    //
    // Holds ENV_LOCK for the whole test body so a peer test cannot
    // set TINKER_*_API_KEY between the unset and the loader read.
    #[test]
    fn test_spec_load_model_config_returns_default_when_file_absent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::remove_var(HIGH_API_KEY_ENV);
            std::env::remove_var(MID_API_KEY_ENV);
            std::env::remove_var(LOW_API_KEY_ENV);
        }
        let fs = MockFs::new();
        let cfg = load_model_config(&fs, &config_path());
        let high = cfg.native_high();
        assert_eq!(high.endpoint, DEFAULT_HIGH_ENDPOINT);
        assert_eq!(high.model, DEFAULT_HIGH_MODEL);
        let mid = cfg.native_mid();
        assert_eq!(mid.endpoint, DEFAULT_MID_ENDPOINT);
        assert_eq!(mid.model, DEFAULT_MID_MODEL);
        let low = cfg.native_low();
        assert_eq!(low.endpoint, DEFAULT_LOW_ENDPOINT);
        assert_eq!(low.model, DEFAULT_LOW_MODEL);
        // Auth is unresolved (no env var set) — `TierConfig` no longer
        // carries auth; the dedicated accessors return None when the
        // env vars are unset.
        assert!(
            cfg.native_high_api_key().is_none(),
            "high auth must be None with no env var set"
        );
        assert!(
            cfg.native_mid_api_key().is_none(),
            "mid auth must be None with no env var set"
        );
        assert!(
            cfg.native_low_api_key().is_none(),
            "low auth must be None with no env var set"
        );
    }

    // spec (model-config): when TOML is invalid, load returns default rather
    // than panicking — a corrupted config file changes nothing.
    #[test]
    fn test_spec_load_model_config_returns_default_on_parse_error() {
        clear_auth_env();
        let fs = MockFs::new();
        fs.add_file(&config_path(), "not = valid [ toml garbage");
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.native_high().model, DEFAULT_HIGH_MODEL);
    }

    // spec (model-config): a present tier block overrides the built-in
    // defaults; an absent tier block still falls back to the defaults.
    #[test]
    fn test_spec_load_model_config_overrides_present_tier_and_falls_back_for_absent() {
        clear_auth_env();
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[native.high]\n\
             endpoint = \"https://example.com/v1/chat\"\n\
             model = \"google/gemini-x\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        let high = cfg.native_high();
        assert_eq!(high.endpoint, "https://example.com/v1/chat");
        assert_eq!(high.model, "google/gemini-x");
        // absent tier falls back to loader defaults
        let mid = cfg.native_mid();
        assert_eq!(mid.endpoint, DEFAULT_MID_ENDPOINT);
        assert_eq!(mid.model, DEFAULT_MID_MODEL);
    }

    // spec (model-config): all three tier blocks round-trip correctly when
    // each carries its own (endpoint, model, key).
    #[test]
    fn test_spec_load_model_config_parses_all_three_tier_blocks() {
        clear_auth_env();
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[native.high]\n\
             endpoint = \"https://h.example/v1\"\n\
             model = \"mh\"\n\
             [native.mid]\n\
             endpoint = \"https://m.example/v1\"\n\
             model = \"mm\"\n\
             [native.low]\n\
             endpoint = \"https://l.example/v1\"\n\
             model = \"ml\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        let high = cfg.native_high();
        assert_eq!(high.endpoint, "https://h.example/v1");
        assert_eq!(high.model, "mh");
        // Auth is env-var-only — the TOML never carries `key`, and
        // `TierConfig` doesn't either. The dedicated accessor returns
        // None when no env var is set (the default for this test).
        assert!(
            cfg.native_high_api_key().is_none(),
            "auth must be env-var-only — TOML never carries key",
        );
        let mid = cfg.native_mid();
        assert_eq!(mid.endpoint, "https://m.example/v1");
        assert_eq!(mid.model, "mm");
        assert!(cfg.native_mid_api_key().is_none());
        let low = cfg.native_low();
        assert_eq!(low.endpoint, "https://l.example/v1");
        assert_eq!(low.model, "ml");
        assert!(cfg.native_low_api_key().is_none());
    }

    // spec (model-config): endpoint and model fall back independently inside
    // a present tier block — a block with only `endpoint` set still uses
    // the loader's model default.
    #[test]
    fn test_spec_load_model_config_partial_tier_block_uses_independent_fallbacks() {
        clear_auth_env();
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[native.high]\n\
             endpoint = \"https://only-ep.example/v1\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        let high = cfg.native_high();
        assert_eq!(high.endpoint, "https://only-ep.example/v1");
        assert_eq!(high.model, DEFAULT_HIGH_MODEL);
    }

    // spec (model-config): a present tier block with only `model` set still
    // uses the loader's endpoint default.
    #[test]
    fn test_spec_load_model_config_partial_tier_block_uses_default_endpoint() {
        clear_auth_env();
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[native.high]\n\
             model = \"only-model\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        let high = cfg.native_high();
        assert_eq!(high.endpoint, DEFAULT_HIGH_ENDPOINT);
        assert_eq!(high.model, "only-model");
    }

    // spec (backends): an empty (but set) env var is treated the same as
    // an unset var — auth resolves to None, no Authorization header is
    // sent. This is the local-model-server path: the user sets the var
    // to "" to be explicit about wanting no auth, or simply leaves it
    // unset. Both forms must yield identical runtime behavior.
    #[test]
    fn test_spec_load_model_config_empty_env_var_treated_as_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var(HIGH_API_KEY_ENV, ""); }
        let fs = MockFs::new();
        fs.add_file(&config_path(), "");
        let cfg = load_model_config(&fs, &config_path());
        assert!(
            cfg.native_high_api_key().is_none(),
            "empty env var must resolve to None (same as unset)",
        );
        unsafe { std::env::remove_var(HIGH_API_KEY_ENV); }
    }

    // spec (backends): each tier resolves its auth independently from
    // its own env var — high/mid/low don't share the auth slot. A user
    // can mix tiers across providers, each with its own key (or no key
    // for a local server).
    #[test]
    fn test_spec_load_model_config_per_tier_auth_is_independent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var(HIGH_API_KEY_ENV, "high-key"); }
        unsafe { std::env::set_var(LOW_API_KEY_ENV, "low-key"); }
        // mid env var is unset.
        let fs = MockFs::new();
        fs.add_file(&config_path(), "");
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.native_high_api_key().as_deref(), Some("high-key"));
        assert!(
            cfg.native_mid_api_key().is_none(),
            "mid tier env var unset must yield None",
        );
        assert_eq!(cfg.native_low_api_key().as_deref(), Some("low-key"));
        unsafe { std::env::remove_var(HIGH_API_KEY_ENV); }
        unsafe { std::env::remove_var(LOW_API_KEY_ENV); }
    }

    // spec (backends): the auth env-var convention names are exported
    // as public constants so the composition root (main.rs) can use them
    // for its startup-precondition check, and so external tests can
    // assert against the exact strings the loader reads.
    #[test]
    fn test_spec_auth_env_var_constants_have_generic_names() {
        // No provider name in any of the env-var constants — the
        // convention is generic so any OpenAI-protocol endpoint can use it.
        for name in [HIGH_API_KEY_ENV, MID_API_KEY_ENV, LOW_API_KEY_ENV] {
            assert!(!name.contains("OPENROUTER"), "{name} must not contain OPENROUTER");
            assert!(!name.contains("ANTHROPIC"), "{name} must not contain ANTHROPIC");
            assert!(!name.contains("OPENAI"), "{name} must not contain OPENAI");
            // Tinker-namespaced, per-tier: TINKER_<TIER>_API_KEY.
            assert!(name.starts_with("TINKER_"), "{name} must be namespaced under TINKER_");
            assert!(name.ends_with("_API_KEY"), "{name} must end with _API_KEY");
        }
        // All three distinct.
        assert_ne!(HIGH_API_KEY_ENV, MID_API_KEY_ENV);
        assert_ne!(MID_API_KEY_ENV, LOW_API_KEY_ENV);
        assert_ne!(HIGH_API_KEY_ENV, LOW_API_KEY_ENV);
    }

    // spec (model-config): write_starter_template does not overwrite an
    // existing config file — whether hand-edited or previously generated.
    #[test]
    fn test_spec_write_starter_template_skips_when_file_exists() {
        clear_auth_env();
        let fs = MockFs::new();
        fs.add_file(&config_path(), "existing content");
        write_starter_template(&fs, &config_path()).unwrap();
        assert_eq!(fs.read_to_string(&config_path()).unwrap(), "existing content");
    }

    // spec (model-config): the starter template is written when no config
    // file exists, and it contains the three per-tier section headers.
    #[test]
    fn test_spec_write_starter_template_written_when_absent() {
        clear_auth_env();
        let fs = MockFs::new();
        write_starter_template(&fs, &config_path()).unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // Per-tier sub-table headers — each tier is its own block.
        assert!(content.contains("[native.high]"), "must contain [native.high] section");
        assert!(content.contains("[native.mid]"), "must contain [native.mid] section");
        assert!(content.contains("[native.low]"), "must contain [native.low] section");
        // No per-backend sections outside [native.*] — the config governs
        // the native backend only.
        assert!(!content.contains("[claude]"), "must not contain [claude] section");
        assert!(!content.contains("[opencode]"), "must not contain [opencode] section");
    }

    // spec (model-config): every assignment line in the starter template is
    // commented out so the file changes nothing on first use.
    #[test]
    fn test_spec_write_starter_template_all_slot_lines_commented() {
        clear_auth_env();
        let fs = MockFs::new();
        write_starter_template(&fs, &config_path()).unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // No uncommented assignment lines (lines with `=` that don't start with `[`).
        // Section headers (e.g. `[native.high]`) start with `[` and are skipped.
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.contains('=') && !trimmed.starts_with('[') {
                assert!(
                    trimmed.starts_with('#'),
                    "slot line must be commented out: {:?}",
                    line
                );
            }
        }
    }

    // spec (model-config): the loader's OpenRouter defaults appear in the
    // generated starter template so the user can see what each tier
    // defaults to without reading source code.
    #[test]
    fn test_spec_write_starter_template_shows_built_in_defaults() {
        clear_auth_env();
        let fs = MockFs::new();
        write_starter_template(&fs, &config_path()).unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains(DEFAULT_HIGH_ENDPOINT), "default high endpoint must appear");
        assert!(content.contains(DEFAULT_MID_ENDPOINT), "default mid endpoint must appear");
        assert!(content.contains(DEFAULT_LOW_ENDPOINT), "default low endpoint must appear");
        assert!(content.contains(DEFAULT_HIGH_MODEL), "native high default model must appear");
        assert!(content.contains(DEFAULT_MID_MODEL), "native mid default model must appear");
        assert!(content.contains(DEFAULT_LOW_MODEL), "native low default model must appear");
    }

    // spec (model-config): the starter template annotates each tier with
    // the agents that consume that tier so the user knows what a rename
    // affects.
    #[test]
    fn test_spec_write_starter_template_annotates_agents_per_tier() {
        clear_auth_env();
        let fs = MockFs::new();
        write_starter_template(&fs, &config_path()).unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(
            content.contains("tend") && content.contains("rummage") && content.contains("jog"),
            "high tier annotation must name tend, rummage, and jog"
        );
        assert!(
            content.contains("goal sessions"),
            "mid tier annotation must name goal sessions"
        );
        assert!(content.contains("cleanup"), "low tier annotation must name cleanup");
    }

    // spec (model-config): the starter template includes both an `endpoint`
    // and a `model` field per tier — the two configurable fields the
    // schema exposes. Auth is not in the template because it's sourced
    // from per-tier environment variables, never stored in the file.
    #[test]
    fn test_spec_write_starter_template_contains_endpoint_and_model_fields() {
        clear_auth_env();
        let fs = MockFs::new();
        write_starter_template(&fs, &config_path()).unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // Restrict to commented assignment lines (the starter template is
        // a no-op until the user uncomments). The key is the LHS of the
        // first `=` on each line.
        let count_field = |needle: &str| -> usize {
            content
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    if !(t.starts_with('#') && t.contains('=')) {
                        return false;
                    }
                    t.split_once('=')
                        .map(|(lhs, _)| lhs.split_whitespace().any(|w| w == needle))
                        .unwrap_or(false)
                })
                .count()
        };
        assert_eq!(count_field("endpoint"), 3, "all three tiers must have an endpoint field");
        assert_eq!(count_field("model"), 3, "all three tiers must have a model field");
        // No `key` field — auth is env-var-only by spec, the TOML never
        // carries secrets.
        assert_eq!(
            count_field("key"),
            0,
            "the starter template must NOT include a `key` field — auth is env-var-only",
        );
    }

    // spec (backends): the starter template documents the per-tier auth
    // env-var convention so the user knows where the keys come from
    // without reading source code. The env-var constants are stable —
    // this test pins the template against them.
    #[test]
    fn test_spec_write_starter_template_documents_auth_env_vars() {
        clear_auth_env();
        let fs = MockFs::new();
        write_starter_template(&fs, &config_path()).unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // The template must name all three env vars so the user can see
        // the convention at a glance.
        assert!(
            content.contains(HIGH_API_KEY_ENV),
            "starter template must name {HIGH_API_KEY_ENV}"
        );
        assert!(
            content.contains(MID_API_KEY_ENV),
            "starter template must name {MID_API_KEY_ENV}"
        );
        assert!(
            content.contains(LOW_API_KEY_ENV),
            "starter template must name {LOW_API_KEY_ENV}"
        );
        // And it must say the key is optional / falls back to the env
        // var, so the user knows the typical config path.
        assert!(
            content.contains("env") || content.contains("environment"),
            "starter template must describe the env-var fallback for the optional key field"
        );
    }
}