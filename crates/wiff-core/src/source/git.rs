//! A git diff of a repository as a [`DiffSource`].

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use async_trait::async_trait;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::record::SourceKind;
use crate::source::{CapturedDiff, DiffSource};

/// The context wiff asks git for around each hunk. A large window means a hunk
/// holds most or all of its file, so highlighting and rebasing have more to work
/// with while the diff stays the single artifact.
const GIT_CONTEXT_LINES: u32 = 3000;

/// Which slice of the repository a [`GitSource`] captures.
#[derive(Debug, Clone)]
enum Mode {
    /// The uncommitted working copy (`git diff` plus intent-to-add untracked).
    Worktree,
    /// The index against `HEAD` (`git diff --cached`).
    Index,
    /// The changes a single revision introduces (`git show REF`).
    Rev(String),
}

/// A git diff of a repository: the uncommitted working copy, the staged index
/// against `HEAD`, or the changes a single revision introduces.
#[derive(Debug, Clone)]
pub struct GitSource {
    repo_root: PathBuf,
    mode: Mode,
}

impl GitSource {
    /// A source for the uncommitted working copy: every change against the
    /// index, plus new-but-not-ignored files that a bare `git diff` would omit.
    pub fn worktree(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            mode: Mode::Worktree,
        }
    }

    /// A source for the index against `HEAD` (`git diff --cached`).
    pub fn index(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            mode: Mode::Index,
        }
    }

    /// A source for the changes `rev` introduces (`git show REF`).
    pub fn rev(repo_root: impl Into<PathBuf>, rev: impl Into<String>) -> Self {
        Self {
            repo_root: repo_root.into(),
            mode: Mode::Rev(rev.into()),
        }
    }

    fn kind(&self) -> SourceKind {
        match &self.mode {
            Mode::Worktree => SourceKind::GitWorktree,
            Mode::Index => SourceKind::GitIndex,
            Mode::Rev(rev) => SourceKind::GitRev { rev: rev.clone() },
        }
    }

    /// The full uncommitted working copy diff. To show untracked files as
    /// additions without disturbing the real index, we seed a throwaway index
    /// from it and record only the untracked, non-ignored files there as
    /// intent-to-add; modifications and deletions still show because they are
    /// never staged into the throwaway.
    async fn capture_worktree(&self) -> Result<String> {
        let index = self.seed_temp_index().await?;
        self.add_untracked_files_to_temp_index(index.path()).await?;
        self.run_diff(Some(index.path())).await
    }

    /// Copy the repo's real index into a temporary file. A repository without an
    /// index yet (freshly initialized, no commits) yields an empty throwaway, so
    /// every tracked-or-not file shows as an addition.
    async fn seed_temp_index(&self) -> Result<NamedTempFile> {
        let temp = NamedTempFile::new().map_err(|source| {
            Error::Source(format!("could not create a temporary index: {source}"))
        })?;
        let real = self.real_index_path().await?;
        match tokio::fs::read(&real).await {
            Ok(bytes) => {
                let handle = temp.as_file().try_clone().map_err(|source| {
                    Error::Source(format!("could not open the temporary index: {source}"))
                })?;
                let mut file = tokio::fs::File::from_std(handle);
                file.write_all(&bytes).await.map_err(|source| {
                    Error::Source(format!("could not write the temporary index: {source}"))
                })?;
                file.flush().await.map_err(|source| {
                    Error::Source(format!("could not write the temporary index: {source}"))
                })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Source(format!(
                    "could not read the git index: {source}"
                )));
            }
        }
        Ok(temp)
    }

    /// The path to the repo's real index file.
    async fn real_index_path(&self) -> Result<PathBuf> {
        let output = self.git(["rev-parse", "--git-path", "index"], None).await?;
        let text = String::from_utf8(output.stdout).map_err(|source| {
            Error::Source(format!("git rev-parse was not valid UTF-8: {source}"))
        })?;
        let path = Path::new(text.trim());
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.repo_root.join(path)
        })
    }

    /// Record every untracked, non-ignored file as intent-to-add in `index`, so
    /// it appears in the diff as a new file with its full content.
    async fn add_untracked_files_to_temp_index(&self, index: &Path) -> Result<()> {
        let output = self
            .git(["ls-files", "--others", "--exclude-standard", "-z"], None)
            .await?;
        let untracked: Vec<OsString> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| OsStr::from_bytes(path).to_owned())
            .collect();
        if untracked.is_empty() {
            return Ok(());
        }
        let mut args: Vec<OsString> = vec!["add".into(), "--intent-to-add".into(), "--".into()];
        args.extend(untracked);
        self.git(args, Some(index)).await?;
        Ok(())
    }

    /// Run `git diff` with expanded context, optionally against an alternate
    /// index, returning its text.
    async fn run_diff(&self, index: Option<&Path>) -> Result<String> {
        let mut args: Vec<OsString> = vec![
            "diff".into(),
            format!("--unified={GIT_CONTEXT_LINES}").into(),
        ];
        if matches!(self.mode, Mode::Index) {
            args.push("--cached".into());
        }
        let output = self.git(args, index).await?;
        String::from_utf8(output.stdout)
            .map_err(|source| Error::Source(format!("git diff was not valid UTF-8: {source}")))
    }

    /// The patch a single revision introduces (`git show REF`).
    async fn capture_rev(&self, rev: &str) -> Result<String> {
        // An empty --format= suppresses the commit log, so the sideband holds
        // only the diff.
        let args: [OsString; 4] = [
            "show".into(),
            "--format=".into(),
            format!("--unified={GIT_CONTEXT_LINES}").into(),
            rev.into(),
        ];
        let output = self.git(args, None).await?;
        String::from_utf8(output.stdout)
            .map_err(|source| Error::Source(format!("git show was not valid UTF-8: {source}")))
    }

    /// Run a git subcommand under the repo and return its output on success.
    /// When `index` is set, git operates against that index file instead of the
    /// repo's real one.
    ///
    /// git is started in a fresh session so it has no controlling terminal.
    /// stdin on /dev/null is not enough on its own: git opens /dev/tty directly
    /// to prompt for credentials, which would hang us (and fight the TUI for the
    /// terminal). Without a controlling terminal that open fails, so a diff
    /// needing credentials fails cleanly instead. setsid(2) is async-signal-safe,
    /// so it is safe to call in pre_exec.
    async fn git<I, S>(&self, args: I, index: Option<&Path>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let subcommand = args
            .first()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.repo_root).args(&args);
        command.stdin(Stdio::null());
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = tokio::process::Command::from(command)
            .output()
            .await
            .map_err(|source| Error::Source(format!("could not run git: {source}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Source(format!(
                "git {subcommand} failed ({}): {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(output)
    }
}

#[async_trait]
impl DiffSource for GitSource {
    async fn capture(&self) -> Result<CapturedDiff> {
        let text = match &self.mode {
            Mode::Worktree => self.capture_worktree().await?,
            Mode::Index => self.run_diff(None).await?,
            Mode::Rev(rev) => self.capture_rev(rev).await?,
        };
        Ok(CapturedDiff {
            text,
            source: self.kind(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::GitSource;
    use crate::record::SourceKind;
    use crate::source::DiffSource;

    /// Run `git` with `args` in `repo` under a laundered environment so neither
    /// the setup nor the capture under test can pick up host or per-user git
    /// configuration: the environment is emptied, `HOME` points at an empty
    /// `home` directory, and system config is disabled outright.
    fn git(repo: &Path, home: &Path, args: &[&str]) {
        let output = Command::new("git")
            .env_clear()
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["-c", "user.name=wez", "-c", "user.email=wez@example.com"])
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

    /// Blank the variable `index <old>..<new>` blob hashes so the captured patch
    /// can be asserted whole.
    fn stable(text: &str) -> String {
        text.lines()
            .map(|line| {
                if line.starts_with("index ") {
                    "index HASHES".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_revision_source_captures_the_commit_patch() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "add f"]);

        let captured = GitSource::rev(repo.path(), "HEAD")
            .capture()
            .await
            .expect("capture");
        let expected = "\
diff --git a/f.txt b/f.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,2 @@
+alpha
+beta";
        k9::assert_equal!(stable(&captured.text), expected.to_string());
        k9::assert_equal!(
            captured.source,
            SourceKind::GitRev {
                rev: "HEAD".to_string()
            }
        );
    }
}
