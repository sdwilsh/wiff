//! Recognising machine-generated files a review collapses by default.
//!
//! Some files are produced by a tool rather than written by hand: lock files,
//! minified bundles, code with a generator's banner. They are rarely worth
//! reading line by line, so a review can fold them away and mark them with a
//! badge. A file qualifies by either of two independent tests: its path matching
//! a curated glob, or a marker string appearing near the top of its after-side
//! content. Each set of tests starts from built-in defaults the user's config
//! extends, a leading `!` on a configured entry dropping a matching default.

use globset::{GlobBuilder, GlobMatcher};

use crate::model::{FileDiff, Side};

/// Path globs whose match marks a file generated, checked against the file's
/// basename, or against the whole path when the glob itself contains a `/`.
const BUILTIN_NAMES: &[&str] = &[
    "Cargo.lock",
    "Gemfile.lock",
    "Pipfile.lock",
    "composer.lock",
    "flake.lock",
    "go.sum",
    "npm-shrinkwrap.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "poetry.lock",
    "yarn.lock",
    "*.min.css",
    "*.min.js",
];

/// How many lines down from the top of a file a marker is honored. A generator
/// banner sits in the file's opening comment, so a match deeper than this is
/// treated as ordinary content that merely mentions the marker.
const MARKER_HEAD_LINES: u32 = 40;

/// The built-in marker strings, any of which near the top of a file marks it
/// generated. Assembled at runtime so this source file, which explains the
/// convention, does not itself contain the literal banner that would flag it.
fn builtin_markers() -> Vec<String> {
    vec![format!("@{}", "generated")]
}

/// Why a file was recognised as generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedReason {
    /// The file's path matched a name glob.
    Name,
    /// A marker string appeared near the top of the file's content.
    Marker,
}

/// An error compiling a generated-file name glob into a matcher.
#[derive(Debug, thiserror::Error)]
pub enum GeneratedError {
    /// A configured name glob was not a valid glob.
    #[error("invalid generated-file name pattern {pattern:?}: {source}")]
    Pattern {
        /// The glob that failed to compile.
        pattern: String,
        /// The underlying glob compilation error.
        source: Box<globset::Error>,
    },
}

/// One compiled name glob and whether it matches the whole path rather than the
/// basename alone.
struct NameGlob {
    matcher: GlobMatcher,
    whole_path: bool,
}

/// The compiled tests deciding whether a file is generated: the name globs and
/// the marker strings, each the built-in set merged with the user's extensions.
pub struct GeneratedMatchers {
    names: Vec<NameGlob>,
    markers: Vec<String>,
}

impl GeneratedMatchers {
    /// Build the matchers from the built-in name globs and markers, extended by
    /// `name_overrides` and `marker_overrides`. A plain entry adds to its set; an
    /// entry led by `!` drops a built-in that matches it verbatim.
    pub fn new(
        name_overrides: &[String],
        marker_overrides: &[String],
    ) -> Result<Self, GeneratedError> {
        let names = merge(BUILTIN_NAMES, name_overrides)
            .into_iter()
            .map(NameGlob::compile)
            .collect::<Result<_, _>>()?;
        // An empty marker would match every line via `contains("")`, folding the
        // whole review, so a stray empty entry is dropped rather than honored.
        let markers = merge(&builtin_markers(), marker_overrides)
            .into_iter()
            .filter(|marker| !marker.is_empty())
            .collect();
        Ok(Self { names, markers })
    }

    /// The matchers with only the built-in globs and markers.
    pub fn builtins() -> Self {
        Self::new(&[], &[]).expect("built-in generated-file globs are valid")
    }

    /// Why `file` counts as generated, or `None` when it does not. A name match
    /// is preferred over a marker match, since the path is certain while the
    /// marker is a best-effort read of the file's head.
    pub fn classify(&self, file: &FileDiff) -> Option<GeneratedReason> {
        if self.matches_name(file.display_path()) {
            return Some(GeneratedReason::Name);
        }
        if self.matches_marker(file) {
            return Some(GeneratedReason::Marker);
        }
        None
    }

