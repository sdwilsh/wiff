#![allow(missing_docs)]

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::comment::{
    delete_event, disposition_event, edit_event, import_create, import_delete, import_edit,
    import_resolve, link_event, reanchor_event, resolve_event, sync_marker_event,
};
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentNumber,
    CommentReanchor, CommentRef, CommentTarget, Confidence, Description, DescriptionRecord,
    DiffVersionRecord, Disposition, ExternalKind, ExternalRef, FORMAT_VERSION, FileSummary,
    ForgeId, Record, RecordBody, ScmSource, Seq, SessionHeader, SourceKind, TipRule, VersionNumber,
    comment_body_marker,
};
use wiff_core::review::{
    ActorVerdict, CommentState, DescriptionState, ReviewState, SyncedState, fold, threads,
};
use wiff_core::{BaseRuleset, Error, ScmType, SidebandHash};
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
        kind: CommentEventKind::Create(CommentCreate {
            target,
            version: VersionNumber(version),
            anchor,
            body: body.to_string(),
            disposition: None,
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
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("ref(name(deadbeef))"),
            tip: TipRule::Worktree,
            branch_hint: None,
        }),
        forge: None,
    }
}

fn version(number: u32, path: &str) -> DiffVersionRecord {
    DiffVersionRecord {
        number: VersionNumber(number),
        diff_hash: SidebandHash::of(path.as_bytes()),
        base_revision: None,
        base_tip_relative: false,
        head_revision: None,
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
        description: None,
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
                disposition: None,
                confidence: None,
                origin: None,
                synced: None,
                number: Some(wiff_core::record::CommentNumber(1)),
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
                disposition: None,
                confidence: Some(Confidence::Approximate),
                origin: None,
                synced: None,
                number: Some(wiff_core::record::CommentNumber(2)),
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
                disposition: None,
                confidence: None,
                origin: None,
                synced: None,
                number: Some(wiff_core::record::CommentNumber(3)),
                created_seq: Seq(9),
                updated_seq: Seq(9),
            },
        ],
        verdicts: Vec::new(),
        pushed_verdicts: Vec::new(),
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
        kind: CommentEventKind::Create(CommentCreate {
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "imported".to_string(),
            disposition: None,
        }),
    });
    let imported_edit = RecordBody::CommentEvent(CommentEvent {
        id: comment_a(),
        author: human("dev"),
        authored_at: Some(authored_edit),
        origin: None,
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

fn review_comment_ref(id: &str) -> ExternalRef {
    ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: id.to_string(),
        url: Some(format!(
            "https://github.com/octo/demo/pull/7#discussion_r{id}"
        )),
    }
}

#[test]
fn a_link_binds_a_local_comment_to_its_forge_object_without_reattributing_it() {
    // A link sets origin and stamps the time forward while preserving
    // authorship. The link record has a later time than the create, so a folded
    // updated_at that moved to it proves the link stamped the comment.
    let linked_at = OffsetDateTime::from_unix_timestamp(3_000_000).unwrap();
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
                "needs a test",
            ),
        ),
        Record {
            seq: Seq(3),
            at: linked_at,
            body: link_event(
                comment_a(),
                human("wez"),
                review_comment_ref("610"),
                comment_body_marker("needs a test"),
            ),
        },
    ];

    let state = fold(&records).unwrap();

    wince::assert_eq!(
        state.comments,
        vec![CommentState {
            id: comment_a(),
            author: human("wez"),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "needs a test".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: linked_at,
            updated_by: human("wez"),
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: Some(review_comment_ref("610")),
            synced: Some(SyncedState {
                body_marker: comment_body_marker("needs a test"),
                resolved: false,
            }),
            number: Some(CommentNumber(1)),
            created_seq: Seq(2),
            updated_seq: Seq(3),
        }]
    );
}

#[test]
fn linking_an_already_linked_comment_is_a_corrupt_log() {
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
                "needs a test",
            ),
        ),
        rec(
            3,
            link_event(
                comment_a(),
                human("wez"),
                review_comment_ref("610"),
                comment_body_marker("needs a test"),
            ),
        ),
        rec(
            4,
            link_event(
                comment_a(),
                human("wez"),
                review_comment_ref("611"),
                comment_body_marker("needs a test"),
            ),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 4 links comment 00000000000000000000000000 that already mirrors a forge object"
    );
}

