#![allow(missing_docs)]

use std::path::Path;

use ulid::Ulid;
use wiff_core::hash::SidebandHash;
use wiff_core::record::{
    Author, AuthorKind, Description, DescriptionRecord, DiffVersionRecord, FORMAT_VERSION,
    FileSummary, ForgeUrl, Record, RecordBody, RevisionId, ScmSource, SessionHeader, SourceKind,
    TipRule, VersionNumber,
};
use wiff_core::review::DescriptionState;
use wiff_core::session::{SessionLog, read_records};
use wiff_core::{
    BaseRuleset, CapturedDiff, DiffSource, IfNeeded, LockWait, ProjectIdentity, ReviewState,
    ScmType, create_forge_session, create_session, reuse_or_create, set_description,
};
use wiff_diff::FileStatus;

const DIFF: &str = "\
diff --git a/added.txt b/added.txt
new file mode 100644
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+first
+second
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
";

fn identity() -> ProjectIdentity {
    ProjectIdentity {
        canonical: "demo".to_string(),
        repo_root: None,
        scm: None,
    }
}

fn stdin_diff() -> CapturedDiff {
    CapturedDiff {
        text: DIFF.to_string(),
        source: SourceKind::Stdin,
        base_revision: None,
        base_tip_relative: false,
        head_revision: None,
    }
}

#[tokio::test]
async fn captured_diff_is_its_own_source() {
    let original = stdin_diff();
    let captured = original.capture().await.unwrap();
    wince::assert_eq!(captured, original);
}

#[test]
fn create_session_writes_header_version_and_sideband() {
    let base = tempfile::tempdir().unwrap();
    let captured = stdin_diff();
    let log = create_session(
        base.path(),
        &identity(),
        Path::new("/work"),
        &captured,
        None,
    )
    .unwrap();

    // The log holds exactly the header and the v0 diff version, in order.
    let records = read_records(log.path()).unwrap();
    let bodies: Vec<(u64, RecordBody)> = records
        .iter()
        .map(|Record { seq, body, .. }| (seq.get(), body.clone()))
        .collect();
    let expected = vec![
        (
            0,
            RecordBody::Session(SessionHeader {
                ulid: log.ulid(),
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: None,
                cwd: "/work".to_string(),
                source: SourceKind::Stdin,
                forge: None,
            }),
        ),
        (
            1,
            RecordBody::DiffVersion(DiffVersionRecord {
                number: VersionNumber(0),
                diff_hash: SidebandHash::of(DIFF.as_bytes()),
                base_revision: None,
                base_tip_relative: false,
                head_revision: None,
                files: vec![
                    FileSummary {
                        old_path: "added.txt".to_string(),
                        new_path: "added.txt".to_string(),
                        status: FileStatus::Added,
                        hunk_count: 1,
                    },
                    FileSummary {
                        old_path: "src/main.rs".to_string(),
                        new_path: "src/main.rs".to_string(),
                        status: FileStatus::Modified,
                        hunk_count: 1,
                    },
                ],
            }),
        ),
    ];
    wince::assert_eq!(bodies, expected);

    // The raw diff lands in the sideband v0.diff, byte for byte.
    let sideband = log.sideband_dir().join("v0.diff");
    let written = std::fs::read_to_string(&sideband).unwrap();
    wince::assert_eq!(written, DIFF.to_string());
}

