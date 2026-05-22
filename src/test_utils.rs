//! Test-only helpers. Compiled only under `#[cfg(test)]`.

use crate::cap::Filesystem;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// In-memory `Filesystem` for tests.
pub struct MockFs {
    files: Mutex<HashMap<PathBuf, String>>,
    dirs: Mutex<HashSet<PathBuf>>,
}

impl MockFs {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashSet::new()),
        }
    }

    /// Pre-populate a file. Marks the file's parent directories as existing.
    pub fn add_file(&self, path: &Path, content: &str) {
        let path = path.to_path_buf();
        let mut cur = path.parent();
        let mut dirs = self.dirs.lock().unwrap();
        while let Some(p) = cur {
            dirs.insert(p.to_path_buf());
            cur = p.parent();
        }
        drop(dirs);
        self.files
            .lock()
            .unwrap()
            .insert(path, content.to_string());
    }

    pub fn add_dir(&self, path: &Path) {
        let mut cur = Some(path.to_path_buf());
        let mut dirs = self.dirs.lock().unwrap();
        while let Some(p) = cur {
            let parent = p.parent().map(|p| p.to_path_buf());
            dirs.insert(p);
            cur = parent;
        }
    }

}

impl Filesystem for MockFs {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("no such file: {}", path.display()))
    }

    fn write(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            self.dirs.lock().unwrap().insert(parent.to_path_buf());
        }
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), content.to_string());
        Ok(())
    }

    fn mkdir_all(&self, path: &Path) -> Result<()> {
        let mut cur = Some(path.to_path_buf());
        let mut dirs = self.dirs.lock().unwrap();
        while let Some(p) = cur {
            let parent = p.parent().map(|p| p.to_path_buf());
            dirs.insert(p);
            cur = parent;
        }
        Ok(())
    }

    fn list_files_with_ext(&self, dir: &Path, ext: &str) -> Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let mut out: Vec<PathBuf> = files
            .keys()
            .filter(|p| p.parent() == Some(dir))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let dirs = self.dirs.lock().unwrap();
        let mut out: Vec<PathBuf> = files
            .keys()
            .filter(|p| p.parent() == Some(dir))
            .cloned()
            .collect();
        for d in dirs.iter() {
            if d.parent() == Some(dir) {
                out.push(d.clone());
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.dirs.lock().unwrap().contains(path)
    }
}
