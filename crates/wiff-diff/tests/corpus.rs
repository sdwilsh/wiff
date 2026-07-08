#![allow(missing_docs)]

//! Pins the parser's behavior across the diff formats produced by git,
//! Mercurial, Subversion, Bazaar, and plain `diff -u`, using a vendored corpus
//! of real-world diffs (see `fixtures/ATTRIBUTION.md`).
//!
//! Each `fixtures/<name>.diff` is parsed and rendered to a readable form that
//! is compared against a checked-in `fixtures/<name>.parsed` golden file. Run
//! with `WIFF_BLESS=1` to regenerate the golden files after an intended change.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use wiff_diff::model::{Diff, FileStatus, Hunk, LineKind};
use wiff_diff::parse::parse;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Render a parsed [`Diff`] to a stable, human-readable text form that captures
/// every field the parser populates.
fn render(diff: &Diff) -> String {
    let mut out = String::new();
    if diff.files.is_empty() {
        return "(no files)\n".to_string();
    }
    for (index, file) in diff.files.iter().enumerate() {
        let status = match file.status {
            FileStatus::Added => "Added",
            FileStatus::Deleted => "Deleted",
            FileStatus::Modified => "Modified",
            FileStatus::Renamed => "Renamed",
        };
        writeln!(
            out,
            "file[{index}]: {} -> {} ({status})",
            file.old_path, file.new_path
        )
        .unwrap();
        for hunk in &file.hunks {
            render_hunk(&mut out, hunk);
        }
    }
    out
}

fn render_hunk(out: &mut String, hunk: &Hunk) {
    let Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        section,
        lines,
    } = hunk;
    let section = match section {
        Some(text) => format!(" section={text:?}"),
        None => String::new(),
    };
    writeln!(
        out,
        "  @@ -{old_start},{old_len} +{new_start},{new_len} @@{section}"
    )
    .unwrap();
    for line in lines {
        let sign = match line.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
        };
        let old = line
            .old_lineno
            .map_or_else(|| ".".to_string(), |n| n.to_string());
        let new = line
            .new_lineno
            .map_or_else(|| ".".to_string(), |n| n.to_string());
        writeln!(out, "    {sign} old={old} new={new} |{}", line.text).unwrap();
    }
}

/// Parse every `*.diff` in the fixtures directory and compare its rendered form
/// against the matching `*.parsed` golden file.
#[test]
fn parses_the_vendored_corpus() {
    let dir = fixtures_dir();
    let bless = std::env::var_os("WIFF_BLESS").is_some();

    let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read fixtures dir")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "diff"))
        .collect();
    fixtures.sort();
    assert!(
        !fixtures.is_empty(),
        "no fixtures found in {}",
        dir.display()
    );

    for fixture in fixtures {
        let input = fs::read_to_string(&fixture).expect("read fixture");
        let diff = parse(&input).expect("parse fixture");
        let rendered = render(&diff);
        let golden = fixture.with_extension("parsed");
        if bless {
            fs::write(&golden, &rendered).expect("write golden");
            continue;
        }
        let expected = fs::read_to_string(&golden).unwrap_or_else(|_| {
            panic!("missing golden {}; run with WIFF_BLESS=1", golden.display())
        });
        k9::assert_equal!(
            rendered,
            expected,
            "parsed form of {} diverged from its golden file",
            fixture.display()
        );
    }
}