#[test]
fn a_marker_advance_records_a_pushed_edit_and_resolve_as_synced() {
    // A comment linked at "first wording", then edited and resolved locally, has
    // a synced marker that no longer matches its body or resolution: those are
    // the unpushed changes push finds. Push publishes the edit and the resolve
    // as separate writes and appends a marker-advance for each, advancing that
    // one field alone. Once both have run the divergence is gone, without
    // disturbing the body, resolution, or the resolve's attribution.
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
                "first wording",
            ),
        ),
        rec(
            3,
            link_event(
                comment_a(),
                human("wez"),
                review_comment_ref("610"),
                comment_body_marker("first wording"),
            ),
        ),
        rec(
            4,
            edit_event(comment_a(), human("wez"), "sharper wording".to_string()),
        ),
        rec(5, resolve_event(comment_a(), human("wez"), true)),
        rec(
            6,
            sync_marker_event(
                comment_a(),
                human("wez"),
                Some(comment_body_marker("sharper wording")),
                None,
            ),
        ),
        rec(
            7,
            sync_marker_event(comment_a(), human("wez"), None, Some(true)),
        ),
    ];

    let state = fold(&records).unwrap();

    wince::assert_eq!(
        state.comments,
        vec![CommentState {
            id: comment_a(),
            author: human("wez"),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "sharper wording".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: human("wez"),
            resolved: true,
            resolved_by: Some(human("wez")),
            resolved_at: Some(OffsetDateTime::UNIX_EPOCH),
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: Some(review_comment_ref("610")),
            synced: Some(SyncedState {
                body_marker: comment_body_marker("sharper wording"),
                resolved: true,
            }),
            number: Some(CommentNumber(1)),
            created_seq: Seq(2),
            updated_seq: Seq(5),
        }]
    );
}

#[test]
fn advancing_the_marker_of_an_unlinked_comment_is_a_corrupt_log() {
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
                "never linked",
            ),
        ),
        rec(
            3,
            sync_marker_event(
                comment_a(),
                human("wez"),
                Some(comment_body_marker("never linked")),
                Some(false),
            ),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 3 advances the synced marker of comment 00000000000000000000000000 that mirrors no forge object"
    );
}

#[test]
fn imported_events_mirror_their_forge_object_at_the_forges_own_times() {
    // The folded times come from each event's authored_at, not the epoch the
    // records were recorded at.
    let created_at = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
    let edited_at = OffsetDateTime::from_unix_timestamp(2_000_000).unwrap();
    let resolved_at = OffsetDateTime::from_unix_timestamp(3_000_000).unwrap();
    let origin = review_comment_ref("610");
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            import_create(
                comment_a(),
                human("dev"),
                origin.clone(),
                created_at,
                CommentCreate {
                    target: CommentTarget::Review,
                    version: VersionNumber(0),
                    anchor: None,
                    body: "needs a test".to_string(),
                    disposition: None,
                },
            ),
        ),
        rec(
            3,
            import_edit(
                comment_a(),
                human("dev"),
                origin.clone(),
                edited_at,
                "needs a unit test".to_string(),
            ),
        ),
        rec(
            4,
            import_resolve(comment_a(), human("wez"), origin.clone(), resolved_at, true),
        ),
    ];

    let state = fold(&records).unwrap();

    wince::assert_eq!(
        state.comments,
        vec![CommentState {
            id: comment_a(),
            author: human("dev"),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "needs a unit test".to_string(),
            created_at,
            updated_at: resolved_at,
            updated_by: human("wez"),
            resolved: true,
            resolved_by: Some(human("wez")),
            resolved_at: Some(resolved_at),
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: Some(origin),
            synced: Some(SyncedState {
                body_marker: comment_body_marker("needs a unit test"),
                resolved: true,
            }),
            number: Some(CommentNumber(1)),
            created_seq: Seq(2),
            updated_seq: Seq(4),
        }]
    );
}

