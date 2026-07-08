#![allow(missing_docs)]

use wiff_diff::line::LineNo;
use wiff_diff::model::Side;
use wiff_diff::parse::parse;
use wiff_diff::reconstitute::{ReconLine, known_lines, reconstitute};

fn no(n: u32) -> LineNo {
    LineNo::new(n).unwrap()
}

const INPUT: &str = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -10,4 +10,4 @@
 ten
-eleven
+ELEVEN
 twelve
 thirteen
";

#[test]
fn reconstitutes_after_side_with_gaps() {
    let diff = parse(INPUT).unwrap();
    let file = &diff.files[0];
    let expected = vec![
        ReconLine::Gap { count: Some(9) },
        ReconLine::Known {
            lineno: no(10),
            text: "ten".to_string(),
        },
        ReconLine::Known {
            lineno: no(11),
            text: "ELEVEN".to_string(),
        },
        ReconLine::Known {
            lineno: no(12),
            text: "twelve".to_string(),
        },
        ReconLine::Known {
            lineno: no(13),
            text: "thirteen".to_string(),
        },
        ReconLine::Gap { count: None },
    ];
    k9::assert_equal!(reconstitute(file, Side::After), expected);
}

#[test]
fn known_lines_drop_gaps_per_side() {
    let diff = parse(INPUT).unwrap();
    let file = &diff.files[0];
    let before = vec![
        (no(10), "ten".to_string()),
        (no(11), "eleven".to_string()),
        (no(12), "twelve".to_string()),
        (no(13), "thirteen".to_string()),
    ];
    let after = vec![
        (no(10), "ten".to_string()),
        (no(11), "ELEVEN".to_string()),
        (no(12), "twelve".to_string()),
        (no(13), "thirteen".to_string()),
    ];
    k9::assert_equal!(known_lines(file, Side::Before), before);
    k9::assert_equal!(known_lines(file, Side::After), after);
}
