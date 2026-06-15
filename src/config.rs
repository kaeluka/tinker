use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

use crate::cap::Filesystem;

#[derive(Debug, Default, Deserialize)]
pub struct NativeModelConfig {
    pub high: Option<String>,
    pub mid: Option<String>,
    pub low: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelConfig {
    pub native: Option<NativeModelConfig>,
}

impl ModelConfig {
    pub fn native_high<'a>(&'a self, default: &'a str) -> &'a str {
        self.native.as_ref().and_then(|n| n.high.as_deref()).unwrap_or(default)
    }

    pub fn native_mid<'a>(&'a self, default: &'a str) -> &'a str {
        self.native.as_ref().and_then(|n| n.mid.as_deref()).unwrap_or(default)
    }

    pub fn native_low<'a>(&'a self, default: &'a str) -> &'a str {
        self.native.as_ref().and_then(|n| n.low.as_deref()).unwrap_or(default)
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
    native_defaults: [&str; 3],
) -> Result<()> {
    if fs.read_to_string(path).is_ok() {
        return Ok(());
    }
    let template = crate::prompts::config_starter_template(
        native_defaults[0],
        native_defaults[1],
        native_defaults[2],
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
        assert_eq!(cfg.native_high("opus"), "opus");
        assert_eq!(cfg.native_mid("sonnet"), "sonnet");
    }

    // spec (model-config): when TOML is invalid, load returns default rather
    // than panicking — a corrupted config file changes nothing.
    #[test]
    fn test_spec_load_model_config_returns_default_on_parse_error() {
        let fs = MockFs::new();
        fs.add_file(&config_path(), "not = valid [ toml garbage");
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.native_high("opus"), "opus");
    }

    // spec (model-config): slots present in the [native] config override the
    // built-in default; absent slots still fall back to the default.
    #[test]
    fn test_spec_load_model_config_overrides_present_slots_and_falls_back_for_absent() {
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[native]\nhigh = \"google/gemini-x\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.native_high("opus"), "google/gemini-x");
        // absent slot falls back to default
        assert_eq!(cfg.native_mid("sonnet"), "sonnet");
    }

    // spec (model-config): all three slots round-trip correctly when all are set.
    #[test]
    fn test_spec_load_model_config_parses_all_three_slots() {
        let fs = MockFs::new();
        fs.add_file(
            &config_path(),
            "[native]\n\
             high = \"nh\"\n\
             mid  = \"nm\"\n\
             low  = \"nl\"\n",
        );
        let cfg = load_model_config(&fs, &config_path());
        assert_eq!(cfg.native_high("x"), "nh");
        assert_eq!(cfg.native_mid("x"), "nm");
        assert_eq!(cfg.native_low("x"), "nl");
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
            ["nat-h", "nat-m", "nat-l"],
        )
        .unwrap();
        assert_eq!(fs.read_to_string(&config_path()).unwrap(), "existing content");
    }

    // spec (model-config): the starter template is written when no config file
    // exists, and it contains the [native] section header.
    #[test]
    fn test_spec_write_starter_template_written_when_absent() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["nat-h", "nat-m", "nat-l"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains("[native]"), "must contain [native] section");
        // No per-backend sections outside [native] — the config governs the
        // native backend only.
        assert!(!content.contains("[claude]"), "must not contain [claude] section");
        assert!(!content.contains("[opencode]"), "must not contain [opencode] section");
    }

    // spec (model-config): the starter template lists all three slot keys
    // (high, mid, low) under the [native] section.
    #[test]
    fn test_spec_write_starter_template_contains_all_three_slot_keys() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["a", "b", "c"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        // Each slot key must appear at least once (in the [native] section).
        assert!(content.contains("high"), "high must appear");
        assert!(content.contains("mid"), "mid must appear");
        assert!(content.contains("low"), "low must appear");
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
            ["google/gemini-native", "deepseek/flash-native", "google/lite-native"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains("google/gemini-native"), "native high default must appear");
        assert!(content.contains("deepseek/flash-native"), "native mid default must appear");
        assert!(content.contains("google/lite-native"), "native low default must appear");
    }

    // spec (model-config): the starter template annotates each slot with the
    // agents that consume that tier so the user knows what a rename affects.
    #[test]
    fn test_spec_write_starter_template_annotates_agents_per_tier() {
        let fs = MockFs::new();
        write_starter_template(
            &fs,
            &config_path(),
            ["nat-h", "nat-m", "nat-l"],
        )
        .unwrap();
        let content = fs.read_to_string(&config_path()).unwrap();
        assert!(content.contains("tend") && content.contains("rummage") && content.contains("jog"),
            "high tier annotation must name tend, rummage, and jog");
        assert!(content.contains("goal sessions"),
            "mid tier annotation must name goal sessions");
        assert!(content.contains("cleanup"),
            "low tier annotation must name cleanup");
    }
}
