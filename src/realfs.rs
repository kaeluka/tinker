//! Production `Filesystem` capability — delegates to `std::fs`.
//!
//! Constructed at the composition root (`main.rs`). Business logic depends
//! only on the `Filesystem` trait in `cap.rs`.

use crate::cap::Filesystem;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct RealFilesystem;

impl Filesystem for RealFilesystem {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))
    }

    fn write(&self, path: &Path, content: &str) -> Result<()> {
        std::fs::write(path, content)
            .with_context(|| format!("writing {}", path.display()))
    }

    fn mkdir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("mkdir -p {}", path.display()))
    }

    fn list_files_with_ext(&self, dir: &Path, ext: &str) -> Result<Vec<PathBuf>> {
        let mut out = vec![];
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("reading dir {}", dir.display()))?
        {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some(ext) {
                out.push(p);
            }
        }
        Ok(out)
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = vec![];
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("reading dir {}", dir.display()))?
        {
            let entry = entry?;
            out.push(entry.path());
        }
        Ok(out)
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn open_append(&self, path: &Path) -> Result<Box<dyn Write + Send>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {} for append", path.display()))?;
        Ok(Box::new(file))
    }
}
