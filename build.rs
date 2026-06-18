//! Build-time population of the packaged tier.
//!
//! The population contract: every `cargo build`
//! and `cargo install --path .` produces a binary whose
//! `<binary>/../../.tinker/goals/packaged-goals/` directory contains the
//! source `.tinker/goals/packaged-goals/*.toml` files. Without this step,
//! the runtime's third pass in `goal::load_all_goals`
//! finds an empty directory in dev mode and relies on the embedded fallback
//! — which works but loses the ability to edit packaged goals without
//! recompiling.
//!
//! This script does NOT cover `cargo install` from a package registry: there
//! no build step runs, so the on-disk copy is absent and the loader falls
//! through to the embedded fallback (the `include_dir!` macro in
//! `src/goal.rs`).

use std::fs;
use std::path::PathBuf;

/// Source directory holding tinker-shipped packaged goals, relative to
/// `CARGO_MANIFEST_DIR`. The single source of truth — `goal-storage` reads
/// this same path as its second tier at runtime, and `binary-relative-tier-loading`'s
/// `include_dir!` macro reads it at compile time as the embedded fallback.
const SOURCE_PACKAGED_DIR: &str = ".tinker/goals/packaged-goals";

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by Cargo during build"),
    );
    let source_dir = manifest_dir.join(SOURCE_PACKAGED_DIR);

    // Re-run when the packaged-goals contents or this script change.
    // Cargo's automatic tracking of `include_dir!`-read files in `src/goal.rs`
    // handles the embedded-fallback side; this side needs an explicit hint
    // because the build script reads the directory itself.
    println!("cargo:rerun-if-changed={}", SOURCE_PACKAGED_DIR);
    println!("cargo:rerun-if-changed=build.rs");

    // The source directory must exist. A missing or empty source is a real
    // error — fail loud so the eager-start invariant doesn't break at
    // runtime via a confusing "tend not found" panic that points at a
    // directory the user can't reach.
    if !source_dir.is_dir() {
        panic!(
            "binary-relative-tier-loading: source packaged-goals directory missing at `{}` — \
             tinker requires `.tinker/goals/packaged-goals/*.toml` to exist in the source tree \
             so build.rs can copy them to the binary-relative tier.",
            source_dir.display(),
        );
    }

    let toml_files: Vec<PathBuf> = match fs::read_dir(&source_dir) {
        Ok(rd) => rd
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().map(|e| e == "toml").unwrap_or(false))
            .collect(),
        Err(e) => panic!(
            "binary-relative-tier-loading: could not read source packaged-goals directory `{}`: {}",
            source_dir.display(),
            e,
        ),
    };

    if toml_files.is_empty() {
        panic!(
            "binary-relative-tier-loading: source packaged-goals directory `{}` is empty — \
             tinker requires at least `tend.toml` to satisfy the eager-start invariant. \
             Add the packaged goals (run `git restore .tinker/goals/packaged-goals/` if missing) \
             and rebuild.",
            source_dir.display(),
        );
    }

    // Resolve the cargo target directory by walking up from OUT_DIR.
    // Standard layout: OUT_DIR = <target_dir>/<profile>/build/<package>-<hash>/out
    // so four `.parent()` calls land on target_dir. Fragile if Cargo ever
    // changes the layout, but the alternative (parsing `cargo build --verbose`
    // output) is worse. Cargo's documented contract is that build scripts
    // can use OUT_DIR for build artifacts under <target_dir>, and the
    // target_dir is the natural sibling for the binary-relative packaged tier.
    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR").expect("OUT_DIR must be set by Cargo during build"),
    );
    let target_dir = out_dir
        .parent() // build/tinker-<hash>
        .and_then(|p| p.parent()) // build
        .and_then(|p| p.parent()) // <profile>
        .and_then(|p| p.parent()) // <target_dir>
        .expect(
            "OUT_DIR must have the standard layout: \
             <target_dir>/<profile>/build/<package>-<hash>/out",
        )
        .to_path_buf();

    let dest_dir = target_dir.join(".tinker").join("goals").join("packaged-goals");
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        panic!(
            "binary-relative-tier-loading: could not create destination directory `{}`: {}",
            dest_dir.display(),
            e,
        );
    }

    for src in &toml_files {
        let file_name = src
            .file_name()
            .expect("source path must have a filename component");
        let dst = dest_dir.join(file_name);
        if let Err(e) = fs::copy(src, &dst) {
            panic!(
                "binary-relative-tier-loading: could not copy `{}` to `{}`: {}",
                src.display(),
                dst.display(),
                e,
            );
        }
    }

    // Emit a build-script info line so `cargo build -v` shows the population
    // happened and the destination. Useful when debugging "why does my dev
    // binary see no packaged goals?" — the population is visible in build output.
    println!(
        "binary-relative-tier-loading: copied {} packaged-goals TOML files to `{}`",
        toml_files.len(),
        dest_dir.display(),
    );
}
