//! `wiff forge`: mirror a pull request into a session and publish the review
//! back to its host. The credential overrides and the step that turns a pull
//! request's host into a connected adapter are common to every subcommand, so
//! they live here; each subcommand owns the round-trips it drives through that
//! adapter.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use ulid::Ulid;
use wiff_config::Config;
use wiff_core::record::{Author, ForgeUrl, TipRule};
use wiff_core::session::{data_dir, session_bound_to, session_file};
use wiff_core::source::GitRepo;
use wiff_core::{BaseRuleset, GitSource, ProjectIdentity, ScmRepo, SessionLog};
use wiff_forge::{
    FetchedPullRequest, Forge, GithubForge, ImportRequest, TokenOverride, import_pull_request,
    resolve_token, resync_pull_request, select_pull_request_remote,
};

use super::resolve_author;
use crate::tui;

/// Arguments for `wiff forge`.
#[derive(Debug, Args)]
pub struct ForgeArgs {
    #[command(flatten)]
    token: ForgeToken,
    #[command(subcommand)]
    command: ForgeCommand,
}

impl ForgeArgs {
    /// Dispatch the selected `wiff forge` subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        let cli = self.token.overrides();
        match self.command {
            ForgeCommand::Pull(args) => args.run(&cli).await,
            ForgeCommand::Push => {
                bail!("wiff forge push is not implemented yet")
            }
        }
    }
}

/// The credentials for the forge, given directly or as a file to read the
/// token from. The two are mutually exclusive.
#[derive(Debug, Args)]
struct ForgeToken {
    /// Read the forge token from this file, using its trimmed contents.
    #[arg(long)]
    forge_token_file: Option<PathBuf>,
    /// The forge token, given directly.
    #[arg(long, conflicts_with = "forge_token_file")]
    forge_token: Option<String>,
}

impl ForgeToken {
    /// The command-line token overrides these arguments express.
    fn overrides(&self) -> TokenOverride {
        TokenOverride {
            token_file: self.forge_token_file.clone(),
            token: self.forge_token.clone(),
        }
    }
}

/// The `wiff forge` subcommands.
#[derive(Debug, Subcommand)]
enum ForgeCommand {
    /// Fetch a pull request into a session and open it.
    Pull(PullArgs),
    /// Publish the local review to the bound pull request.
    Push,
}

/// Arguments for `wiff forge pull`.
#[derive(Debug, Args)]
struct PullArgs {
    /// The pull request to mirror: a number read against the repository's forge
    /// remote, or a full pull-request URL such as
    /// `https://github.com/wezterm/wezterm/pull/6185`.
    #[arg(value_name = "NUMBER|URL")]
    pr: String,
    /// Review the pull request in a fresh session even when one is already bound
    /// to it, instead of re-syncing that session.
    #[arg(long)]
    new_session: bool,
    /// The display name to attribute a re-sync's rebased comments to.
    #[arg(long)]
    author: Option<String>,
    /// Attribute a re-sync's rebased comments to an agent rather than a human.
    #[arg(long)]
    agent: bool,
}

impl PullArgs {
    /// Mirror the named pull request into a session and open it.
    async fn run(self, cli: &TokenOverride) -> anyhow::Result<()> {
        let config = Config::load()?;
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let (url, forge) = resolve_target(&self.pr, &cwd, &config, cli).await?;
        let base = data_dir()?;
        let author = resolve_author(self.agent, self.author)?;
        let session =
            mirror_pull_request(forge.as_ref(), &url, &cwd, &base, author, self.new_session)
                .await?;
        tui::open(&session, &config, false)
    }
}

/// The pull request `wiff forge pull` names: a bare number resolved against the
/// repository's forge remote, or a full URL that names the forge outright.
enum PullTarget {
    /// A pull-request number, meaningful only against a repository's remote.
    Number(u64),
    /// A full pull-request URL, standing on its own without a repository.
    Url(ForgeUrl),
}

/// Tell a full pull-request URL from a bare number. The command line is the only
/// place that reads a bare id as a number; below it the id is an opaque string,
/// and this is the point to revisit for a forge that names its pull requests some
/// other way.
fn classify_target(pr: &str) -> anyhow::Result<PullTarget> {
    // A "://" marks input the user meant as a URL: parse it and report why an
    // ill-formed one is rejected, rather than fall through and misreport it as
    // not a URL at all.
    if pr.contains("://") {
        return Ok(PullTarget::Url(ForgeUrl::parse(pr)?));
    }
    match pr.parse::<u64>() {
        Ok(number) => Ok(PullTarget::Number(number)),
        Err(_) => bail!("{pr} is neither a pull-request number nor a full pull-request URL"),
    }
}

