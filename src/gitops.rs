use anyhow::{bail, Result};
use async_trait::async_trait;
use std::path::Path;
use std::process::Command;

use crate::cap::GitOps;

pub struct RealGitOps;

#[async_trait]
impl GitOps for RealGitOps {
    async fn worktree_add(&self, base: &Path, dest: &Path) -> Result<()> {
        let out = Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(dest)
            .current_dir(base)
            .output()?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    async fn worktree_remove(&self, path: &Path) -> Result<()> {
        let out = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .output()?;
        if !out.status.success() {
            bail!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }
}
