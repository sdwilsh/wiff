#![allow(missing_docs)]

use std::path::Path;

use wiff_core::hash::SidebandHash;
use wiff_core::record::{
    DiffVersionRecord, FORMAT_VERSION, FileSummary, Record, RecordBody, SessionHeader, SourceKind,
};
use wiff_core::session::read_records;
use wiff_core::{CapturedDiff, DiffSource, ProjectIdentity, create_session};
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
    }
}

#[tokio::test]
async fn captured_diff_is_its_own_source() {
    let original = stdin_diff();
    let captured = original.capture().await.unwrap();
    k9::assert_equal!(captured, original);
}

#[test]
fn create_session_writes_header_version_and_sideband() {
    let base = tempfile::tempdir().unwrap();
    let captured = stdin_diff();
    let log = create_session(base.path(), &identity(), Path::new("/work"), &captured).unwrap();

    // The log holds exactly the header and the v0 diff version, in order.
    let records = read_records(log.path()).unwrap();
    let bodies: Vec<(u64, RecordBody)> = records
        .iter()
        .map(|Record { seq, body, .. }| (*seq, body.clone()))
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
                number: 0,
                diff_hash: SidebandHash::of(DIFF.as_bytes()),
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
    k9::assert_equal!(bodies, expected);

    // The raw diff lands in the sideband v0.diff, byte for byte.
    let sideband = log.sideband_dir().join("v0.diff");
    let written = std::fs::read_to_string(&sideband).unwrap();
    k9::assert_equal!(written, DIFF.to_string());
}