    /// Whether `path` matches any configured name glob.
    fn matches_name(&self, path: &str) -> bool {
        let basename = path.rsplit('/').next().unwrap_or(path);
        self.names.iter().any(|glob| {
            let candidate = if glob.whole_path { path } else { basename };
            glob.matcher.is_match(candidate)
        })
    }

    /// Whether any marker string appears on an after-side line within the head of
    /// `file`. Only lines numbered within [`MARKER_HEAD_LINES`] of the top count,
    /// so a marker recognises a file only when the file's opening lines reach the
    /// diff. A capture that omits the head (the sole change being deep in the
    /// file) therefore misses a marker-only file; name matching, which needs no
    /// content, stays reliable there.
    fn matches_marker(&self, file: &FileDiff) -> bool {
        if self.markers.is_empty() {
            return false;
        }
        for hunk in &file.hunks {
            for line in &hunk.lines {
                let Some(lineno) = line.lineno(Side::After) else {
                    continue;
                };
                if lineno.get() > MARKER_HEAD_LINES {
                    continue;
                }
                if self
                    .markers
                    .iter()
                    .any(|marker| line.text.contains(marker.as_str()))
                {
                    return true;
                }
            }
        }
        false
    }
}

impl NameGlob {
    /// Compile a name glob, matching the whole path when it contains a `/` and
    /// the basename otherwise. A `*` stays within a path segment while `**`
    /// crosses separators.
    fn compile(glob: String) -> Result<Self, GeneratedError> {
        let whole_path = glob.contains('/');
        let matcher = GlobBuilder::new(&glob)
            .literal_separator(true)
            .build()
            .map_err(|source| GeneratedError::Pattern {
                pattern: glob,
                source: Box::new(source),
            })?
            .compile_matcher();
        Ok(Self {
            matcher,
            whole_path,
        })
    }
}

