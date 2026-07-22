//! Shared helpers for the crate's tests.

use std::path::Path;
use std::process::Command;

/// Run `git` in `repo` under a laundered environment so neither ambient nor
/// per-user configuration can perturb the test: the environment is emptied,
/// `HOME` points at a throwaway directory, and system and global config are
/// disabled outright. A fixed identity with signing off keeps any commit
/// deterministic. Panics if git cannot be spawned or exits nonzero.
pub(crate) fn git(repo: &Path, args: &[&str]) {
    let home = tempfile::tempdir().expect("git home tempdir");
    let output = Command::new("git")
        .env_clear()
        .env("HOME", home.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["-c", "user.name=wiff", "-c", "user.email=wiff@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
