#![allow(missing_docs)]

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::comment::{delete_event, edit_event, reanchor_event, resolve_event};
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentReanchor,
    CommentTarget, Confidence, DiffVersionRecord, FORMAT_VERSION, FileSummary, Record, RecordBody,
    Seq, SessionHeader, SourceKind, VersionNumber,
};
use wiff_core::review::{CommentState, ReviewState, fold, threads};
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
        seq: Seq(seq),
        at: OffsetDateTime::UNIX_EPOCH,
        body,
    }
}

/// A create event for `id`, the record every other event in a chain mutates.
fn create_event(
    id: Ulid,
    author: Author,
    target: CommentTarget,
    version: u32,
    anchor: Option<Anchor>,
    body: &str,
) -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id,
        author,
        authored_at: None,
        origin: None,
        synced_marker: None,
        kind: CommentEventKind::Create(CommentCreate {
            target,
            version: VersionNumber(version),
            anchor,
            body: body.to_string(),
        }),
    })
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
        number: VersionNumber(number),
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
            create_event(
                comment_a(),
                human("wez"),
                lines_target(),
                0,
                Some(anchor.clone()),
                "why?",
            ),
        ),
        rec(
            3,
            create_event(
                comment_b(),
                human("dev"),
                CommentTarget::File {
                    file: "src/main.rs".to_string(),
                },
                0,
                None,
                "typo",
            ),
        ),
        rec(4, RecordBody::DiffVersion(version(1, "src/lib.rs"))),
        rec(
            5,
            edit_event(comment_a(), human("wez"), "why 3?".to_string()),
        ),
        rec(6, resolve_event(comment_a(), human("wez"), true)),
        rec(
            7,
            reanchor_event(
                comment_b(),
                human("dev"),
                CommentReanchor {
                    version: VersionNumber(1),
                    target: CommentTarget::File {
                        file: "src/lib.rs".to_string(),
                    },
                    confidence: Confidence::Approximate,
                },
            ),
        ),
        rec(8, delete_event(comment_b(), human("dev"))),
        rec(
            9,
            create_event(
                comment_c(),
                human("wez"),
                CommentTarget::Review,
                1,
                None,
                "LGTM overall",
            ),
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
                version: VersionNumber(0),
                anchor: Some(anchor),
                body: "why 3?".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("wez"),
                resolved: true,
                resolved_by: Some(human("wez")),
                resolved_at: Some(OffsetDateTime::UNIX_EPOCH),
                deleted: false,
                deleted_by: None,
                deleted_at: None,
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(2),
                updated_seq: Seq(6),
            },
            CommentState {
                id: comment_b(),
                author: human("dev"),
                target: CommentTarget::File {
                    file: "src/lib.rs".to_string(),
                },
                version: VersionNumber(1),
                anchor: None,
                body: "typo".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("dev"),
                resolved: false,
                resolved_by: None,
                resolved_at: None,
                deleted: true,
                deleted_by: Some(human("dev")),
                deleted_at: Some(OffsetDateTime::UNIX_EPOCH),
                confidence: Some(Confidence::Approximate),
                origin: None,
                synced_marker: None,
                created_seq: Seq(3),
                updated_seq: Seq(8),
            },
            CommentState {
                id: comment_c(),
                author: human("wez"),
                target: CommentTarget::Review,
                version: VersionNumber(1),
                anchor: None,
                body: "LGTM overall".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("wez"),
                resolved: false,
                resolved_by: None,
                resolved_at: None,
                deleted: false,
                deleted_by: None,
                deleted_at: None,
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(9),
                updated_seq: Seq(9),
            },
        ],
    };
    wince::assert_eq!(state, expected);
    wince::assert_eq!(state.latest_version(), Some(&version(1, "src/lib.rs")));
}

#[test]
fn an_edit_by_another_actor_updates_updated_by_but_not_author() {
    // A reviewer's comment reworded by an agent keeps its original author while
    // recording the agent as the most recent editor.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "wip",
            ),
        ),
        rec(
            3,
            edit_event(
                comment_a(),
                Author {
                    name: "assistant".to_string(),
                    kind: AuthorKind::Agent,
                },
                "polished".to_string(),
            ),
        ),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(state.comments.len(), 1);
    let comment = &state.comments[0];
    wince::assert_eq!(
        (
            comment.author.clone(),
            comment.updated_by.clone(),
            comment.body.clone()
        ),
        (
            human("wez"),
            Author {
                name: "assistant".to_string(),
                kind: AuthorKind::Agent,
            },
            "polished".to_string()
        )
    );
}

