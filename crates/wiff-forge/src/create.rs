//! Naming the branch `push --create` publishes.
//!
//! Opening a pull request needs a head branch, and wiff names it from the
//! review's description title rather than asking the user to invent one. The
//! name is derived deterministically so the same review always publishes to the
//! same branch, and a suffix drawn from the session keeps two reviews whose
//! titles slugify alike from colliding on one remote branch.

use ulid::Ulid;

/// Turn a description title into a git branch name: lowercase, with each run of
/// characters that cannot appear in a readable branch name collapsed to a single
/// hyphen and the ends trimmed. A title that yields no usable characters (only
/// punctuation, say) returns an empty string, which the caller replaces with a
/// session-derived name.
pub fn branch_slug(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Append a suffix drawn from `session` to `slug`, for when the plain slug is
/// already taken by a different branch on the remote. The suffix is the tail of
/// the session's ULID, whose random component gives two same-titled reviews
/// distinct branches. An empty `slug` (from a title with no usable
/// characters) becomes the suffix alone.
pub fn disambiguated_branch(slug: &str, session: Ulid) -> String {
    let ulid = session.to_string();
    let suffix = ulid[ulid.len() - 8..].to_ascii_lowercase();
    if slug.is_empty() {
        suffix
    } else {
        format!("{slug}-{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_becomes_a_lowercase_hyphenated_slug() {
        wince::assert_eq!(branch_slug("Refactor the widget"), "refactor-the-widget");
    }

    #[test]
    fn punctuation_and_repeated_separators_collapse_to_single_hyphens() {
        wince::assert_eq!(
            branch_slug("  Fix: the parser (v2) -- again!  "),
            "fix-the-parser-v2-again"
        );
    }

    #[test]
    fn non_ascii_letters_are_dropped_rather_than_transliterated() {
        wince::assert_eq!(branch_slug("Cafe\u{301} au lait"), "cafe-au-lait");
    }

    #[test]
    fn a_title_with_no_usable_characters_yields_an_empty_slug() {
        wince::assert_eq!(branch_slug("--- !!! ---"), "");
    }

    #[test]
    fn a_disambiguated_branch_appends_the_ulid_tail() {
        let session = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        wince::assert_eq!(
            disambiguated_branch("refactor-the-widget", session),
            "refactor-the-widget-q69g5fav".to_string()
        );
    }

    #[test]
    fn a_disambiguated_empty_slug_is_the_suffix_alone() {
        let session = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        wince::assert_eq!(disambiguated_branch("", session), "q69g5fav".to_string());
    }
}
