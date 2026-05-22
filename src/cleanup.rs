//! Cleanup of tinker-test-case markers before each goal session.
//!
//! See `## Tinkering` in the orchestrator prompt and section 5 of
//! `packaged-goals/coding-standards.toml` for the marker convention.
//!
//! The orchestrator's tinkering accumulates across its turns and is wiped
//! between goal-session runs. This module is what does the wiping: walks the
//! project tree, finds files containing comment-form markers, dispatches a
//! cheap opencode agent to remove them, retries up to N times, and reports
//! the outcome.
//!
//! The matcher is line-anchored on a comment delimiter. That's what keeps
//! this file's own source (the const definition, the prompt template, the
//! test fixtures) from triggering matches — string literals containing the
//! marker text aren't at the start of comment lines.

use crate::cap::{Filesystem, OpenCodeRunner};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The marker tag without comment-delimiter prefix. The trailing colon is
/// deliberate: it's the load-bearing grep character.
pub const MARKER: &str = "tinker-test-case:";
pub const MAX_RETRIES: usize = 3;

/// Comment delimiters this scanner recognizes when line-anchoring the match.
/// `//` covers C-family languages; `#` covers Python/Ruby/shell; `--` covers
/// SQL/Haskell/Lua; `;` covers Lisps; `*` covers block-comment continuation
/// lines (`/* ... \n * ... \n */`). `/*` covers block-comment opens.
const COMMENT_PREFIXES: &[&str] = &["//", "/*", "#", "--", ";", "*"];

/// True when `content` has any line that is a comment-form marker —
/// optional whitespace, a comment delimiter, optional whitespace, then the
/// literal `MARKER`. Line-anchoring means string literals or prose mentions
/// of the marker (this file's own const definition, the orchestrator
/// prompt's explanatory text, etc.) don't match.
pub fn file_contains_marker(content: &str) -> bool {
    content.lines().any(line_is_marker)
}

fn line_is_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    for prefix in COMMENT_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest.trim_start().starts_with(MARKER) {
                return true;
            }
        }
    }
    false
}

/// Directory names skipped when walking the project. Either the build
/// artifact (`target/`, `node_modules/`), tinker's own state (`.tinker/`),
/// or version control (`.git/`).
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".tinker",
    "dist",
    "build",
    ".next",
    "__pycache__",
    ".venv",
    "venv",
];

/// File extensions skipped when scanning for markers. Documentation and
/// metadata files can contain the literal string (e.g. design notes, this
/// file) without it being a marker.
const SKIP_EXTS: &[&str] = &[
    "md", "txt", "toml", "json", "lock", "log", "svg", "png", "jpg", "jpeg",
    "gif", "ico", "yaml", "yml",
];

pub enum CleanupOutcome {
    Clean,
    FailedAfterRetries(Vec<PathBuf>),
}

/// Walks the project tree and returns every source file containing a
/// comment-form marker (see `file_contains_marker` for the exact pattern).
pub fn find_marker_files(fs: &dyn Filesystem, work_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = vec![];
    walk(fs, work_dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(fs: &dyn Filesystem, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if should_skip_dir(dir) {
        return Ok(());
    }
    let entries = match fs.list_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        if fs.is_dir(&entry) {
            walk(fs, &entry, out)?;
        } else if should_scan_file(&entry) {
            if let Ok(content) = fs.read_to_string(&entry) {
                if file_contains_marker(&content) {
                    out.push(entry);
                }
            }
        }
    }
    Ok(())
}

fn should_skip_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|s| s.to_str())
        .map(|n| SKIP_DIRS.contains(&n))
        .unwrap_or(false)
}

fn should_scan_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    !SKIP_EXTS.contains(&ext)
}

