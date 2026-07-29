//! Deciding whether a file's raw bytes are line-oriented text.
//!
//! The diff model, its rendering, and comment anchoring are all line-oriented
//! text; a blob that is not text has no place in them. This is the one rule for
//! that decision, shared by every path that reads a file's bytes and must choose
//! whether to treat them as text or set them aside as binary.

/// Decode `bytes` as text, or return `None` when they are binary. Bytes holding
/// a NUL anywhere, or that are not valid UTF-8, are binary, matching how git
/// decides a blob is not textual. The whole content is examined: a NUL is valid
/// UTF-8, so a file that is otherwise clean text but embeds one is still binary.
pub fn decode_text(bytes: Vec<u8>) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}
