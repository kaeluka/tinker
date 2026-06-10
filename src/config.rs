use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

use crate::cap::Filesystem;

#[derive(Debug, Default, Deserialize)]
pub struct BackendModelConfig {
    pub high: Option<String>,
    pub mid: Option<String>,
    pub low: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelConfig {
    pub claude: Option<BackendModelConfig>,
    pub opencode: Option<BackendModelConfig>,
}

impl ModelConfig {
    pub fn claude_high<'a>(&'a self, default: &'a str) -> &'a str {
        self.claude.as_ref().and_then(|c| c.high.as_deref()).unwrap_or(default)
    }

    pub fn claude_mid<'a>(&'a self, default: &'a str) -> &'a str {
        self.claude.as_ref().and_then(|c| c.mid.as_deref()).unwrap_or(default)
    }

    pub fn claude_low<'a>(&'a self, default: &'a str) -> &'a str {
        self.claude.as_ref().and_then(|c| c.low.as_deref()).unwrap_or(default)
    }

    pub fn opencode_high<'a>(&'a self, default: &'a str) -> &'a str {
        self.opencode.as_ref().and_then(|c| c.high.as_deref()).unwrap_or(default)
    }

    pub fn opencode_mid<'a>(&'a self, default: &'a str) -> &'a str {
        self.opencode.as_ref().and_then(|c| c.mid.as_deref()).unwrap_or(default)
    }

