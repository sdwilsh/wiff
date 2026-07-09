//! The JSON rendering of a folded review, for programmatic consumers.

use serde::Serialize;
use wiff_core::record::FileSummary;
use wiff_core::review::{CommentState, ReviewState};

use super::{latest_files, live_comments};

/// The JSON schema version emitted by `wiff render --format json`.
const JSON_SCHEMA_VERSION: u32 = 1;

/// Render `state` as a versioned JSON document.
pub(super) fn render(state: &ReviewState) -> anyhow::Result<String> {
    let envelope = JsonEnvelope {
        schema_version: JSON_SCHEMA_VERSION,
        session: JsonSession {
            ulid: state.session.ulid.to_string(),
            project: &state.session.project,
            repo_root: state.session.repo_root.as_deref(),
            cwd: &state.session.cwd,
            source: state.session.source.as_str(),
        },
        files: latest_files(state),
        comments: live_comments(state),
    };
    Ok(serde_json::to_string_pretty(&envelope)?)
}

/// The versioned JSON view of the folded review state.
#[derive(Serialize)]
struct JsonEnvelope<'a> {
    schema_version: u32,
    session: JsonSession<'a>,
    files: &'a [FileSummary],
    comments: Vec<&'a CommentState>,
}

/// The session identity fields exposed in JSON, with the ULID and source as
/// stable strings rather than their in-memory representations.
#[derive(Serialize)]
struct JsonSession<'a> {
    ulid: String,
    project: &'a str,
    repo_root: Option<&'a str>,
    cwd: &'a str,
    source: &'static str,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render;
    use crate::render::fixture::state;

    #[test]
    fn json_is_the_versioned_folded_state_without_deleted() {
        let out = render(&state()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let expected = json!({
            "schema_version": 1,
            "session": {
                "ulid": "00000000000000000000000000",
                "project": "demo",
                "repo_root": "/repos/demo",
                "cwd": "/repos/demo",
                "source": "git_worktree"
            },
            "files": [
                {
                    "old_path": "main.rs",
                    "new_path": "main.rs",
                    "status": "modified",
                    "hunk_count": 1
                }
            ],
            "comments": [
                {
                    "id": "00000000000000000000000001",
                    "author": { "name": "wez", "kind": "human" },
                    "target": {
                        "target": "lines",
                        "file": "main.rs",
                        "side": "after",
                        "start_line": 2,
                        "end_line": 2
                    },
                    "version": 0,
                    "anchor": {
                        "snippet": ["let b = 3;"],
                        "context_before": ["let a = 1;"],
                        "context_after": ["let c = 4;"]
                    },
                    "body": "why 3?",
                    "resolved": false,
                    "deleted": false,
                    "confidence": null,
                    "created_seq": 2,
                    "updated_seq": 2
                },
                {
                    "id": "00000000000000000000000002",
                    "author": { "name": "assistant", "kind": "agent" },
                    "target": { "target": "file", "file": "main.rs" },
                    "version": 0,
                    "anchor": null,
                    "body": "needs tests",
                    "resolved": false,
                    "deleted": false,
                    "confidence": null,
                    "created_seq": 3,
                    "updated_seq": 3
                },
                {
                    "id": "00000000000000000000000003",
                    "author": { "name": "wez", "kind": "human" },
                    "target": { "target": "review" },
                    "version": 0,
                    "anchor": null,
                    "body": "overall solid",
                    "resolved": false,
                    "deleted": false,
                    "confidence": null,
                    "created_seq": 4,
                    "updated_seq": 4
                },
                {
                    "id": "00000000000000000000000004",
                    "author": { "name": "dev", "kind": "human" },
                    "target": {
                        "target": "lines",
                        "file": "other.rs",
                        "side": "after",
                        "start_line": 5,
                        "end_line": 6
                    },
                    "version": 0,
                    "anchor": null,
                    "body": "moved code",
                    "resolved": false,
                    "deleted": false,
                    "confidence": "approximate",
                    "created_seq": 5,
                    "updated_seq": 6
                }
            ]
        });
        k9::assert_equal!(value, expected);
    }
}