#[test]
fn an_imported_delete_tombstones_a_comment_removed_upstream() {
    let created_at = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
    let deleted_at = OffsetDateTime::from_unix_timestamp(4_000_000).unwrap();
    let origin = review_comment_ref("610");
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            import_create(
                comment_a(),
                human("dev"),
                origin.clone(),
                created_at,
                CommentCreate {
                    target: CommentTarget::Review,
                    version: VersionNumber(0),
                    anchor: None,
                    body: "stray thought".to_string(),
                    disposition: None,
                },
            ),
        ),
        rec(
            3,
            import_delete(comment_a(), human("dev"), origin.clone(), deleted_at),
        ),
    ];

    let state = fold(&records).unwrap();

    wince::assert_eq!(
        state.comments,
        vec![CommentState {
            id: comment_a(),
            author: human("dev"),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "stray thought".to_string(),
            created_at,
            updated_at: deleted_at,
            updated_by: human("dev"),
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: true,
            deleted_by: Some(human("dev")),
            deleted_at: Some(deleted_at),
            disposition: None,
            confidence: None,
            origin: Some(origin),
            synced: Some(SyncedState {
                body_marker: comment_body_marker("stray thought"),
                resolved: false,
            }),
            number: Some(CommentNumber(1)),
            created_seq: Seq(2),
            updated_seq: Seq(3),
        }]
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
        "session format version 5 does not match supported version 4"
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
        "session format version 3 does not match supported version 4"
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
            disposition: None,
            confidence: None,
            origin: None,
            synced: None,
            number: Some(wiff_core::record::CommentNumber(1)),
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
            disposition: None,
            confidence: None,
            origin: None,
            synced: None,
            number: Some(wiff_core::record::CommentNumber(2)),
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
fn a_comment_reference_resolves_a_number_to_its_id_and_passes_a_ulid_through() {
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
                comment_b(),
                human("dev"),
                CommentTarget::Review,
                0,
                None,
                "second",
            ),
        ),
    ];
    let state = fold(&records).unwrap();
    // Numbers run in create order, so comment_a is #1 and comment_b is #2.
    wince::assert_eq!(
        state
            .comment_by_number(CommentNumber(2))
            .map(|comment| (comment.number, comment.id)),
        Some((Some(CommentNumber(2)), comment_b()))
    );
    wince::assert_eq!(
        state.comment_by_number(CommentNumber(3)).map(|c| c.id),
        None
    );
    // A number resolves to its comment's id; a ULID passes straight through.
    wince::assert_eq!(
        state.resolve_ref(CommentRef::Number(CommentNumber(1))).ok(),
        Some(comment_a())
    );
    wince::assert_eq!(
        state.resolve_ref(CommentRef::Ulid(comment_b())).ok(),
        Some(comment_b())
    );
    // A number past the last comment is refused.
    wince::assert_eq!(
        state
            .resolve_ref(CommentRef::Number(CommentNumber(9)))
            .map_err(|err| err.to_string()),
        Err("no comment #9 in this session".to_string())
    );
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

fn description_record(
    author: Author,
    title: &str,
    body: &str,
    origin: Option<ExternalRef>,
    synced_marker: Option<String>,
) -> RecordBody {
    RecordBody::Description(DescriptionRecord {
        author,
        authored_at: None,
        origin,
        synced_marker,
        description: Description {
            title: title.to_string(),
            body: body.to_string(),
        },
    })
}

fn pull_body_ref() -> ExternalRef {
    ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::Description,
        id: "42".to_string(),
        url: None,
    }
}

#[test]
fn the_most_recent_description_revision_wins() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            description_record(human("wez"), "First cut", "an early draft", None, None),
        ),
        rec(
            3,
            description_record(
                human("dev"),
                "Tidy the parser",
                "split the lexer out",
                None,
                None,
            ),
        ),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        state.description,
        Some(DescriptionState {
            content: Description {
                title: "Tidy the parser".to_string(),
                body: "split the lexer out".to_string(),
            },
            author: human("dev"),
            updated_at: OffsetDateTime::UNIX_EPOCH,
            origin: None,
            synced_marker: None,
        })
    );
}