#[test]
fn an_imported_events_folded_time_comes_from_its_authored_time() {
    // An imported event records the authoritative time it was authored on the
    // originating forge in `authored_at`, distinct from when wiff recorded it in
    // `Record::at`. Its folded times come from `authored_at`; a local event,
    // with no `authored_at`, falls back to `Record::at`.
    let recorded = OffsetDateTime::UNIX_EPOCH;
    let authored_create = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
    let authored_edit = OffsetDateTime::from_unix_timestamp(2_000_000).unwrap();

    let imported_create = RecordBody::CommentEvent(CommentEvent {
        id: comment_a(),
        author: human("wez"),
        authored_at: Some(authored_create),
        origin: None,
        synced_marker: None,
        kind: CommentEventKind::Create(CommentCreate {
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "imported".to_string(),
        }),
    });
    let imported_edit = RecordBody::CommentEvent(CommentEvent {
        id: comment_a(),
        author: human("dev"),
        authored_at: Some(authored_edit),
        origin: None,
        synced_marker: None,
        kind: CommentEventKind::Edit {
            body: "imported, edited".to_string(),
        },
    });
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(2, imported_create),
        rec(3, imported_edit),
        // A local create at seq 4, recorded at the epoch with no authored time.
        rec(
            4,
            create_event(
                comment_b(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "local",
            ),
        ),
    ];

    let state = fold(&records).unwrap();
    let times: Vec<(OffsetDateTime, OffsetDateTime)> = state
        .comments
        .iter()
        .map(|comment| (comment.created_at, comment.updated_at))
        .collect();
    wince::assert_eq!(
        times,
        vec![(authored_create, authored_edit), (recorded, recorded),]
    );
}

#[test]
fn a_second_create_for_an_existing_comment_is_a_corrupt_log() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "first",
            ),
        ),
        rec(
            3,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "again",
            ),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 3 re-creates existing comment 00000000000000000000000000"
    );
}

#[test]
fn a_mutation_referencing_an_unknown_comment_is_a_corrupt_log() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(
            1,
            edit_event(comment_a(), human("wez"), "orphan".to_string()),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 1 references unknown comment 00000000000000000000000000"
    );
}

#[test]
fn an_unrecognized_record_type_is_a_corrupt_log() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::Unknown),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: unrecognized record type at seq 1"
    );
}

#[test]
fn a_newer_format_version_is_refused() {
    let mut newer = header();
    newer.version = FORMAT_VERSION + 1;
    let records = vec![rec(0, RecordBody::Session(newer))];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(
        matches!(error, Error::UnsupportedVersion { found, supported }
            if found == FORMAT_VERSION + 1 && supported == FORMAT_VERSION),
        true
    );
    wince::snapshot_display!(
        error,
        "session format version 4 does not match supported version 3"
    );
}

#[test]
fn an_older_format_version_is_refused() {
    let mut older = header();
    older.version = FORMAT_VERSION - 1;
    let records = vec![rec(0, RecordBody::Session(older))];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(
        matches!(error, Error::UnsupportedVersion { found, supported }
            if found == FORMAT_VERSION - 1 && supported == FORMAT_VERSION),
        true
    );
    wince::snapshot_display!(
        error,
        "session format version 2 does not match supported version 3"
    );
}

#[test]
fn a_log_without_a_header_cannot_be_folded() {
    let error = fold(&[]).unwrap_err();
    wince::assert_eq!(matches!(error, Error::MissingHeader), true);
    wince::snapshot_display!(error, "session has no header record");
}

/// A reply to `parent` by `author` with the given identity and body.
fn reply_event(id: Ulid, parent: Ulid, author: Author, body: &str) -> RecordBody {
    create_event(
        id,
        author,
        CommentTarget::Comment { id: parent },
        0,
        None,
        body,
    )
}