#[test]
fn create_forge_session_binds_the_pull_request_under_the_chosen_ulid() {
    let base = tempfile::tempdir().unwrap();
    let forge = ForgeUrl::parse("https://github.com/octo/demo/pull/7").unwrap();
    let session = Ulid::new();
    let captured = CapturedDiff {
        text: DIFF.to_string(),
        source: SourceKind::Forge,
        base_revision: Some(RevisionId("base".to_string())),
        base_tip_relative: false,
        head_revision: Some(RevisionId("head".to_string())),
    };
    let log = create_forge_session(
        base.path(),
        &identity(),
        Path::new("/work"),
        forge.clone(),
        session,
        &captured,
        Vec::new(),
    )
    .unwrap();

    // The header takes the chosen ULID and records the bound pull request; the
    // v0 version keeps the fetched base and head the diff was captured between.
    wince::assert_eq!(log.ulid(), session);
    let records = read_records(log.path()).unwrap();
    let bodies: Vec<(u64, RecordBody)> = records
        .iter()
        .map(|Record { seq, body, .. }| (seq.get(), body.clone()))
        .collect();
    let expected = vec![
        (
            0,
            RecordBody::Session(SessionHeader {
                ulid: session,
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: None,
                cwd: "/work".to_string(),
                source: SourceKind::Forge,
                forge: Some(forge),
            }),
        ),
        (
            1,
            RecordBody::DiffVersion(DiffVersionRecord {
                number: VersionNumber(0),
                diff_hash: SidebandHash::of(DIFF.as_bytes()),
                base_revision: Some(RevisionId("base".to_string())),
                base_tip_relative: false,
                head_revision: Some(RevisionId("head".to_string())),
                files: vec![
                    FileSummary {
                        old_path: "added.txt".to_string(),
                        new_path: "added.txt".to_string(),
                        status: FileStatus::Added,
                        hunk_count: 1,
                    },
                    FileSummary {
                        old_path: "src/main.rs".to_string(),
                        new_path: "src/main.rs".to_string(),
                        status: FileStatus::Modified,
                        hunk_count: 1,
                    },
                ],
            }),
        ),
    ];
    wince::assert_eq!(bodies, expected);
}

#[test]
fn create_forge_session_rejects_an_id_that_already_names_a_session() {
    let base = tempfile::tempdir().unwrap();
    let forge = ForgeUrl::parse("https://github.com/octo/demo/pull/7").unwrap();
    let session = Ulid::new();
    let captured = CapturedDiff {
        text: DIFF.to_string(),
        source: SourceKind::Forge,
        base_revision: Some(RevisionId("base".to_string())),
        base_tip_relative: false,
        head_revision: Some(RevisionId("head".to_string())),
    };
    let make = || {
        create_forge_session(
            base.path(),
            &identity(),
            Path::new("/work"),
            forge.clone(),
            session,
            &captured,
            Vec::new(),
        )
    };
    make().expect("first import succeeds");

    // A second import under the same id reports the collision as a distinct,
    // matchable error rather than an opaque io failure.
    let error = make().expect_err("the id is already taken");
    wince::assert_eq!(
        error.to_string(),
        format!("session {session} already exists")
    );
    assert!(matches!(error, wiff_core::error::Error::SessionExists(id) if id == session));
}

#[test]
fn create_session_writes_an_initial_description_after_the_diff() {
    let base = tempfile::tempdir().unwrap();
    let captured = stdin_diff();
    let author = Author {
        name: "wez".to_string(),
        kind: AuthorKind::Human,
    };
    let description = Description {
        title: "Tidy the parser".to_string(),
        body: "Split the lexer out.".to_string(),
    };
    let log = create_session(
        base.path(),
        &identity(),
        Path::new("/work"),
        &captured,
        Some((author.clone(), description.clone())),
    )
    .unwrap();

    // The log holds the header, the v0 diff version, and the description
    // revision that trails them, in order.
    let records = read_records(log.path()).unwrap();
    let bodies: Vec<(u64, RecordBody)> = records
        .iter()
        .map(|Record { seq, body, .. }| (seq.get(), body.clone()))
        .collect();
    let expected = vec![
        (
            0,
            RecordBody::Session(SessionHeader {
                ulid: log.ulid(),
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: None,
                cwd: "/work".to_string(),
                source: SourceKind::Stdin,
                forge: None,
            }),
        ),
        (
            1,
            RecordBody::DiffVersion(DiffVersionRecord {
                number: VersionNumber(0),
                diff_hash: SidebandHash::of(DIFF.as_bytes()),
                base_revision: None,
                base_tip_relative: false,
                head_revision: None,
                files: vec![
                    FileSummary {
                        old_path: "added.txt".to_string(),
                        new_path: "added.txt".to_string(),
                        status: FileStatus::Added,
                        hunk_count: 1,
                    },
                    FileSummary {
                        old_path: "src/main.rs".to_string(),
                        new_path: "src/main.rs".to_string(),
                        status: FileStatus::Modified,
                        hunk_count: 1,
                    },
                ],
            }),
        ),
        (
            2,
            RecordBody::Description(DescriptionRecord {
                author: author.clone(),
                authored_at: None,
                origin: None,
                synced_marker: None,
                description: description.clone(),
            }),
        ),
    ];
    wince::assert_eq!(bodies, expected);

    // It folds into the review's whole current state.
    let at = records.last().unwrap().at;
    let state = ReviewState::load(log.path()).unwrap();
    wince::assert_eq!(
        state,
        ReviewState {
            session: SessionHeader {
                ulid: log.ulid(),
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: None,
                cwd: "/work".to_string(),
                source: SourceKind::Stdin,
                forge: None,
            },
            versions: vec![DiffVersionRecord {
                number: VersionNumber(0),
                diff_hash: SidebandHash::of(DIFF.as_bytes()),
                base_revision: None,
                base_tip_relative: false,
                head_revision: None,
                files: vec![
                    FileSummary {
                        old_path: "added.txt".to_string(),
                        new_path: "added.txt".to_string(),
                        status: FileStatus::Added,
                        hunk_count: 1,
                    },
                    FileSummary {
                        old_path: "src/main.rs".to_string(),
                        new_path: "src/main.rs".to_string(),
                        status: FileStatus::Modified,
                        hunk_count: 1,
                    },
                ],
            }],
            description: Some(DescriptionState {
                content: description,
                author,
                updated_at: at,
                origin: None,
                synced_marker: None,
            }),
            comments: Vec::new(),
            verdicts: Vec::new(),
            pushed_verdicts: Vec::new(),
        }
    );
}