#[test]
fn a_local_description_edit_keeps_the_forge_binding_of_the_revision_it_replaces() {
    // An imported revision binds the description to a pull request body and sets
    // a sync marker; a later local edit sets neither, so the folded origin and
    // marker stay while the content becomes the local edit's.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            description_record(
                human("wez"),
                "Imported title",
                "imported body",
                Some(pull_body_ref()),
                Some("etag-1".to_string()),
            ),
        ),
        rec(
            3,
            description_record(human("wez"), "Edited title", "edited body", None, None),
        ),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        state.description,
        Some(DescriptionState {
            content: Description {
                title: "Edited title".to_string(),
                body: "edited body".to_string(),
            },
            author: human("wez"),
            updated_at: OffsetDateTime::UNIX_EPOCH,
            origin: Some(pull_body_ref()),
            synced_marker: Some("etag-1".to_string()),
        })
    );
}

#[test]
fn an_imported_descriptions_folded_time_comes_from_its_authored_time() {
    // A revision mirrored from a forge records the time it was authored upstream
    // in `authored_at`, distinct from when wiff recorded it in `Record::at`. The
    // folded `updated_at` reflects the upstream time, not wiff's.
    let authored = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let imported = RecordBody::Description(DescriptionRecord {
        author: human("wez"),
        authored_at: Some(authored),
        origin: Some(pull_body_ref()),
        synced_marker: Some("etag-1".to_string()),
        description: Description {
            title: "Imported title".to_string(),
            body: "imported body".to_string(),
        },
    });
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(2, imported),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        state.description,
        Some(DescriptionState {
            content: Description {
                title: "Imported title".to_string(),
                body: "imported body".to_string(),
            },
            author: human("wez"),
            updated_at: authored,
            origin: Some(pull_body_ref()),
            synced_marker: Some("etag-1".to_string()),
        })
    );
}

/// A review-level create for `id` by `author` that sets `disposition` from
/// the moment it exists.
fn create_verdict_event(
    id: Ulid,
    author: Author,
    disposition: Disposition,
    body: &str,
) -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id,
        author,
        authored_at: None,
        origin: None,
        kind: CommentEventKind::Create(CommentCreate {
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            disposition: Some(disposition),
        }),
    })
}

/// The per-comment verdict of each comment, in comment order, alongside the
/// review's derived per-actor verdicts.
fn verdicts(state: &ReviewState) -> (Vec<Option<Disposition>>, Vec<ActorVerdict>) {
    (
        state.comments.iter().map(|c| c.disposition).collect(),
        state.verdicts.clone(),
    )
}

#[test]
fn each_actor_gets_their_own_verdict() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(comment_a(), human("wez"), Disposition::Approve, "lgtm"),
        ),
        rec(
            3,
            create_verdict_event(comment_b(), human("dev"), Disposition::RequestChanges, "no"),
        ),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (
            vec![
                Some(Disposition::Approve),
                Some(Disposition::RequestChanges)
            ],
            vec![
                ActorVerdict {
                    author: human("wez"),
                    disposition: Disposition::Approve,
                },
                ActorVerdict {
                    author: human("dev"),
                    disposition: Disposition::RequestChanges,
                },
            ],
        )
    );
}

#[test]
fn a_set_disposition_on_an_older_comment_outranks_a_later_neutral_one() {
    // wez leaves two neutral comments, then sets a verdict on the earlier one.
    // The verdict comes from the latest verdict-bearing event, not the newest
    // comment.
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
                "one",
            ),
        ),
        rec(
            3,
            create_event(
                comment_b(),
                human("wez"),
                CommentTarget::Review,
                0,
                None,
                "two",
            ),
        ),
        rec(
            4,
            disposition_event(comment_a(), human("wez"), Some(Disposition::RequestChanges)),
        ),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (
            vec![Some(Disposition::RequestChanges), None],
            vec![ActorVerdict {
                author: human("wez"),
                disposition: Disposition::RequestChanges,
            }],
        )
    );
}

#[test]
fn a_set_disposition_by_someone_other_than_the_author_is_a_corrupt_log() {
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
                "mine",
            ),
        ),
        rec(
            3,
            disposition_event(comment_a(), human("dev"), Some(Disposition::Approve)),
        ),
    ];

    let error = fold(&records).unwrap_err();
    wince::assert_eq!(matches!(error, Error::InconsistentLog(_)), true);
    wince::snapshot_display!(
        error,
        "inconsistent session log: record at seq 3 sets a verdict on comment 00000000000000000000000000 authored by someone else"
    );
}