    pub fn opencode_low<'a>(&'a self, default: &'a str) -> &'a str {
        self.opencode.as_ref().and_then(|c| c.low.as_deref()).unwrap_or(default)
    }
}

// Returns ModelConfig::default() silently when the file is absent or unparseable.
pub fn load_model_config(fs: &dyn Filesystem, path: &Path) -> ModelConfig {
    let Ok(text) = fs.read_to_string(path) else {
        return ModelConfig::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

// Writes a commented-out starter config; skips silently if the file already exists.
pub fn write_starter_template(
    fs: &dyn Filesystem,
    path: &Path,
    claude_defaults: [&str; 3],
    opencode_defaults: [&str; 3],
) -> Result<()> {
    if fs.read_to_string(path).is_ok() {
        return Ok(());
    }
    let template = crate::prompts::config_starter_template(
        claude_defaults[0],
        claude_defaults[1],
        claude_defaults[2],
        opencode_defaults[0],
        opencode_defaults[1],
        opencode_defaults[2],
    );
    fs.write(path, &template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockFs;
    use std::path::PathBuf;

    fn config_path() -> PathBuf {
        PathBuf::from("/project/.tinker/config.toml")
    }

    // spec (model-config): when no config file exists, load returns a default
    // ModelConfig so all accessor calls fall back to the built-in default.
    #[test]
    fn test_spec_load_model_config_returns_default_when_file_absent() {
        let fs = MockFs::new();
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.claude_high("opus"), "opus");
        assert_eq!(cfg.opencode_mid("mid-model"), "mid-model");
    }

    // spec (model-config): when TOML is invalid, load returns default rather
    // than panicking — a corrupted config file changes nothing.
    #[test]
    fn test_spec_load_model_config_returns_default_on_parse_error() {
        let fs = MockFs::new();
        fs.add_file(&config_path(), "not = valid [ toml garbage");
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.claude_high("opus"), "opus");
    }

    // spec (model-config): slots present in the config override the built-in
    // default; absent slots still fall back to the default.
    #[test]
    fn test_spec_load_model_config_overrides_present_slots_and_falls_back_for_absent() {
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[claude]\nhigh = \"claude-opus-4-7\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.claude_high("opus"), "claude-opus-4-7");
        // absent slot falls back to default
        assert_eq!(cfg.claude_mid("sonnet"), "sonnet");
        // entirely absent backend section falls back to default
        assert_eq!(cfg.opencode_high("openrouter/foo"), "openrouter/foo");
    }

    // spec (model-config): all six slots round-trip correctly when all are set.
    #[test]
    fn test_spec_load_model_config_parses_all_six_slots() {
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[claude]\n\
             high = \"ch\"\n\
             mid  = \"cm\"\n\
             low  = \"cl\"\n\
             \n\
             [opencode]\n\
             high = \"oh\"\n\
             mid  = \"om\"\n\
             low  = \"ol\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.claude_high("x"), "ch");
        assert_eq!(cfg.claude_mid("x"), "cm");
        assert_eq!(cfg.claude_low("x"), "cl");
        assert_eq!(cfg.opencode_high("x"), "oh");
        assert_eq!(cfg.opencode_mid("x"), "om");
        assert_eq!(cfg.opencode_low("x"), "ol");
    }

    // spec (model-config): write_starter_template does not overwrite an
    // existing config file — whether hand-edited or previously generated.
    #[test]
    fn test_spec_write_starter_template_skips_when_file_exists() {
        let fs = MockFs::new();
        fs.add_file(&config_path(), "existing content");
        write_starter_template(
            &fs,
            &config_path(),
            ["opus", "sonnet", "haiku"],
            ["oc-h", "oc-m", "oc-l"],
        )
        .unwrap();
        assert_eq!(fs.read_to_string(&config_path()).unwrap(), "existing content");
    }

    // spec (model-config): the starter template is written when no config file
    // exists, and it contains both [claude] and [opencode] section headers.
    #[test]
    fn test_spec_write_starter_template_written_when_absent() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["opus", "sonnet", "haiku"],
            ["openrouter/h", "openrouter/m", "openrouter/l"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains("[claude]"), "must contain [claude] section");
        assert!(content.contains("[opencode]"), "must contain [opencode] section");
    }

    // spec (model-config): the starter template lists all six slot keys.
    #[test]
    fn test_spec_write_starter_template_contains_all_six_slot_keys() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["a", "b", "c"],
            ["x", "y", "z"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // Each slot key must appear at least twice (once per backend section)
        let occurrences = |key: &str| content.matches(key).count();
        assert!(occurrences("high") >= 2, "high must appear for both backends");
        assert!(occurrences("mid") >= 2, "mid must appear for both backends");
        assert!(occurrences("low") >= 2, "low must appear for both backends");
    }

    // spec (model-config): every slot line in the starter template is commented
    // out so the file changes nothing on first use.
    #[test]
    fn test_spec_write_starter_template_all_slot_lines_commented() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["opus", "sonnet", "haiku"],
            ["oc-h", "oc-m", "oc-l"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // No uncommented assignment lines (lines with `=` that don't start with `#`)
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

    // spec (model-config): the built-in default values passed to
    // write_starter_template appear in the generated template so the user can
    // see what each slot defaults to without reading source code.
    #[test]
    fn test_spec_write_starter_template_shows_built_in_defaults() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["opus", "sonnet", "haiku"],
            ["openrouter/google/gemini", "openrouter/deepseek/flash", "openrouter/google/lite"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains("opus"), "claude high default must appear");
        assert!(content.contains("sonnet"), "claude mid default must appear");
        assert!(content.contains("haiku"), "claude low default must appear");
        assert!(content.contains("openrouter/google/gemini"), "opencode high default must appear");
        assert!(content.contains("openrouter/deepseek/flash"), "opencode mid default must appear");
        assert!(content.contains("openrouter/google/lite"), "opencode low default must appear");
    }

    // spec (model-config): the starter template annotates each slot with the
    // agents that consume that tier so the user knows what a rename affects.
    #[test]
    fn test_spec_write_starter_template_annotates_agents_per_tier() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["opus", "sonnet", "haiku"],
            ["oc-h", "oc-m", "oc-l"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains("tinker") && content.contains("rummage") && content.contains("jog"),
            "high tier annotation must name tinker, rummage, and jog");
        assert!(content.contains("goal sessions"),
            "mid tier annotation must name goal sessions");
        assert!(content.contains("cleanup"),
            "low tier annotation must name cleanup");
    }
}
