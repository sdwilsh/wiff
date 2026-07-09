#![allow(missing_docs)]

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentDelete, CommentEdit, CommentReanchor, CommentRecord,
    CommentResolve, CommentTarget, Confidence, DiffVersionRecord, FORMAT_VERSION, FileSummary,
    Record, RecordBody, SessionHeader, SourceKind,
};
use wiff_core::review::{CommentState, ReviewState, fold};
use wiff_core::{Error, SidebandHash};
use wiff_diff::{FileStatus, LineNo, Side};

fn comment_a() -> Ulid {
    Ulid::from_string("00000000000000000000000000").unwrap()
}

fn comment_b() -> Ulid {
    Ulid::from_string("00000000000000000000000001").unwrap()
}

fn comment_c() -> Ulid {
    Ulid::from_string("00000000000000000000000002").unwrap()
}

fn human(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    }
}

fn rec(seq: u64, body: RecordBody) -> Record {
    Record {
        seq,
        at: OffsetDateTime::UNIX_EPOCH,
        body,
    }
}

fn header() -> SessionHeader {
    SessionHeader {
        ulid: Ulid::from_string("00000000000000000000000009").unwrap(),
        version: FORMAT_VERSION,
        project: "demo".to_string(),
        repo_root: Some("/repos/demo".to_string()),
        cwd: "/repos/demo".to_string(),
        source: SourceKind::GitWorktree,
    }
}

fn version(number: u32, path: &str) -> DiffVersionRecord {
    DiffVersionRecord {
        number,
        diff_hash: SidebandHash::of(path.as_bytes()),
        files: vec![FileSummary {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Modified,
            hunk_count: 1,
        }],
    }
}

fn lines_target() -> CommentTarget {
    CommentTarget::Lines {
        file: "src/main.rs".to_string(),
        side: Side::After,
        start_line: LineNo::new(2).unwrap(),
        end_line: LineNo::new(2).unwrap(),
    }
}

#[test]
fn folds_versions_and_comment_chains() {
    let anchor = Anchor {
        snippet: vec!["let b = 3;".to_string()],
        context_before: vec!["let a = 1;".to_string()],
        context_after: vec!["let c = 4;".to_string()],
    };
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            RecordBody::Comment(CommentRecord {
                id: comment_a(),
                author: human("wez"),
                target: lines_target(),
                version: 0,
                anchor: Some(anchor.clone()),
                body: "why?".to_string(),
            }),
        ),
        rec(
            3,
            RecordBody::Comment(CommentRecord {
                id: comment_b(),
                author: human("dev"),
                target: CommentTarget::File {
                    file: "src/main.rs".to_string(),
                },
                version: 0,
                anchor: None,
                body: "typo".to_string(),
            }),
        ),
        rec(4, RecordBody::DiffVersion(version(1, "src/lib.rs"))),
        rec(
            5,
            RecordBody::CommentEdit(CommentEdit {
                id: comment_a(),
                body: "why 3?".to_string(),
            }),
        ),
        rec(
            6,
            RecordBody::CommentResolve(CommentResolve {
                id: comment_a(),
                resolved: true,
            }),
        ),
        rec(
            7,
            RecordBody::CommentReanchor(CommentReanchor {
                id: comment_b(),
                version: 1,
                target: CommentTarget::File {
                    file: "src/lib.rs".to_string(),
                },
                confidence: Confidence::Approximate,
            }),
        ),
        rec(
            8,
            RecordBody::CommentDelete(CommentDelete { id: comment_b() }),
        ),
        rec(
            9,
            RecordBody::Comment(CommentRecord {
                id: comment_c(),
                author: human("wez"),
                target: CommentTarget::Review,
                version: 1,
                anchor: None,
                body: "LGTM overall".to_string(),
            }),
        ),
    ];

    let state = fold(&records).unwrap();

    let expected = ReviewState {
        session: header(),
        versions: vec![version(0, "src/main.rs"), version(1, "src/lib.rs")],
        comments: vec![
            CommentState {
                id: comment_a(),
                author: human("wez"),
                target: lines_target(),
                version: 0,
                anchor: Some(anchor),
                body: "why 3?".to_string(),
                resolved: true,
                deleted: false,
                confidence: None,
                created_seq: 2,
                updated_seq: 6,
            },
            CommentState {
                id: comment_b(),
                author: human("dev"),
                target: CommentTarget::File {
                    file: "src/lib.rs".to_string(),
                },
                version: 1,
                anchor: None,
                body: "typo".to_string(),
                resolved: false,
                deleted: true,
                confidence: Some(Confidence::Approximate),
                created_seq: 3,
                updated_seq: 8,
            },
            CommentState {
                id: comment_c(),
                author: human("wez"),
                target: CommentTarget::Review,
                version: 1,
                anchor: None,
                body: "LGTM overall".to_string(),
                resolved: false,
                deleted: false,
                confidence: None,
                created_seq: 9,
                updated_seq: 9,
            },
        ],
    };
    k9::assert_equal!(state, expected);
    k9::assert_equal!(state.latest_version(), Some(&version(1, "src/lib.rs")));
}

#[test]
fn a_mutation_referencing_an_unknown_comment_is_a_corrupt_log() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(
            1,
            RecordBody::CommentEdit(CommentEdit {
                id: comment_a(),
                body: "orphan".to_string(),
            }),
        ),
    ];

    let error = fold(&records).unwrap_err();
    k9::assert_equal!(matches!(error, Error::InconsistentLog(_)), true);
    k9::assert_equal!(
        error.to_string(),
        "inconsistent session log: record at seq 1 references unknown comment \
         00000000000000000000000000"
            .to_string()
    );
}

#[test]
fn an_unrecognized_record_type_is_a_corrupt_log() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::Unknown),
    ];

    let error = fold(&records).unwrap_err();
    k9::assert_equal!(matches!(error, Error::InconsistentLog(_)), true);
    k9::assert_equal!(
        error.to_string(),
        "inconsistent session log: unrecognized record type at seq 1".to_string()
    );
}

#[test]
fn a_newer_format_version_is_refused() {
    let mut newer = header();
    newer.version = FORMAT_VERSION + 1;
    let records = vec![rec(0, RecordBody::Session(newer))];

    let error = fold(&records).unwrap_err();
    k9::assert_equal!(
        matches!(error, Error::UnsupportedVersion { found, supported }
            if found == FORMAT_VERSION + 1 && supported == FORMAT_VERSION),
        true
    );
    k9::assert_equal!(
        error.to_string(),
        "session format version 2 is newer than supported version 1".to_string()
    );
}

#[test]
fn a_log_without_a_header_cannot_be_folded() {
    let error = fold(&[]).unwrap_err();
    k9::assert_equal!(matches!(error, Error::MissingHeader), true);
    k9::assert_equal!(
        error.to_string(),
        "session has no header record".to_string()
    );
}