#[test]
fn setting_the_description_after_creation_replaces_the_earlier_one() {
    let base = tempfile::tempdir().unwrap();
    let captured = stdin_diff();
    let mut log = create_session(
        base.path(),
        &identity(),
        Path::new("/work"),
        &captured,
        None,
    )
    .unwrap();
    let dev = Author {
        name: "dev".to_string(),
        kind: AuthorKind::Agent,
    };
    set_description(
        &mut log,
        Description {
            title: "First".to_string(),
            body: String::new(),
        },
        dev.clone(),
        LockWait::Block,
    )
    .unwrap();
    set_description(
        &mut log,
        Description {
            title: "Second".to_string(),
            body: "with a body".to_string(),
        },
        dev.clone(),
        LockWait::Block,
    )
    .unwrap();

    let state = ReviewState::load(log.path()).unwrap();
    let at = read_records(log.path()).unwrap().last().unwrap().at;
    wince::assert_eq!(
        state.description,
        Some(DescriptionState {
            content: Description {
                title: "Second".to_string(),
                body: "with a body".to_string(),
            },
            author: dev,
            updated_at: at,
            origin: None,
            synced_marker: None,
        })
    );
}

const DIFF_A: &str = "diff --git a/f.txt b/f.txt\n\
     --- a/f.txt\n\
     +++ b/f.txt\n\
     @@ -1 +1 @@\n\
     -old\n\
     +new\n";
const DIFF_B: &str = "diff --git a/f.txt b/f.txt\n\
     --- a/f.txt\n\
     +++ b/f.txt\n\
     @@ -1 +1 @@\n\
     -old\n\
     +newer\n";

fn actor() -> Author {
    Author {
        name: "wez".to_string(),
        kind: AuthorKind::Human,
    }
}

/// A working-tree capture of `text` on the `topic` branch.
fn topic_capture(text: &str) -> CapturedDiff {
    CapturedDiff {
        text: text.to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("merge-base(trunk)"),
            tip: TipRule::WorkingCopy,
            branch_hint: Some("refs/heads/topic".to_string()),
        }),
        base_revision: None,
        base_tip_relative: false,
        head_revision: None,
    }
}

/// A capture of the same file on a different branch, a distinct range.
fn other_capture(text: &str) -> CapturedDiff {
    CapturedDiff {
        text: text.to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("merge-base(trunk)"),
            tip: TipRule::Ref {
                name: "refs/heads/other".to_string(),
            },
            branch_hint: None,
        }),
        base_revision: None,
        base_tip_relative: false,
        head_revision: None,
    }
}