#[test]
fn resolving_your_own_blocking_comment_withdraws_your_verdict() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "fix this",
            ),
        ),
        rec(3, resolve_event(comment_a(), human("wez"), true)),
    ];

    let state = fold(&records).unwrap();
    // The comment still bears its verdict, but the actor's derived verdict is
    // cleared by resolving it.
    wince::assert_eq!(
        verdicts(&state),
        (vec![Some(Disposition::RequestChanges)], Vec::new())
    );
}

#[test]
fn reopening_a_resolved_blocking_comment_reasserts_the_request() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "fix this",
            ),
        ),
        rec(3, resolve_event(comment_a(), human("wez"), true)),
        rec(4, resolve_event(comment_a(), human("wez"), false)),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (
            vec![Some(Disposition::RequestChanges)],
            vec![ActorVerdict {
                author: human("wez"),
                disposition: Disposition::RequestChanges,
            }],
        )
    );
}

#[test]
fn a_verdict_collapses_from_request_changes_to_approve_then_to_neutral() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "concern",
            ),
        ),
        rec(
            3,
            disposition_event(comment_a(), human("wez"), Some(Disposition::Approve)),
        ),
        rec(4, disposition_event(comment_a(), human("wez"), None)),
    ];

    let state = fold(&records).unwrap();
    // The neutral set leaves the comment with no verdict and clears the
    // actor's derived verdict.
    wince::assert_eq!(verdicts(&state), (vec![None], Vec::new()));
}

#[test]
fn a_later_approval_does_not_mask_an_unresolved_blocking_comment() {
    // wez blocks on comment A and approves on comment B. The standing objection
    // dominates: a later approval on an unrelated comment must not clear it.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "fix A",
            ),
        ),
        rec(
            3,
            create_verdict_event(comment_b(), human("wez"), Disposition::Approve, "B is fine"),
        ),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (
            vec![
                Some(Disposition::RequestChanges),
                Some(Disposition::Approve)
            ],
            vec![ActorVerdict {
                author: human("wez"),
                disposition: Disposition::RequestChanges,
            }],
        )
    );
}

#[test]
fn resolving_one_blocking_comment_leaves_a_separate_approval_standing() {
    // wez blocks on A and approves on B, then resolves A. Resolving the blocker
    // must not clobber the approval from the unrelated comment B.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "fix A",
            ),
        ),
        rec(
            3,
            create_verdict_event(comment_b(), human("wez"), Disposition::Approve, "B is fine"),
        ),
        rec(4, resolve_event(comment_a(), human("wez"), true)),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (
            vec![
                Some(Disposition::RequestChanges),
                Some(Disposition::Approve)
            ],
            vec![ActorVerdict {
                author: human("wez"),
                disposition: Disposition::Approve,
            }],
        )
    );
}

#[test]
fn a_resolve_after_reapproving_a_blocking_comment_clears_the_verdict() {
    // A comment blocks, is re-set to approve, then resolved by its author. The
    // result depends only on the final state (approved and resolved by the
    // author), not on the order the resolve and re-set arrived, so the comment
    // contributes nothing and the actor has no verdict.
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "fix this",
            ),
        ),
        rec(
            3,
            disposition_event(comment_a(), human("wez"), Some(Disposition::Approve)),
        ),
        rec(4, resolve_event(comment_a(), human("wez"), true)),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (vec![Some(Disposition::Approve)], Vec::new())
    );
}

#[test]
fn a_withdrawn_comment_is_excluded_from_verdict_derivation() {
    let records = vec![
        rec(0, RecordBody::Session(header())),
        rec(1, RecordBody::DiffVersion(version(0, "src/main.rs"))),
        rec(
            2,
            create_verdict_event(
                comment_a(),
                human("wez"),
                Disposition::RequestChanges,
                "never mind",
            ),
        ),
        rec(3, delete_event(comment_a(), human("wez"))),
    ];

    let state = fold(&records).unwrap();
    wince::assert_eq!(
        verdicts(&state),
        (vec![Some(Disposition::RequestChanges)], Vec::new())
    );
}