/// Resolve the user's pull-request argument for fetching: a full URL names the
/// host itself, while a bare number is meaningful only against a repository and
/// is read through the configured forge remote its clone URLs name. Returns the
/// canonical URL and an adapter connected to its host.
async fn resolve_target(
    pr: &str,
    cwd: &Path,
    config: &Config,
    cli: &TokenOverride,
) -> anyhow::Result<(ForgeUrl, Box<dyn Forge>)> {
    match classify_target(pr)? {
        PullTarget::Url(url) => {
            let forge = connect_forge(config, &url.host(), cli)?;
            Ok((url, forge))
        }
        PullTarget::Number(number) => {
            let identity = ProjectIdentity::for_dir(cwd).map_err(|_| {
                anyhow::anyhow!(
                    "a pull-request number is read against the repository's forge remote, but \
                     the current directory is not inside a repository; give a full pull-request \
                     URL instead"
                )
            })?;
            let root = identity
                .repo_root
                .expect("for_dir yields a repo root on success");
            // A number resolves through git remotes. A colocated jj checkout
            // keeps a usable .git beside its .jj, so read git whenever a .git is
            // present rather than trusting the discovered scm, which names jj for
            // that layout.
            if !root.join(".git").exists() {
                bail!(
                    "a pull-request number is read against a git remote, but {} is not a git \
                     repository; give a full pull-request URL instead",
                    root.display()
                );
            }
            let remotes = GitRepo::new(root).remotes().await?;
            let (remote, host) = select_pull_request_remote(&remotes, &config.forge)?;
            let forge = connect_forge(config, &host, cli)?;
            let url = forge.pull_request_url(&remote.url, &number.to_string())?;
            Ok((url, forge))
        }
    }
}

/// Mirror a fetched pull request into a session bound to the current repo,
/// returning the session file to open. A session already bound to the pull
/// request is re-synced in place, attributing the rebased comments to `author`,
/// unless `new_session` forces a fresh review alongside it.
async fn mirror_pull_request(
    forge: &dyn Forge,
    url: &ForgeUrl,
    cwd: &Path,
    base: &Path,
    author: Author,
    new_session: bool,
) -> anyhow::Result<PathBuf> {
    let identity = ProjectIdentity::for_dir(cwd).map_err(|_| {
        anyhow::anyhow!(
            "wiff forge pull mirrors a pull request into the current repository, but the current \
             directory is not inside one; a repo-less pull is not supported yet"
        )
    })?;
    let root = identity
        .repo_root
        .clone()
        .expect("for_dir yields a repo root on success");
    // A colocated jj checkout keeps a usable .git beside its .jj, so read git
    // whenever a .git is present rather than trusting the discovered scm.
    if !root.join(".git").exists() {
        bail!(
            "wiff forge pull does not yet support mirroring into a non-git repository, but {} is \
             not a git repository",
            root.display()
        );
    }

    let fetched = forge.fetch(url).await?;
    let existing = if new_session {
        None
    } else {
        session_bound_to(base, &identity.canonical, url)?
    };
    match existing {
        Some(path) => {
            let mut log = SessionLog::open(&path)?;
            let source = prepare_source(&root, &fetched, log.ulid()).await?;
            resync_pull_request(&mut log, &source, &fetched, author).await?;
            Ok(path)
        }
        None => {
            let session = Ulid::new();
            let source = prepare_source(&root, &fetched, session).await?;
            let request = ImportRequest {
                session,
                base,
                identity: &identity,
                cwd,
            };
            import_pull_request(&source, &fetched, &request).await?;
            Ok(session_file(base, &identity.canonical, session))
        }
    }
}

/// Fetch and pin the pull request's head and target-branch commits in the
/// repository at `root` under `session`, then build a source that diffs the head
/// against its merge-base with the target branch, the range showing the pull
/// request's own changes and not commits the target has moved on to. The target
/// tip is fetched rather than assumed present: once the target advances past the
/// fork point it is no longer reachable from the head.
async fn prepare_source(
    root: &Path,
    fetched: &FetchedPullRequest,
    session: Ulid,
) -> anyhow::Result<GitSource> {
    let repo = GitRepo::new(root);
    let head = repo.fetch_pinned(&fetched.head, session).await?;
    repo.fetch_base(&fetched.base, session).await?;
    let base = BaseRuleset::new(format!(
        "merge-base(name({}))",
        fetched.base_commit().as_str()
    ));
    Ok(GitSource::revision(
        root.to_path_buf(),
        base,
        TipRule::Pinned { revision: head },
    ))
}