fn render(outcome: &IfNeeded) -> String {
    match outcome {
        IfNeeded::Created(log) => format!("created {}", log.ulid()),
        IfNeeded::Unchanged(log) => format!("unchanged {}", log.ulid()),
        IfNeeded::Refreshed(log, o) => format!("refreshed {} v{}", log.ulid(), o.version),
        IfNeeded::NothingToReview => "nothing to review".to_string(),
    }
}

// Re-running reuse_or_create against the same range reuses one session: the
// first run creates it, an identical diff leaves it untouched, a moved working
// copy refreshes it in place, and a different range starts a session of its own.
#[test]
fn reuse_or_create_creates_then_reuses_then_refreshes_one_session() {
    let data = tempfile::tempdir().expect("data tempdir");
    let base = data.path();
    let id = identity();
    let cwd = Path::new("/work");

    let first = reuse_or_create(base, &id, cwd, &topic_capture(DIFF_A), actor(), None)
        .expect("first run creates");
    let (topic_ulid, topic_path) = match &first {
        IfNeeded::Created(log) => (log.ulid(), log.path().to_path_buf()),
        other => panic!("expected a create, got {}", render(other)),
    };
    let reused = reuse_or_create(base, &id, cwd, &topic_capture(DIFF_A), actor(), None)
        .expect("identical diff reuses");
    let refreshed = reuse_or_create(base, &id, cwd, &topic_capture(DIFF_B), actor(), None)
        .expect("moved working copy refreshes");
    let other = reuse_or_create(base, &id, cwd, &other_capture(DIFF_A), actor(), None)
        .expect("a different range creates");
    let other_ulid = match &other {
        IfNeeded::Created(log) => log.ulid(),
        other => panic!("expected a create, got {}", render(other)),
    };

    let state = ReviewState::load(&topic_path).expect("load topic session");
    let latest = state.versions.last().expect("a captured version").number;
    let latest_diff = SessionLog::open(&topic_path)
        .expect("open topic session")
        .read_diff(latest)
        .expect("read latest diff");
    let report = format!(
        "first: {}\n\
         reuse: {}\n\
         refresh: {}\n\
         other: {} (distinct from topic: {})\n\
         topic versions: {}\n\
         topic latest diff:\n{latest_diff}",
        render(&first),
        render(&reused),
        render(&refreshed),
        render(&other),
        other_ulid != topic_ulid,
        state.versions.len(),
    )
    .replace(&topic_ulid.to_string(), "TOPIC")
    .replace(&other_ulid.to_string(), "OTHER");

    wince::snapshot_display!(
        report,
        "first: created TOPIC\n\
         reuse: unchanged TOPIC\n\
         refresh: refreshed TOPIC v1\n\
         other: created OTHER (distinct from topic: true)\n\
         topic versions: 2\n\
         topic latest diff:\n\
         diff --git a/f.txt b/f.txt\n\
         --- a/f.txt\n\
         +++ b/f.txt\n\
         @@ -1 +1 @@\n\
         -old\n\
         +newer\n"
    );
}

// An empty capture with no session is a dead end, but once a session exists the
// same range reuses it: a reverted working copy refreshes to the empty diff
// rather than starting over.
#[test]
fn reuse_or_create_reports_nothing_to_review_until_a_session_exists() {
    let data = tempfile::tempdir().expect("data tempdir");
    let base = data.path();
    let id = identity();
    let cwd = Path::new("/work");

    let empty_first = reuse_or_create(base, &id, cwd, &topic_capture(""), actor(), None)
        .expect("an empty capture is reported, not an error");
    let created = reuse_or_create(base, &id, cwd, &topic_capture(DIFF_A), actor(), None)
        .expect("a non-empty capture opens the session");
    let ulid = match &created {
        IfNeeded::Created(log) => log.ulid(),
        other => panic!("expected a create, got {}", render(other)),
    };
    let empty_again = reuse_or_create(base, &id, cwd, &topic_capture(""), actor(), None)
        .expect("an empty capture reuses the existing session");

    let report = format!(
        "empty first: {}\n\
         created: {}\n\
         empty again: {}",
        render(&empty_first),
        render(&created),
        render(&empty_again),
    )
    .replace(&ulid.to_string(), "TOPIC");

    wince::snapshot_display!(
        report,
        "empty first: nothing to review\n\
         created: created TOPIC\n\
         empty again: refreshed TOPIC v1"
    );
}
