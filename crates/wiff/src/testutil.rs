//! Shared helpers for the crate's tests.

use std::path::Path;
use std::process::Command;

/// Run `git` in `repo` for its effect under a laundered environment, panicking
/// if git cannot be spawned or exits nonzero. See [`git_out`] for the isolation
/// this provides.
pub(crate) fn git(repo: &Path, args: &[&str]) {
    git_out(repo, args);
}

/// Run `git` in `repo` and return its trimmed stdout, for reading back the
/// commit ids a test's setup produced.
///
/// Neither ambient nor per-user configuration can perturb the run: the
/// environment is emptied, `HOME` points at a throwaway directory, and system
/// and global config are disabled outright. A fixed identity with signing off
/// keeps any commit deterministic. Panics if git cannot be spawned or exits
/// nonzero.
pub(crate) fn git_out(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout)
        .expect("git stdout is utf-8")
        .trim()
        .to_string()
}