/// Greps for markers; if any are found, dispatches a cleanup agent, re-greps,
/// retries up to `MAX_RETRIES` times. Returns `Clean` on success,
/// `FailedAfterRetries(files)` if dirt remains after retries.
pub async fn run_cleanup(
    oc: &dyn OpenCodeRunner,
    fs: &dyn Filesystem,
    work_dir: &Path,
) -> Result<CleanupOutcome> {
    let mut remaining = find_marker_files(fs, work_dir)?;
    if remaining.is_empty() {
        return Ok(CleanupOutcome::Clean);
    }
    for _ in 0..MAX_RETRIES {
        let prompt = build_cleanup_prompt(&remaining);
        run_oc_oneshot(oc, &prompt, work_dir).await?;
        remaining = find_marker_files(fs, work_dir)?;
        if remaining.is_empty() {
            return Ok(CleanupOutcome::Clean);
        }
    }
    Ok(CleanupOutcome::FailedAfterRetries(remaining))
}

pub fn build_cleanup_prompt(files: &[PathBuf]) -> String {
    let listing = files
        .iter()
        .map(|p| format!("- {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"You are a cleanup agent for tinker. Your only job is to remove or revert investigation markers left by the orchestrator's tinkering. Don't touch anything else in the codebase.

## What counts as a real marker

A real marker is a comment line whose content begins with `tinker-test-case:` — that is, optional whitespace, a comment delimiter (`//`, `#`, `--`, `;`, `*`, or `/*`), optional whitespace, then the literal `tinker-test-case:` followed by the reason. Examples:

- `// tinker-test-case: probing the lexer on f(1)(`
- `# tinker-test-case: temporary debug print`
- `    /* tinker-test-case: hacked the timeout */`

## What is NOT a marker — leave these alone

These references mention the marker name but aren't markers themselves:

- A string literal containing the marker text (e.g. `const MARKER: &str = "tinker-test-case:";`).
- A prose mention inside another comment, like `// the orchestrator emits tinker-test-case: lines`.
- Mentions in this project's own source describing the convention — typically in `src/cleanup.rs` (where the matcher and this very prompt live) and `src/orchestrator.rs` (where the convention is documented for the orchestrator). Leave these intact.

## Files with real markers

{listing}

## What to do with each marker

For each real marker, identify its shape and clean accordingly:

- **Inline addition** (the marker labels a temporary region the orchestrator added): remove the marked region — the test function, the instrumentation line, the adapter function, etc.
- **In-place modification** (the marker labels a temporary change to existing code): revert to the prior state. The marker line may carry explicit undo instructions (e.g. `revert: DEFAULT_TIMEOUT_MS = 1000`); the original may also be commented out directly above the tinker version, in which case remove the tinker line and uncomment the original.
- **File-level marker** (the marker is the first comment in the file): delete the entire file.

## When you finish

The project should contain zero real markers — zero comment lines starting with the marker form. String-literal and prose references stay. Verify by grepping for the comment-anchored pattern (e.g. lines matching `^\s*(//|#|--|;|\*|/\*)\s*tinker-test-case:`), not the bare string `tinker-test-case:` on its own. Report what you cleaned, one line per file."#
    )
}

async fn run_oc_oneshot(
    oc: &dyn OpenCodeRunner,
    message: &str,
    work_dir: &Path,
) -> Result<()> {
    let on_sid: Box<dyn FnMut(String) + Send> = Box::new(|_| {});
    let on_chunk: Box<dyn FnMut(String) + Send> = Box::new(|_| {});
    oc.run(message, None, work_dir, on_sid, on_chunk).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockFs;
    use crate::cap::Chunk;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_spec_find_marker_files_inline() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/lexer.rs"),
            "fn lex(s: &str) {}\n// tinker-test-case: probing f(1)(\n#[test] fn t() {}\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert_eq!(hits, vec![PathBuf::from("/proj/lexer.rs")]);
    }

    #[test]
    fn test_spec_find_marker_files_file_level() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/scratch_fuzzer.rs"),
            "// tinker-test-case: fuzz lexer for one hour\nfn main() {}\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert_eq!(hits, vec![PathBuf::from("/proj/scratch_fuzzer.rs")]);
    }

    #[test]
    fn test_spec_find_marker_files_skips_docs() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        // Design notes containing a real marker form — should NOT be flagged
        // because .md is skip-listed.
        fs.add_file(Path::new("/proj/design.md"),
            "Example: write `// tinker-test-case: probing foo` in your code",
        );
        fs.add_file(Path::new("/proj/Cargo.toml"),
            "# tinker-test-case: this would be a marker if .toml were scanned",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert!(hits.is_empty(), "doc files should not be scanned, got {hits:?}");
    }

    #[test]
    fn test_spec_find_marker_files_skips_known_dirs() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_dir(Path::new("/proj/.git"));
        fs.add_dir(Path::new("/proj/target"));
        fs.add_dir(Path::new("/proj/.tinker"));
        fs.add_file(Path::new("/proj/.git/HEAD"), "// tinker-test-case: would match if scanned");
        fs.add_file(Path::new("/proj/target/build.rs"), "// tinker-test-case: build artifact");
        fs.add_file(Path::new("/proj/.tinker/notes"), "// tinker-test-case: tinker's own state");
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert!(hits.is_empty(), "skip-listed dirs should be ignored, got {hits:?}");
    }

    #[test]
    fn test_spec_find_marker_files_ignores_prose_without_colon() {
        // The trailing colon is what distinguishes a marker from prose
        // references. Without the colon, no match — even at the start of a
        // comment line.
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/about.rs"),
            "// The orchestrator can use tinker-test-case markers to investigate.\nfn main() {}\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert!(hits.is_empty(), "no-colon prose should not match, got {hits:?}");
    }

    #[test]
    fn test_spec_marker_must_be_in_comment() {
        // A bare string-literal occurrence (e.g., a constant definition, a
        // prompt template, a test fixture) must NOT match — the marker is
        // recognized only when line-anchored under a comment delimiter.
        // This is what keeps tinker's own source from self-matching.
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/constants.rs"),
            // String literal that contains the marker text but isn't itself
            // a comment.
            r#"pub const M: &str = "tinker-test-case:";"#,
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert!(hits.is_empty(), "marker in string literal must not match, got {hits:?}");
    }

    #[test]
    fn test_spec_marker_recognized_with_python_comment() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/script.py"),
            "# tinker-test-case: probing the regex compile path\ndef main(): pass\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert_eq!(hits, vec![PathBuf::from("/proj/script.py")]);
    }

    #[test]
    fn test_spec_no_self_match_in_tinker_source() {
        // Regression: cleanup.rs (this file) and orchestrator.rs both
        // reference the marker in prose, string literals, and doc comments.
        // The line-anchored matcher must NOT see those as real markers,
        // otherwise running tinker against itself mangles its own source.
        let fs = crate::realfs::RealFilesystem;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for name in ["src/cleanup.rs", "src/orchestrator.rs"] {
            let content = fs.read_to_string(&root.join(name)).unwrap();
            assert!(
                !file_contains_marker(&content),
                "{name} self-matches the marker pattern — fix the matcher or the file",
            );
        }
    }

    #[test]
    fn test_spec_marker_recognized_with_indented_comment() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/nested.rs"),
            "fn outer() {\n    // tinker-test-case: probe inside block\n    inner();\n}\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert_eq!(hits, vec![PathBuf::from("/proj/nested.rs")]);
    }

    #[test]
    fn test_spec_find_marker_files_in_place_modification_shape() {
        // Goal decision: an "in-place modification" marker labels a change
        // to existing code with the original commented out directly above
        // the tinker version. Detection must still flag the file — the
        // marker comment line is what the scanner anchors on.
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/timeout.rs"),
            "// pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;\n// tinker-test-case: shortened timeout to reproduce flake\npub const DEFAULT_TIMEOUT_MS: u64 = 1_000;\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert_eq!(hits, vec![PathBuf::from("/proj/timeout.rs")]);
    }

    #[test]
    fn test_spec_cleanup_prompt_describes_three_marker_shapes() {
        // Goal decision: three shapes of marker are supported — inline
        // addition, in-place modification, file-level. The cleanup prompt
        // is what tells the cleanup agent how to handle each, so it must
        // name all three shapes.
        let prompt = build_cleanup_prompt(&[PathBuf::from("/proj/a.rs")]);
        assert!(prompt.to_lowercase().contains("inline addition"),
            "cleanup prompt should describe the inline-addition shape:\n{prompt}");
        assert!(prompt.to_lowercase().contains("in-place modification"),
            "cleanup prompt should describe the in-place-modification shape:\n{prompt}");
        assert!(prompt.to_lowercase().contains("file-level"),
            "cleanup prompt should describe the file-level shape:\n{prompt}");
    }

    #[test]
    fn test_spec_cleanup_does_not_rely_on_version_control() {
        // Goal decision: no reliance on version control. Undo information
        // travels in the marker comment itself (instructions in the
        // reason, or the original commented-out alongside) — never via
        // `git diff` or similar. The cleanup prompt must not direct the
        // agent to consult VCS.
        let prompt = build_cleanup_prompt(&[PathBuf::from("/proj/a.rs")]);
        let lower = prompt.to_lowercase();
        for forbidden in ["git diff", "git log", "git show", "git blame", "version control"] {
            assert!(
                !lower.contains(forbidden),
                "cleanup prompt must not reference `{forbidden}`:\n{prompt}",
            );
        }
    }

    #[test]
    fn test_spec_find_marker_files_recurses() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_dir(Path::new("/proj/src"));
        fs.add_dir(Path::new("/proj/src/parser"));
        fs.add_file(Path::new("/proj/src/parser/lexer.rs"),
            "// tinker-test-case: deep nested file\n",
        );
        let hits = find_marker_files(&fs, Path::new("/proj")).unwrap();
        assert_eq!(hits, vec![PathBuf::from("/proj/src/parser/lexer.rs")]);
    }

    /// Mock runner that "cleans up" by clearing the marker out of each
    /// matching file in MockFs on each call. Lets us drive the retry loop.
    struct CleaningRunner {
        fs: Arc<MockFs>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl OpenCodeRunner for CleaningRunner {
        async fn run(
            &self,
            _message: &str,
            _session_id: Option<&str>,
            _work_dir: &Path,
            _on_session_id: Chunk,
            _on_chunk: Chunk,
        ) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Drop any line that is a marker line — mirrors what a real
            // cleanup agent would do for inline markers.
            let hits = find_marker_files(self.fs.as_ref(), Path::new("/proj"))?;
            for path in hits {
                let content = self.fs.read_to_string(&path)?;
                let cleaned: String = content
                    .lines()
                    .filter(|l| !super::line_is_marker(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.fs.write(&path, &cleaned)?;
            }
            Ok(String::new())
        }
    }

    struct InertRunner {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl OpenCodeRunner for InertRunner {
        async fn run(
            &self,
            _message: &str,
            _session_id: Option<&str>,
            _work_dir: &Path,
            _on_session_id: Chunk,
            _on_chunk: Chunk,
        ) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn test_spec_run_cleanup_succeeds_on_first_attempt() {
        let fs = Arc::new(MockFs::new());
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/a.rs"), "// tinker-test-case: x\n");
        let runner = CleaningRunner {
            fs: fs.clone(),
            calls: AtomicUsize::new(0),
        };
        let outcome = run_cleanup(&runner, fs.as_ref(), Path::new("/proj"))
            .await
            .unwrap();
        assert!(matches!(outcome, CleanupOutcome::Clean));
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_spec_run_cleanup_clean_when_no_markers() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/a.rs"), "fn x() {}\n");
        let runner = InertRunner {
            calls: AtomicUsize::new(0),
        };
        let outcome = run_cleanup(&runner, &fs, Path::new("/proj"))
            .await
            .unwrap();
        assert!(matches!(outcome, CleanupOutcome::Clean));
        // No call when there were no markers to begin with.
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_spec_run_cleanup_fails_after_retries() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/proj"));
        fs.add_file(Path::new("/proj/a.rs"), "// tinker-test-case: stubborn\n");
        let runner = InertRunner {
            calls: AtomicUsize::new(0),
        };
        let outcome = run_cleanup(&runner, &fs, Path::new("/proj"))
            .await
            .unwrap();
        match outcome {
            CleanupOutcome::FailedAfterRetries(files) => {
                assert_eq!(files, vec![PathBuf::from("/proj/a.rs")]);
            }
            CleanupOutcome::Clean => panic!("expected FailedAfterRetries"),
        }
        // Three attempts, all no-ops.
        assert_eq!(runner.calls.load(Ordering::SeqCst), MAX_RETRIES);
    }
}
