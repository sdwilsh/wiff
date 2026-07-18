#![allow(missing_docs)]

use std::path::Path;

use wiff_core::hash::SidebandHash;
use wiff_core::record::{
    Author, AuthorKind, Description, DescriptionRecord, DiffVersionRecord, FORMAT_VERSION,
    FileSummary, Record, RecordBody, SessionHeader, SourceKind, VersionNumber,
};
use wiff_core::review::DescriptionState;
use wiff_core::session::read_records;
use wiff_core::{
    CapturedDiff, DiffSource, LockWait, ProjectIdentity, ReviewState, create_session,
    set_description,
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
