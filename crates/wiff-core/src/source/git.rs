//! A git diff of a repository as a [`DiffSource`].

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use async_trait::async_trait;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;

use crate::base_resolve::{RevisionResolver, resolve_base};
use crate::base_ruleset::{BaseRuleset, parse_ruleset};
use crate::error::{Error, Result};
use crate::identity::ScmType;
use crate::record::{RevisionId, ScmSource, SourceKind, TipRule};
use crate::source::{CapturedDiff, DiffSource, HeadBranch};

/// The context wiff asks git for around each hunk. A large window means a hunk
/// holds most or all of its file, so highlighting and rebasing have more to work
/// with while the diff stays the single artifact.
const GIT_CONTEXT_LINES: u32 = 3000;

/// The environment for a git subcommand: an alternate index file, and a
/// writable scratch object directory paired with the repo's real objects as an
/// alternate. The scratch directory lets a capture that must write objects (an
/// intent-to-add of untracked files) run against a `.git` mounted read-only,
/// with the redirected writes discarded when the capture completes.
#[derive(Default, Clone, Copy)]
struct GitEnv<'a> {
    /// The `GIT_INDEX_FILE` git operates against, replacing the repo's real one.
    index: Option<&'a Path>,
    /// The `GIT_OBJECT_DIRECTORY` git writes new objects into.
    scratch_objects: Option<&'a Path>,
    /// The repo's real object directory, offered via
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES` so reads still find existing objects
    /// when writes are redirected to [`Self::scratch_objects`].
    real_objects: Option<&'a Path>,
}

/// Revision resolution and diff production against a git repository. Holds the
/// git subprocess plumbing that both the base-ruleset resolver and a capture run
/// through, so a [`GitSource`] and a bare resolution both speak to git the same
/// way (a laundered process with no controlling terminal, an alternate index and
/// object directory when a capture must write).
#[derive(Debug, Clone)]
pub struct GitRepo {
    repo_root: PathBuf,
}

impl GitRepo {
    /// A handle to the git repository rooted at `repo_root`.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
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
        self.git_path("index").await
    }

    /// The path to the repo's real object directory.
    async fn real_objects_path(&self) -> Result<PathBuf> {
        self.git_path("objects").await
    }

    /// Resolve `name` under the repo's git directory to an absolute path.
    async fn git_path(&self, name: &str) -> Result<PathBuf> {
        let output = self
            .git(["rev-parse", "--git-path", name], GitEnv::default())
            .await?;
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
    async fn add_untracked_files_to_temp_index(&self, env: GitEnv<'_>) -> Result<()> {
        let output = self
            .git(
                ["ls-files", "--others", "--exclude-standard", "-z"],
                GitEnv::default(),
            )
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
        self.git(args, env).await?;
        Ok(())
    }

    /// Run `git diff` with expanded context and the given trailing arguments
    /// (the base, and for a two-commit diff the tip), under `env`.
    async fn diff(&self, trailing: &[OsString], env: GitEnv<'_>) -> Result<String> {
        let mut args: Vec<OsString> = vec![
            "diff".into(),
            format!("--unified={GIT_CONTEXT_LINES}").into(),
        ];
        args.extend_from_slice(trailing);
        let output = self.git(args, env).await?;
        String::from_utf8(output.stdout)
            .map_err(|source| Error::Source(format!("git diff was not valid UTF-8: {source}")))
    }

    /// Run a git subcommand under the repo and return its output on success.
    /// `env` redirects git's index and object storage away from the repo's real
    /// ones when set.
    ///
    /// git is started in a fresh session so it has no controlling terminal.
    /// stdin on /dev/null is not enough on its own: git opens /dev/tty directly
    /// to prompt for credentials, which would hang us (and fight the TUI for the
    /// terminal). Without a controlling terminal that open fails, so a diff
    /// needing credentials fails cleanly instead. setsid(2) is async-signal-safe,
    /// so it is safe to call in pre_exec.
    async fn git<I, S>(&self, args: I, env: GitEnv<'_>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let output = self.spawn(&args, env).await?;
        if !output.status.success() {
            let subcommand = args
                .first()
                .map(|arg| arg.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Source(format!(
                "git {subcommand} failed ({}): {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(output)
    }

    /// Run a git subcommand and return its raw output whatever the exit status,
    /// failing only when the process could not be started. A nonzero exit is a
    /// result for the caller to interpret.
    async fn spawn(&self, args: &[OsString], env: GitEnv<'_>) -> Result<Output> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.repo_root).args(args);
        command.stdin(Stdio::null());
        if let Some(index) = env.index {
            command.env("GIT_INDEX_FILE", index);
        }
        if let Some(objects) = env.scratch_objects {
            command.env("GIT_OBJECT_DIRECTORY", objects);
        }
        if let Some(alternate) = env.real_objects {
            command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
        }
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        tokio::process::Command::from(command)
            .output()
            .await
            .map_err(|source| Error::Source(format!("could not run git: {source}")))
    }

    /// Run a git query that prints a single revision, returning it trimmed.
    /// Exit code 1 is git's "named nothing" for the queries resolution runs (an
    /// unknown ref under `rev-parse --verify --quiet`, no common ancestor under
    /// `merge-base`) and yields `None`; any other nonzero exit is a genuine git
    /// failure, reported with its stderr rather than mistaken for a
    /// fall-through. Non-UTF-8 output is a hard error.
    async fn rev_query(&self, args: &[OsString]) -> Result<Option<RevisionId>> {
        let output = self.spawn(args, GitEnv::default()).await?;
        match output.status.code() {
            Some(0) => {}
            Some(1) => return Ok(None),
            _ => {
                let subcommand = args
                    .first()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Error::Source(format!(
                    "git {subcommand} failed ({}): {}",
                    output.status,
                    stderr.trim()
                )));
            }
        }
        let text = String::from_utf8(output.stdout).map_err(|source| {
            Error::Source(format!("git printed a non-UTF-8 revision: {source}"))
        })?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(RevisionId(trimmed.to_string())))
    }

    /// Resolve a revision spec to a commit through `rev-parse`, peeling to a
    /// commit. `--end-of-options` keeps a spec beginning with `-` from being
    /// read as a git option, and `--verify --quiet` makes an unresolvable spec
    /// exit 1, which [`rev_query`](Self::rev_query) reports as `None`.
    async fn resolve_commit(&self, spec: &str) -> Result<Option<RevisionId>> {
        self.rev_query(&[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            format!("{spec}^{{commit}}").into(),
        ])
        .await
    }

    /// The full ref name `spec` names (`refs/heads/...`, `refs/tags/...`), or
    /// `None` when it does not name a ref: a bare revision, a detached `HEAD`,
    /// or a spec that resolves to nothing at all.
    ///
    /// `rev-parse --symbolic-full-name` prints empty for a bare revision, the
    /// literal `HEAD` for a detached head, and exits nonzero for an unresolvable
    /// spec; only an answer under `refs/` is a ref whose name is worth keeping.
    /// The full name is returned rather than the spec so a later re-resolution is
    /// unambiguous when a tag and a branch share a short name.
    async fn symbolic_ref(&self, spec: &str) -> Result<Option<String>> {
        let output = self
            .spawn(
                &[
                    "rev-parse".into(),
                    "--symbolic-full-name".into(),
                    spec.into(),
                ],
                GitEnv::default(),
            )
            .await?;
        ref_name_from_symbolic(output.status.success(), &output.stdout)
    }
}

/// Interpret the output of `rev-parse --symbolic-full-name <spec>`: the full ref
/// name when git named one under `refs/`, or `None` for a bare revision, a
/// detached head (git echoes the literal `HEAD`), or an unresolvable spec (git
/// exits nonzero).
fn ref_name_from_symbolic(success: bool, stdout: &[u8]) -> Result<Option<String>> {
    if !success {
        return Ok(None);
    }
    let text = std::str::from_utf8(stdout)
        .map_err(|source| Error::Source(format!("git rev-parse was not valid UTF-8: {source}")))?;
    let name = text.trim();
    Ok(name.starts_with("refs/").then(|| name.to_string()))
}

/// The branch state of the git repository at `repo_root`: the branch it is on
/// (full ref name, `refs/heads/...`), a detached head, or an unknown state when
/// git cannot be reached or gives an answer that cannot be read. The sync
/// counterpart to [`GitRepo::symbolic_ref`] for paths that run outside an async
/// context; both run the same `rev-parse --symbolic-full-name HEAD`. The child
/// inherits no stdin and runs against `-C repo_root`, matching the process
/// policy of [`GitRepo::spawn`].
pub fn head_branch(repo_root: &Path) -> HeadBranch {
    let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--symbolic-full-name", "HEAD"])
        .stdin(Stdio::null())
        .output()
    else {
        return HeadBranch::Unknown;
    };
    if !output.status.success() {
        return HeadBranch::Unknown;
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return HeadBranch::Unknown;
    };
    let name = text.trim();
    if name.starts_with("refs/") {
        HeadBranch::On(name.to_string())
    } else {
        // Git echoes the literal `HEAD` for a detached head.
        HeadBranch::Detached
    }
}

#[async_trait]
impl RevisionResolver for GitRepo {
    fn scm(&self) -> ScmType {
        ScmType::Git
    }

    async fn resolve_ref(&self, name: &str) -> Result<Option<RevisionId>> {
        self.resolve_commit(name).await
    }

    async fn trunk(&self) -> Result<Option<RevisionId>> {
        // The default branch is the remote's HEAD, falling back to a local main
        // or master when no remote HEAD is set (a freshly initialized repo, or a
        // clone before git records origin's head).
        for candidate in ["origin/HEAD", "main", "master"] {
            if let Some(rev) = self.resolve_commit(candidate).await? {
                return Ok(Some(rev));
            }
        }
        Ok(None)
    }

    async fn upstream(&self) -> Result<Option<RevisionId>> {
        self.resolve_commit("@{upstream}").await
    }

    async fn parent(&self, rev: &RevisionId) -> Result<Option<RevisionId>> {
        // A root commit has no first parent; fall back to the empty tree.
        match self.resolve_commit(&format!("{}^1", rev.as_str())).await? {
            Some(parent) => Ok(Some(parent)),
            None => self.empty().await,
        }
    }

    async fn merge_base(&self, rev: &RevisionId, tip: &RevisionId) -> Result<Option<RevisionId>> {
        self.rev_query(&[
            "merge-base".into(),
            "--end-of-options".into(),
            rev.as_str().into(),
            tip.as_str().into(),
        ])
        .await
    }

    async fn native(&self, expr: &str) -> Result<Option<RevisionId>> {
        // An scm-native escape hatch for expressions the fixed operators cannot
        // form; the expression reaches rev-parse verbatim with no ^{commit} peel
        // appended, and --end-of-options guards a leading dash.
        self.rev_query(&[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            expr.into(),
        ])
        .await
    }

    async fn empty(&self) -> Result<Option<RevisionId>> {
        // Hashing an empty tree yields the repo's empty-tree object name under
        // whatever hash algorithm it uses, rather than assuming the sha1
        // constant. Without -w the object is never written; git special-cases
        // the empty tree so it serves as a diff base even when absent from the
        // object store.
        self.rev_query(&[
            "hash-object".into(),
            "-t".into(),
            "tree".into(),
            "/dev/null".into(),
        ])
        .await
    }
}

/// A git diff of a reviewed range: a base ruleset and a tip rule that resolve to
/// concrete commits, then diffed. A working-tree or index tip diffs the resolved
/// base against the uncommitted state; a ref or pinned tip diffs the base against
/// the resolved tip commit.
#[derive(Debug, Clone)]
pub struct GitSource {
    repo: GitRepo,
    base: BaseRuleset,
    tip: TipRule,
}

impl GitSource {
    /// A source reviewing the uncommitted working copy against `base`.
    pub fn worktree(repo_root: impl Into<PathBuf>, base: BaseRuleset) -> Self {
        Self {
            repo: GitRepo::new(repo_root),
            base,
            tip: TipRule::Worktree,
        }
    }

    /// A source reviewing the staged index against `base` (`git diff --cached`).
    pub fn index(repo_root: impl Into<PathBuf>, base: BaseRuleset) -> Self {
        Self {
            repo: GitRepo::new(repo_root),
            base,
            tip: TipRule::Index,
        }
    }

    /// A source reviewing `base` against the commit a ref or pinned `tip`
    /// resolves to.
    pub fn revision(repo_root: impl Into<PathBuf>, base: BaseRuleset, tip: TipRule) -> Self {
        Self {
            repo: GitRepo::new(repo_root),
            base,
            tip,
        }
    }

    /// Build a source reviewing `change` against `base`. A `change` that names a
    /// ref is tracked as a [`Ref`](TipRule::Ref) tip under its full ref name;
    /// anything else (a bare revision, a detached `HEAD`) is held at the commit
    /// it resolves to as a [`Pinned`](TipRule::Pinned) tip. Errors when `change`
    /// resolves to no commit.
    pub async fn change(
        repo_root: impl Into<PathBuf>,
        base: BaseRuleset,
        change: String,
    ) -> Result<Self> {
        let repo = GitRepo::new(repo_root);
        let tip = match repo.symbolic_ref(&change).await? {
            Some(name) => TipRule::Ref { name },
            None => {
                let revision = repo.resolve_commit(&change).await?.ok_or_else(|| {
                    Error::Source(format!("'{change}' did not resolve to a commit"))
                })?;
                TipRule::Pinned { revision }
            }
        };
        Ok(Self { repo, base, tip })
    }

    /// The base ruleset that pins a review at the repository's current commit, or
    /// the empty tree when HEAD is unborn (a repository with no commits yet).
    pub async fn pinned_base_at_head(repo_root: impl Into<PathBuf>) -> Result<BaseRuleset> {
        let repo = GitRepo::new(repo_root);
        Ok(match repo.resolve_ref("HEAD").await? {
            Some(head) => BaseRuleset::pinned(&head),
            None => BaseRuleset::empty(),
        })
    }

    /// Resolve the tip rule to the commit the base ruleset resolves against. A
    /// working-tree or index tip sits on the current commit, or the empty tree
    /// when HEAD is unborn.
    async fn resolve_tip(&self) -> Result<RevisionId> {
        match &self.tip {
            TipRule::Worktree | TipRule::Index => {
                match self.repo.resolve_ref("HEAD").await? {
                    Some(head) => Ok(head),
                    None => self.repo.empty().await?.ok_or_else(|| {
                        Error::Source("git could not name the empty tree".to_string())
                    }),
                }
            }
            TipRule::Ref { name } => self.repo.resolve_ref(name).await?.ok_or_else(|| {
                Error::Source(format!("revision '{name}' did not resolve to a commit"))
            }),
            TipRule::Pinned { revision } => self
                .repo
                .resolve_commit(revision.as_str())
                .await?
                .ok_or_else(|| {
                    Error::Source(format!("revision '{revision}' did not resolve to a commit"))
                }),
            TipRule::ChangeId { id } => Err(Error::Source(format!(
                "a change-id tip ({id}) is not supported under git"
            ))),
        }
    }

    /// The uncommitted working copy diffed against `base`. To show untracked
    /// files as additions without disturbing the real index, we seed a throwaway
    /// index and record only the untracked, non-ignored files there as
    /// intent-to-add; modifications and deletions still show because they are
    /// never staged into the throwaway.
    async fn capture_worktree(&self, base: &RevisionId) -> Result<String> {
        let index = self.repo.seed_temp_index().await?;
        // git add --intent-to-add writes an empty blob into the object database,
        // which fails when .git is mounted read-only. Redirect object writes to a
        // throwaway directory, keeping the repo's real objects readable as an
        // alternate, so an untracked file still shows without touching the repo.
        let scratch = tempfile::tempdir().map_err(|source| {
            Error::Source(format!(
                "could not create a temporary object directory: {source}"
            ))
        })?;
        let real_objects = self.repo.real_objects_path().await?;
        let env = GitEnv {
            index: Some(index.path()),
            scratch_objects: Some(scratch.path()),
            real_objects: Some(&real_objects),
        };
        self.repo.add_untracked_files_to_temp_index(env).await?;
        self.repo.diff(&[base.as_str().into()], env).await
    }
}

#[async_trait]
impl DiffSource for GitSource {
    async fn capture(&self) -> Result<CapturedDiff> {
        let ruleset = parse_ruleset(self.base.as_str())?;
        let tip = self.resolve_tip().await?;
        let resolved = resolve_base(&ruleset, &tip, &self.repo)
            .await?
            .ok_or_else(|| {
                Error::Source(format!(
                    "base ruleset '{}' did not resolve to any commit",
                    self.base
                ))
            })?;
        let base = resolved.revision;
        let (text, head_revision) = match &self.tip {
            TipRule::Worktree => (self.capture_worktree(&base).await?, None),
            TipRule::Index => (
                self.repo
                    .diff(
                        &["--cached".into(), base.as_str().into()],
                        GitEnv::default(),
                    )
                    .await?,
                None,
            ),
            // Any committed tip resolves to a commit and is diffed against the
            // base; resolve_tip has already rejected a change-id tip under git.
            _ => (
                self.repo
                    .diff(
                        &[base.as_str().into(), tip.as_str().into()],
                        GitEnv::default(),
                    )
                    .await?,
                Some(tip.clone()),
            ),
        };
        let branch_hint = match &self.tip {
            TipRule::Worktree | TipRule::Index => self.repo.symbolic_ref("HEAD").await?,
            _ => None,
        };
        Ok(CapturedDiff {
            text,
            source: SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: self.base.clone(),
                tip: self.tip.clone(),
                branch_hint,
            }),
            base_revision: Some(base),
            base_tip_relative: resolved.tip_relative,
            head_revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::{Command, Output};

    use super::{GitRepo, GitSource};
    use crate::base_resolve::{ResolvedBase, RevisionResolver, resolve_base};
    use crate::base_ruleset::{BaseRuleset, parse_ruleset};
    use crate::identity::ScmType;
    use crate::record::{RevisionId, ScmSource, SourceKind, TipRule};
    use crate::source::DiffSource;

    /// Run `git` with `args` in `repo` under a laundered environment so neither
    /// the setup nor the capture under test can pick up host or per-user git
    /// configuration: the environment is emptied, `HOME` points at an empty
    /// `home` directory, and system config is disabled outright.
    fn run_git(repo: &Path, home: &Path, args: &[&str]) -> Output {
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
        output
    }

    /// Run `git` in `repo` for its effect, asserting it succeeded.
    fn git(repo: &Path, home: &Path, args: &[&str]) {
        run_git(repo, home, args);
    }

    /// Run `git` in `repo` and return its trimmed stdout, for reading back the
    /// commit ids a test's setup produced.
    fn git_out(repo: &Path, home: &Path, args: &[&str]) -> String {
        String::from_utf8(run_git(repo, home, args).stdout)
            .expect("utf-8")
            .trim()
            .to_string()
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

        let head = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let empty_tree = RevisionId(git_out(
            repo.path(),
            home.path(),
            &["hash-object", "-t", "tree", "/dev/null"],
        ));

        // A ref tip against a parent(@) base: for a root commit the parent falls
        // back to the empty tree, so the diff is the whole commit, as git show
        // gave before.
        let captured = GitSource::revision(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            TipRule::Ref {
                name: "HEAD".to_string(),
            },
        )
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
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Ref {
                    name: "HEAD".to_string()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.base_revision, Some(empty_tree));
        // The parent(@) base follows the tip, so a refresh treats its move as
        // expected.
        wince::assert_eq!(captured.base_tip_relative, true);
        wince::assert_eq!(captured.head_revision, Some(head));
    }

    #[tokio::test]
    async fn a_branch_change_tracks_its_newest_commit_across_a_recapture() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));

        // A short branch name is recorded under its full ref name, so a later
        // re-resolution never collides with a like-named tag.
        let source = GitSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "main".to_string(),
        )
        .await
        .expect("change by branch");
        let captured = source.capture().await.expect("capture branch");
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Ref {
                    name: "refs/heads/main".to_string()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.head_revision, Some(first.clone()));

        // Advancing the branch and re-capturing the same source follows the tip
        // to the new commit, proving a Ref re-resolves rather than holding.
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        git(repo.path(), home.path(), &["commit", "-qa", "-m", "second"]);
        let second = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let recaptured = source.capture().await.expect("recapture branch");
        wince::assert_eq!(recaptured.head_revision, Some(second));
    }

    #[tokio::test]
    async fn a_bare_revision_change_holds_its_commit_across_a_recapture() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));

        // The same commit named as a bare revision is held, not tracked.
        let source = GitSource::change(repo.path(), BaseRuleset::new("parent(@)"), first.0.clone())
            .await
            .expect("change by revision");
        let captured = source.capture().await.expect("capture revision");
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Pinned {
                    revision: first.clone()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.head_revision, Some(first.clone()));

        // Advancing the branch leaves the pinned tip on the commit it named.
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        git(repo.path(), home.path(), &["commit", "-qa", "-m", "second"]);
        let recaptured = source.capture().await.expect("recapture revision");
        wince::assert_eq!(recaptured.head_revision, Some(first));
    }

    #[tokio::test]
    async fn a_change_of_head_while_detached_is_pinned_not_tracked() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));

        // A detached HEAD is not a ref: git rev-parse --symbolic-full-name HEAD
        // echoes the literal "HEAD", which must be held as a pinned commit rather
        // than followed like a branch.
        git(
            repo.path(),
            home.path(),
            &["checkout", "-q", first.as_str()],
        );
        let source = GitSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "HEAD".to_string(),
        )
        .await
        .expect("change by detached HEAD");
        let captured = source.capture().await.expect("capture detached HEAD");
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Pinned {
                    revision: first.clone()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.head_revision, Some(first));
    }

    #[tokio::test]
    async fn a_change_naming_no_commit_is_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);

        let error = GitSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "nope".to_string(),
        )
        .await
        .expect_err("unresolved change");
        wince::assert_eq!(
            error.to_string(),
            "could not capture diff: 'nope' did not resolve to a commit".to_string()
        );
    }

    #[tokio::test]
    async fn a_merge_revision_shows_its_first_parent_net_change() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("main.txt"), "base\n").expect("write");
        git(repo.path(), home.path(), &["add", "main.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "base"]);
        let mainline = git_out(
            repo.path(),
            home.path(),
            &["rev-parse", "--abbrev-ref", "HEAD"],
        );

        // A feature branch adds its own file, while the mainline advances
        // independently, so the merge has genuinely divergent parents.
        git(repo.path(), home.path(), &["switch", "-q", "-c", "feature"]);
        std::fs::write(repo.path().join("feature.txt"), "feat\n").expect("write");
        git(repo.path(), home.path(), &["add", "feature.txt"]);
        git(
            repo.path(),
            home.path(),
            &["commit", "-q", "-m", "add feature"],
        );
        git(repo.path(), home.path(), &["switch", "-q", &mainline]);
        std::fs::write(repo.path().join("main2.txt"), "mainline\n").expect("write");
        git(repo.path(), home.path(), &["add", "main2.txt"]);
        git(
            repo.path(),
            home.path(),
            &["commit", "-q", "-m", "advance mainline"],
        );
        git(
            repo.path(),
            home.path(),
            &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
        );

        let head = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let first_parent = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD^1"]));

        let captured = GitSource::revision(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            TipRule::Ref {
                name: "HEAD".to_string(),
            },
        )
        .capture()
        .await
        .expect("capture");
        // The net change the merge brought onto the mainline is the feature
        // branch's file; git show's combined diff of this clean merge is empty.
        let expected = "\
diff --git a/feature.txt b/feature.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/feature.txt
@@ -0,0 +1 @@
+feat";
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Ref {
                    name: "HEAD".to_string()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.base_revision, Some(first_parent));
        wince::assert_eq!(captured.base_tip_relative, true);
        wince::assert_eq!(captured.head_revision, Some(head));
    }

    #[tokio::test]
    async fn a_pinned_tip_that_no_longer_resolves_is_reported() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "add f"]);

        // A well-formed object name that names no commit, as a gc'd or
        // rewritten-away pin would: resolve_tip must reject it with the same
        // diagnostic a missing ref gives, not pass it down to git diff.
        let gone = RevisionId("0000000000000000000000000000000000000000".to_string());
        let result = GitSource::revision(
            repo.path(),
            BaseRuleset::empty(),
            TipRule::Pinned { revision: gone },
        )
        .capture()
        .await;
        wince::assert_eq!(
            result.expect_err("pin gone").to_string(),
            "could not capture diff: revision '0000000000000000000000000000000000000000' did not \
             resolve to a commit"
                .to_string()
        );
    }

    #[tokio::test]
    async fn a_worktree_source_captures_untracked_files_with_a_read_only_git() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "one\n").expect("write");
        git(repo.path(), home.path(), &["add", "tracked.txt"]);
        git(
            repo.path(),
            home.path(),
            &["commit", "-q", "-m", "add tracked"],
        );

        // A modification to a tracked file and a brand-new untracked file. The
        // untracked file is what forces the intent-to-add object write.
        std::fs::write(repo.path().join("tracked.txt"), "one\ntwo\n").expect("write");
        std::fs::write(repo.path().join("fresh.txt"), "new\n").expect("write");

        // Pin the base at the current commit before the capture, matching a
        // default `wiff new`.
        let head = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let base = BaseRuleset::pinned(&head);

        // Strip every write bit from .git to mimic a read-only mount, capture,
        // then restore so the tempdir can be cleaned up.
        let git_dir = repo.path().join(".git");
        set_readonly_recursively(&git_dir, true);
        let result = GitSource::worktree(repo.path(), base.clone())
            .capture()
            .await;
        set_readonly_recursively(&git_dir, false);
        let captured = result.expect("capture");

        let expected = "\
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+new
diff --git a/tracked.txt b/tracked.txt
index HASHES
--- a/tracked.txt
+++ b/tracked.txt
@@ -1 +1,2 @@
 one
+two";
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base,
                tip: TipRule::Worktree,
                // The worktree sits on main, recorded as the discovery hint.
                branch_hint: Some("refs/heads/main".to_string()),
            })
        );
        wince::assert_eq!(captured.base_revision, Some(head));
        // The base is pinned at the commit HEAD was on, not anchored to the
        // tip, so a later move would be reported.
        wince::assert_eq!(captured.base_tip_relative, false);
        wince::assert_eq!(captured.head_revision, None);
    }

    /// Toggle the read-only bit on every file and directory under `root`
    /// (including `root` itself), mimicking a read-only mount closely enough to
    /// reject object writes into `.git`.
    fn set_readonly_recursively(root: &Path, readonly: bool) {
        fn set(path: &Path, readonly: bool) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).expect("read_dir") {
                    set(&entry.expect("entry").path(), readonly);
                }
            }
            let mut perms = std::fs::metadata(path).expect("metadata").permissions();
            perms.set_readonly(readonly);
            std::fs::set_permissions(path, perms).expect("set_permissions");
        }
        set(root, readonly);
    }

    /// A repository with two commits on `main` and a third on a `feature`
    /// branch forked from the first, returning a repo handle and the commit ids
    /// (first, second, feature tip) for a resolver test to assert against.
    fn forked_repo(
        repo: &tempfile::TempDir,
        home: &tempfile::TempDir,
    ) -> (GitRepo, RevisionId, RevisionId, RevisionId) {
        let (r, h) = (repo.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        git(r, h, &["add", "f.txt"]);
        git(r, h, &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        std::fs::write(r.join("f.txt"), "a\nb\n").expect("write");
        git(r, h, &["commit", "-qa", "-m", "second"]);
        let second = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        git(r, h, &["checkout", "-q", "-b", "feature", first.as_str()]);
        std::fs::write(r.join("g.txt"), "c\n").expect("write");
        git(r, h, &["add", "g.txt"]);
        git(r, h, &["commit", "-q", "-m", "feature work"]);
        let feature = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        (GitRepo::new(r), first, second, feature)
    }

    #[tokio::test]
    async fn resolving_a_ref_yields_its_commit_and_an_unknown_ref_yields_none() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, _first, second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(
            source.resolve_ref("main").await.expect("resolve"),
            Some(second)
        );
        wince::assert_eq!(source.resolve_ref("nope").await.expect("resolve"), None);
    }

    #[tokio::test]
    async fn parent_of_a_commit_is_its_first_parent() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, first, second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(source.parent(&second).await.expect("parent"), Some(first));
    }

    #[tokio::test]
    async fn merge_base_of_a_branch_and_the_tip_is_their_fork_point() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, first, second, feature) = forked_repo(&repo, &home);
        wince::assert_eq!(
            source
                .merge_base(&second, &feature)
                .await
                .expect("merge-base"),
            Some(first)
        );
    }

    #[tokio::test]
    async fn resolving_a_merge_base_ruleset_finds_the_fork_point() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, first, _second, feature) = forked_repo(&repo, &home);
        let ruleset = parse_ruleset("merge-base(name(main))").expect("parse");
        let base = resolve_base(&ruleset, &feature, &source)
            .await
            .expect("resolve");
        wince::assert_eq!(
            base,
            Some(ResolvedBase {
                revision: first,
                tip_relative: false,
            })
        );
    }

    #[tokio::test]
    async fn resolving_empty_yields_the_repositorys_empty_tree() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, _first, _second, feature) = forked_repo(&repo, &home);
        let expected = git_out(
            repo.path(),
            home.path(),
            &["hash-object", "-t", "tree", "/dev/null"],
        );
        let ruleset = parse_ruleset("empty").expect("parse");
        let base = resolve_base(&ruleset, &feature, &source)
            .await
            .expect("resolve");
        wince::assert_eq!(
            base,
            Some(ResolvedBase {
                revision: RevisionId(expected),
                tip_relative: false,
            })
        );
    }

    #[tokio::test]
    async fn trunk_falls_back_to_a_local_main_when_there_is_no_remote() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // No remote is configured, so origin/HEAD is absent and trunk resolves
        // through the local main fallback to its commit.
        let (source, _first, second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(source.trunk().await.expect("trunk"), Some(second));
    }

    #[tokio::test]
    async fn upstream_resolves_the_tracking_branch() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // HEAD is on feature; point its upstream at main so @{upstream} resolves
        // to main's commit.
        let (source, _first, second, _feature) = forked_repo(&repo, &home);
        git(
            repo.path(),
            home.path(),
            &["branch", "--set-upstream-to=main", "feature"],
        );
        wince::assert_eq!(source.upstream().await.expect("upstream"), Some(second));
    }

    #[tokio::test]
    async fn a_ref_name_that_looks_like_an_option_resolves_to_none_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // --end-of-options keeps a dash-leading ref name from being read as a
        // git option: it is looked up as a (nonexistent) ref and falls through
        // to None rather than erroring on an unknown flag.
        let (source, _first, _second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(source.resolve_ref("--all").await.expect("resolve"), None);
    }

    #[tokio::test]
    async fn parent_of_a_root_commit_falls_back_to_the_empty_tree() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r, h) = (repo.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        git(r, h, &["add", "f.txt"]);
        git(r, h, &["commit", "-q", "-m", "root"]);
        let root = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        let empty = git_out(r, h, &["hash-object", "-t", "tree", "/dev/null"]);
        let source = GitRepo::new(r);
        wince::assert_eq!(
            source.parent(&root).await.expect("parent"),
            Some(RevisionId(empty))
        );
    }

    #[tokio::test]
    async fn merge_base_with_a_nonexistent_revision_is_an_error_not_a_fall_through() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, _first, _second, feature) = forked_repo(&repo, &home);
        let bogus = RevisionId("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());
        let error = source
            .merge_base(&bogus, &feature)
            .await
            .expect_err("merge-base fails");
        // git's exit 128 for a bad object is a genuine failure, reported rather
        // than swallowed as a fall-through. The trailing stderr detail varies by
        // git version, so only the stable head of the message is asserted.
        let message = error.to_string();
        let normalized = match message.split_once("): ") {
            Some((head, _)) => format!("{head}): STDERR"),
            None => message,
        };
        wince::assert_eq!(
            normalized,
            "could not capture diff: git merge-base failed (exit status: 128): STDERR".to_string()
        );
    }
}