/// Build the forge adapter for `host`: look it up in the effective forge table,
/// resolve the token from the command-line overrides or the host's configured
/// variables, and construct the adapter the host's provider names.
pub(crate) fn connect_forge(
    config: &Config,
    host: &str,
    cli: &TokenOverride,
) -> anyhow::Result<Box<dyn Forge>> {
    let row = config.forge.host(host).with_context(|| {
        format!(
            "no forge is configured for {host}; add a [forge.\"{host}\"] entry naming its provider"
        )
    })?;
    let provider = row
        .provider
        .as_deref()
        .with_context(|| format!("the forge entry for {host} names no provider"))?;
    // Match the provider before resolving the token: an adapter wiff cannot
    // build should say so rather than first demand a credential it will not use.
    match provider {
        "github" => {
            let token = resolve_token(&row, cli, |name| std::env::var(name).ok())?;
            Ok(Box::new(GithubForge::new(&token, row.api_base.as_deref())?))
        }
        "forgejo" => bail!("the forgejo forge adapter is not available yet"),
        other => bail!("host {host} names an unknown forge provider {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use time::OffsetDateTime;
    use wiff_core::record::{
        AuthorKind, Description, ExternalKind, ExternalRef, ForgeId, RevisionId,
    };
    use wiff_core::review::ReviewState;
    use wiff_core::source::FetchSource;
    use wiff_forge::{
        FetchedDescription, ForgeHost, ForgeTable, NewPullRequest, OutgoingComment, OutgoingReview,
        SubmittedReview,
    };

    use super::*;
    use crate::testutil::{git, git_out};

    /// A config whose forge table is `table` and whose other fields are the
    /// defaults, for exercising `connect_forge` without a config file.
    fn config_with(table: ForgeTable) -> Config {
        Config {
            forge: table,
            ..Config::default()
        }
    }

    /// The token override that hands the token over directly, so the resolution
    /// never consults the environment.
    fn direct_token(token: &str) -> TokenOverride {
        TokenOverride {
            token_file: None,
            token: Some(token.to_string()),
        }
    }

    // Building the octocrab-backed adapter spawns a background service, so it
    // needs a tokio runtime even though no request is made.
    #[tokio::test]
    async fn a_github_host_builds_an_adapter() {
        let config = config_with(ForgeTable::default());
        connect_forge(&config, "github.com", &direct_token("t")).expect("github adapter");
    }

    #[test]
    fn an_unconfigured_host_is_reported_with_its_name() {
        let config = config_with(ForgeTable::default());
        let error = connect_forge(&config, "git.example.org", &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "no forge is configured for git.example.org; add a \
             [forge.\"git.example.org\"] entry naming its provider"
        );
    }

    #[test]
    fn a_forgejo_host_reports_the_adapter_is_unavailable() {
        let config = config_with(ForgeTable::default());
        // No token is supplied: an adapter wiff cannot build is reported before
        // any credential is demanded.
        let error = connect_forge(&config, "codeberg.org", &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "the forgejo forge adapter is not available yet"
        );
    }

    #[test]
    fn an_unknown_provider_is_reported_with_the_host() {
        let table = ForgeTable::from([(
            "git.example.org".to_string(),
            ForgeHost {
                provider: Some("bitbucket".to_string()),
                ..ForgeHost::default()
            },
        )]);
        let config = config_with(table);
        let error = connect_forge(&config, "git.example.org", &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "host git.example.org names an unknown forge provider \"bitbucket\""
        );
    }

    /// Render a classification outcome for a full-output assertion.
    fn classified(pr: &str) -> String {
        match classify_target(pr) {
            Ok(PullTarget::Number(number)) => format!("number {number}"),
            Ok(PullTarget::Url(url)) => format!("url {}", url.as_str()),
            Err(error) => format!("error: {error}"),
        }
    }

    #[test]
    fn classify_target_tells_a_url_from_a_number_and_rejects_the_rest() {
        let cases: Vec<String> = [
            "7",
            "https://github.com/octo/demo/pull/7",
            "http://",
            "octo/demo#7",
            "",
        ]
        .into_iter()
        .map(|pr| format!("{pr:?} -> {}", classified(pr)))
        .collect();
        wince::assert_eq!(
            cases.join("\n"),
            "\"7\" -> number 7\n\
             \"https://github.com/octo/demo/pull/7\" -> url https://github.com/octo/demo/pull/7\n\
             \"http://\" -> error: http:// is not an absolute http(s) URL with a host\n\
             \"octo/demo#7\" -> error: octo/demo#7 is neither a pull-request number nor a full \
             pull-request URL\n\
             \"\" -> error:  is neither a pull-request number nor a full pull-request URL"
        );
    }

    // Resolving a full URL neither reads a repository nor reaches the network,
    // but building the github adapter spawns a background service that needs a
    // tokio runtime. The cwd is unread on this path.
    #[tokio::test]
    async fn a_url_target_resolves_to_the_pull_request_url() {
        let config = config_with(ForgeTable::default());
        let (url, _forge) = resolve_target(
            "https://github.com/octo/demo/pull/7",
            Path::new("."),
            &config,
            &direct_token("t"),
        )
        .await
        .expect("resolve");
        wince::assert_eq!(
            url.as_str().to_string(),
            "https://github.com/octo/demo/pull/7".to_string()
        );
    }

    #[tokio::test]
    async fn a_number_target_resolves_against_the_repository_forge_remote() {
        let repo = tempfile::tempdir().expect("repo tempdir");
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/octo/demo.git",
            ],
        );
        let config = config_with(ForgeTable::default());
        let (url, _forge) = resolve_target("7", repo.path(), &config, &direct_token("t"))
            .await
            .expect("resolve");
        wince::assert_eq!(
            url.as_str().to_string(),
            "https://github.com/octo/demo/pull/7".to_string()
        );
    }

    #[tokio::test]
    async fn a_number_target_outside_a_repository_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_with(ForgeTable::default());
        let error = resolve_target("7", dir.path(), &config, &direct_token("t"))
            .await
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "a pull-request number is read against the repository's forge remote, but the \
             current directory is not inside a repository; give a full pull-request URL instead"
        );
    }

    // A discovered root that is not a git checkout (here a bare `.hg` marker)
    // cannot answer a number, since resolution reads git remotes.
    #[tokio::test]
    async fn a_number_target_in_a_non_git_repository_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".hg")).expect("mark hg root");
        let config = config_with(ForgeTable::default());
        let error = resolve_target("7", dir.path(), &config, &direct_token("t"))
            .await
            .map(|_| ())
            .unwrap_err();
        let message = error
            .to_string()
            .replace(&dir.path().display().to_string(), "TMPDIR");
        wince::assert_eq!(
            message,
            "a pull-request number is read against a git remote, but TMPDIR is not a git \
             repository; give a full pull-request URL instead"
        );
    }

    /// A forge whose `fetch` hands back a fixed pull request and whose write
    /// operations are never reached, for driving the mirror orchestration
    /// without a network.
    struct FetchForge(FetchedPullRequest);

    #[async_trait]
    impl Forge for FetchForge {
        async fn fetch(&self, _pr: &ForgeUrl) -> anyhow::Result<FetchedPullRequest> {
            Ok(self.0.clone())
        }

        async fn submit_review(
            &self,
            _pr: &ForgeUrl,
            _review: &OutgoingReview,
        ) -> anyhow::Result<SubmittedReview> {
            unreachable!("mirroring a pull request does not submit a review")
        }

        async fn post_comment(
            &self,
            _pr: &ForgeUrl,
            _comment: &OutgoingComment,
        ) -> anyhow::Result<ExternalRef> {
            unreachable!("mirroring a pull request does not post comments")
        }

        async fn edit_comment(&self, _at: &ExternalRef, _body: &str) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not edit comments")
        }

        async fn set_resolved(&self, _at: &ExternalRef, _resolved: bool) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not resolve comments")
        }

        async fn set_description(
            &self,
            _pr: &ForgeUrl,
            _description: &Description,
        ) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not set a description")
        }

        async fn create_pull_request(&self, _req: &NewPullRequest) -> anyhow::Result<ForgeUrl> {
            unreachable!("mirroring a pull request does not open one")
        }

        fn pull_request_url(&self, _remote_url: &str, _id: &str) -> anyhow::Result<ForgeUrl> {
            unreachable!("mirroring a pull request does not resolve one by id")
        }
    }

    /// A human author with `name`.
    fn human(name: &str) -> Author {
        Author {
            name: name.to_string(),
            kind: AuthorKind::Human,
        }
    }

    /// A github external ref of `kind` with identifier `id`.
    fn github_ref(kind: ExternalKind, id: &str) -> ExternalRef {
        ExternalRef {
            forge: ForgeId {
                provider: "github".to_string(),
                host: "github.com".to_string(),
            },
            kind,
            id: id.to_string(),
            url: None,
        }
    }

    /// A pull request whose head is the `pr` branch of the git repository at
    /// `origin` and whose target is that repository's `main` at `base_commit`,
    /// with a description and no comments or reviews.
    fn fetched_pull_request(
        origin: &Path,
        base_commit: &str,
        head_commit: &str,
    ) -> FetchedPullRequest {
        FetchedPullRequest {
            url: ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url"),
            description: FetchedDescription {
                origin: github_ref(ExternalKind::Description, "7"),
                author: human("octo"),
                content: Description {
                    title: "PR title".to_string(),
                    body: "PR body".to_string(),
                },
                authored_at: OffsetDateTime::UNIX_EPOCH,
            },
            head: FetchSource::Git {
                url: origin.display().to_string(),
                git_ref: "refs/heads/pr".to_string(),
                commit: RevisionId(head_commit.to_string()),
            },
            base: FetchSource::Git {
                url: origin.display().to_string(),
                git_ref: "refs/heads/main".to_string(),
                commit: RevisionId(base_commit.to_string()),
            },
            comments: Vec::new(),
            reviews: Vec::new(),
        }
    }

    // A first pull imports a bound session from the fetched range and mirrors its
    // description; a second pull re-syncs that same session rather than
    // duplicating it, while --new-session deliberately forks a second review of
    // the same pull request.
    #[tokio::test]
    async fn mirroring_imports_then_resyncs_unless_a_new_session_is_forced() {
        let origin = tempfile::tempdir().expect("origin tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        let data = tempfile::tempdir().expect("data tempdir");

        git(origin.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(origin.path().join("f.txt"), "base\n").expect("write base");
        git(origin.path(), &["add", "f.txt"]);
        git(origin.path(), &["commit", "-q", "-m", "base"]);
        git(origin.path(), &["checkout", "-q", "-b", "pr"]);
        std::fs::write(origin.path().join("f.txt"), "base\nchange\n").expect("write change");
        git(origin.path(), &["commit", "-qa", "-m", "pr work"]);
        let head_commit = git_out(origin.path(), &["rev-parse", "HEAD"]);
        git(origin.path(), &["checkout", "-q", "main"]);

        git(
            work.path(),
            &["clone", "-q", &origin.path().display().to_string(), "."],
        );

        // Advance main past the fork point after the clone, so the target tip
        // the pull request names is absent from the working repo and must be
        // fetched to compute the merge-base.
        std::fs::write(origin.path().join("other.txt"), "later\n").expect("write other");
        git(origin.path(), &["add", "other.txt"]);
        git(origin.path(), &["commit", "-q", "-m", "advance main"]);
        let base_commit = git_out(origin.path(), &["rev-parse", "HEAD"]);

        let forge = FetchForge(fetched_pull_request(
            origin.path(),
            &base_commit,
            &head_commit,
        ));
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

        let first =
            mirror_pull_request(&forge, &url, work.path(), data.path(), human("wez"), false)
                .await
                .expect("first pull imports");
        let resynced =
            mirror_pull_request(&forge, &url, work.path(), data.path(), human("wez"), false)
                .await
                .expect("second pull resyncs");
        let forked =
            mirror_pull_request(&forge, &url, work.path(), data.path(), human("wez"), true)
                .await
                .expect("forced fresh session");

        let bucket = first.parent().expect("session bucket");
        let sessions = std::fs::read_dir(bucket)
            .expect("read bucket")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("entry")
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
            })
            .count();
        let state = ReviewState::load(&first).expect("load imported session");
        let description = state
            .description
            .as_ref()
            .map(|d| format!("{} / {}", d.content.title, d.content.body))
            .unwrap_or_else(|| "(none)".to_string());
        let summary = format!(
            "resync reuses the bound session: {}\n\
             new-session forks a second: {}\n\
             sessions in bucket: {sessions}\n\
             bound to: {}\n\
             description: {description}\n\
             diff versions: {}",
            resynced == first,
            forked != first && forked.parent() == Some(bucket),
            state
                .session
                .forge
                .as_ref()
                .map(ForgeUrl::as_str)
                .unwrap_or("(none)"),
            state.versions.len(),
        );
        wince::assert_eq!(
            summary,
            "resync reuses the bound session: true\n\
             new-session forks a second: true\n\
             sessions in bucket: 2\n\
             bound to: https://github.com/octo/demo/pull/7\n\
             description: PR title / PR body\n\
             diff versions: 1"
                .to_string()
        );
    }
}