/// Merge `overrides` onto `builtins`: a plain entry is appended if new, an entry
/// led by `!` removes a built-in equal to its remainder. Order is preserved so
/// the result reads as the built-ins followed by the user's additions.
fn merge<S: AsRef<str>>(builtins: &[S], overrides: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = builtins.iter().map(|s| s.as_ref().to_string()).collect();
    for entry in overrides {
        match entry.strip_prefix('!') {
            Some(dropped) => merged.retain(|kept| kept != dropped),
            None if !merged.iter().any(|kept| kept == entry) => merged.push(entry.clone()),
            None => {}
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{GeneratedMatchers, GeneratedReason};
    use crate::line::LineNo;
    use crate::model::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};

    /// A single-hunk added file whose after-side content is `lines`, numbered
    /// from 1, for exercising path and marker classification.
    fn added_file(path: &str, lines: &[&str]) -> FileDiff {
        let hunk_lines = lines
            .iter()
            .enumerate()
            .map(|(index, text)| DiffLine {
                kind: LineKind::Added,
                text: (*text).to_string(),
                old_lineno: None,
                new_lineno: LineNo::new(index as u32 + 1),
            })
            .collect();
        FileDiff {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Added,
            hunks: vec![Hunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: lines.len() as u32,
                section: None,
                lines: hunk_lines,
            }],
        }
    }

    /// The reason each file classifies as, rendered one `path -> reason` row per
    /// file so the whole verdict is asserted together.
    fn classify(matchers: &GeneratedMatchers, files: &[FileDiff]) -> String {
        let mut out = String::new();
        for file in files {
            let reason = match matchers.classify(file) {
                Some(GeneratedReason::Name) => "name",
                Some(GeneratedReason::Marker) => "marker",
                None => "no",
            };
            out.push_str(&format!("{} -> {}\n", file.display_path(), reason));
        }
        out
    }

    /// Whether `glob` (as a configured name pattern) recognises each of `paths`,
    /// one `path -> reason` row per input so the whole verdict is asserted
    /// together.
    fn name_report(glob: &str, paths: &[&str]) -> String {
        let matchers = GeneratedMatchers::new(&[glob.to_string()], &[]).unwrap();
        let files: Vec<FileDiff> = paths.iter().map(|path| added_file(path, &["x"])).collect();
        classify(&matchers, &files)
    }

    #[test]
    fn built_in_names_and_markers_are_recognised() {
        let matchers = GeneratedMatchers::builtins();
        let banner = format!("// @{} by build.rs", "generated");
        let expected = "\
Cargo.lock -> name
deep/nested/yarn.lock -> name
app/bundle.min.js -> name
src/build_out.rs -> marker
src/main.rs -> no
";
        wince::assert_eq!(
            classify(
                &matchers,
                &[
                    added_file("Cargo.lock", &["a = 1"]),
                    added_file("deep/nested/yarn.lock", &["a"]),
                    added_file("app/bundle.min.js", &["x"]),
                    added_file("src/build_out.rs", &[&banner, "fn main() {}"]),
                    added_file("src/main.rs", &["fn main() {}"]),
                ]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn a_marker_below_the_head_is_ordinary_content() {
        let matchers = GeneratedMatchers::builtins();
        let banner = format!("# @{}", "generated");
        let mut lines: Vec<String> = (1..=50).map(|n| format!("line {n}")).collect();
        lines[44] = banner;
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let expected = "notes.txt -> no\n";
        wince::assert_eq!(
            classify(&matchers, &[added_file("notes.txt", &refs)]),
            expected.to_string()
        );
    }

    #[test]
    fn config_extends_names_and_drops_a_builtin() {
        let names = vec!["generated/**/*.rs".to_string(), "!Cargo.lock".to_string()];
        let matchers = GeneratedMatchers::new(&names, &[]).unwrap();
        let expected = "\
Cargo.lock -> no
generated/api/models.rs -> name
src/main.rs -> no
";
        wince::assert_eq!(
            classify(
                &matchers,
                &[
                    added_file("Cargo.lock", &["a = 1"]),
                    added_file("generated/api/models.rs", &["struct A;"]),
                    added_file("src/main.rs", &["fn main() {}"]),
                ]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn config_extends_markers() {
        let markers = vec!["DO NOT EDIT".to_string()];
        let matchers = GeneratedMatchers::new(&[], &markers).unwrap();
        let expected = "\
gen.go -> marker
hand.go -> no
";
        wince::assert_eq!(
            classify(
                &matchers,
                &[
                    added_file("gen.go", &["// Code generated by protoc. DO NOT EDIT."]),
                    added_file("hand.go", &["package main"]),
                ]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn an_empty_marker_is_dropped_rather_than_matching_every_file() {
        let markers = vec![String::new()];
        let matchers = GeneratedMatchers::new(&[], &markers).unwrap();
        let expected = "any.txt -> no\n";
        wince::assert_eq!(
            classify(&matchers, &[added_file("any.txt", &["some content"])]),
            expected.to_string()
        );
    }

    #[test]
    fn a_single_star_stays_within_a_path_segment_but_a_double_star_crosses_it() {
        let single = name_report("src/*.rs", &["src/main.rs", "src/net/tcp.rs"]);
        wince::assert_eq!(
            single,
            "\
src/main.rs -> name
src/net/tcp.rs -> no
"
            .to_string()
        );
        let double = name_report("src/**/*.rs", &["src/main.rs", "src/net/tcp.rs"]);
        wince::assert_eq!(
            double,
            "\
src/main.rs -> name
src/net/tcp.rs -> name
"
            .to_string()
        );
    }

    #[test]
    fn a_character_class_matches_one_of_its_members_and_honors_negation() {
        let class = name_report("*.[ch]", &["main.c", "main.h", "main.o"]);
        wince::assert_eq!(
            class,
            "\
main.c -> name
main.h -> name
main.o -> no
"
            .to_string()
        );
        let negated = name_report("file[!0-9].txt", &["fileA.txt", "file7.txt"]);
        wince::assert_eq!(
            negated,
            "\
fileA.txt -> name
file7.txt -> no
"
            .to_string()
        );
    }

    #[test]
    fn an_invalid_glob_is_reported() {
        let message = match GeneratedMatchers::new(&["a[b".to_string()], &[]) {
            Ok(_) => "compiled".to_string(),
            Err(err) => err.to_string(),
        };
        wince::assert_eq!(
            message,
            "invalid generated-file name pattern \"a[b\": \
error parsing glob 'a[b': unclosed character class; missing ']'"
                .to_string()
        );
    }
}