#[test]
fn a_reply_folds_with_its_parent_recorded_and_threads_under_it() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "why 3?",
            ),
        ),
        rec(
            3,
            reply_event(comment_b(), comment_a(), human("dev"), "it is the bound"),
        ),
    ];

    let state = fold(&records).unwrap();
    let expected = vec![
        CommentState {
            id: comment_a(),
            author: human("wez"),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "why 3?".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: human("wez"),
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(2),
            updated_seq: Seq(2),
        },
        CommentState {
            id: comment_b(),
            author: human("dev"),
            target: CommentTarget::Comment { id: comment_a() },
            version: VersionNumber(0),
            anchor: None,
            body: "it is the bound".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: human("dev"),
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(3),
            updated_seq: Seq(3),
        },
    ];
    wince::assert_eq!(state.comments, expected);

    let threads = threads(&state.comments);
    let grouped: Vec<(Ulid, Vec<Ulid>)> = threads
        .iter()
        .map(|thread| {
            (
                thread.root.id,
                thread.replies.iter().map(|reply| reply.id).collect(),
            )
        })
        .collect();
    wince::assert_eq!(grouped, vec![(comment_a(), vec![comment_b()])]);
}

#[test]
fn replies_flatten_under_the_root_ordered_by_log_position() {
    // A reply to a reply flattens into the root's sequence, ordered by log
    // position so it reads in the order the replies arrived.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "root",
            ),
        ),
        // Append a reply to the root, then a reply to that reply; the flattened
        // order follows their log position.
        rec(
            3,
            reply_event(comment_c(), comment_a(), human("dev"), "first to arrive"),
        ),
        rec(
            4,
            reply_event(comment_b(), comment_c(), human("wez"), "second to arrive"),
        ),
    ];

    let state = fold(&records).unwrap();
    let threads = threads(&state.comments);
    let grouped: Vec<(Ulid, Vec<Ulid>)> = threads
        .iter()
        .map(|thread| {
            (
                thread.root.id,
                thread.replies.iter().map(|reply| reply.id).collect(),
            )
        })
        .collect();
    wince::assert_eq!(grouped, vec![(comment_a(), vec![comment_c(), comment_b()])]);
}

#[test]
fn a_reply_under_a_deleted_parent_is_kept() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "root",
            ),
        ),
        rec(3, delete_event(comment_a(), human("wez"))),
        rec(
            4,
            reply_event(comment_b(), comment_a(), human("dev"), "still valid"),
        ),
    ];

    let state = fold(&records).unwrap();
    let kept: Vec<(Ulid, Option<Ulid>, bool)> = state
        .comments
        .iter()
        .map(|comment| (comment.id, comment.reply_to(), comment.deleted))
        .collect();
    wince::assert_eq!(
        kept,
        vec![
            (comment_a(), None, true),
            (comment_b(), Some(comment_a()), false),
        ]
    );
}

#[test]
fn a_reanchor_onto_a_reply_target_is_a_corrupt_log() {
    // A reply has no anchor and is never reanchored; a reanchor onto a `Comment`
    // target would turn an anchored comment into a reply, so fold rejects it.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "root",
            ),
        ),
        rec(
            3,
            reanchor_event(
                comment_a(),
                human("wez"),
                CommentReanchor {
                    version: VersionNumber(0),
                    target: CommentTarget::Comment { id: comment_b() },
                    confidence: Confidence::Exact,
                },
            ),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 3 reanchors comment 00000000000000000000000000 onto a reply target"
    );
}

#[test]
fn a_reply_to_an_unknown_comment_is_a_corrupt_log() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            reply_event(comment_b(), comment_a(), human("dev"), "orphan"),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 2 replies to unknown comment 00000000000000000000000000"
    );
}

#[test]
fn a_self_reply_is_a_corrupt_log() {
    // A create whose target replies to its own id is rejected as an unknown
    // parent, since the comment is not folded until after this validation.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            reply_event(comment_a(), comment_a(), human("wez"), "myself"),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 2 replies to unknown comment 00000000000000000000000000"
    );
}

#[test]
fn a_reply_that_arrives_before_its_parent_is_a_corrupt_log() {
    // Fold is single-pass and order-dependent: a reply whose parent has not yet
    // folded is corruption, not deferred resolution.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            reply_event(comment_b(), comment_a(), human("dev"), "early"),
        ),
        rec(
            3,
            create_event(
                comment_a(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "late root",
            ),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 2 replies to unknown comment 00000000000000000000000000"
    );
}
