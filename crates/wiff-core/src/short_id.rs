//! A short, creation-ordered identifier for the things wiff must name in a
//! terminal.
//!
//! A full ULID is 26 characters, which a person copying a session handle has to
//! type or paste whole. This identifier keeps a ULID's two useful properties, a
//! leading timestamp so ids sort in the order they were minted and an embedded
//! creation time, while dropping most of the width: its text form is nine
//! lowercase Crockford base32 characters. Uniqueness is a property of minting
//! rather than of the caller, so an id is safe to use where nothing external
//! guards against a repeat.

use std::sync::Mutex;

use time::OffsetDateTime;

/// Bits of the packed value spent on the timestamp. Thirty bits of seconds past
/// [`EPOCH_SEC`] span about thirty-four years, into the late 2050s, which is
/// ample for a local, human-scale tool. Seconds rather than milliseconds keep
/// the id short; ordering within a second falls to the tail.
const TIME_BITS: u32 = 30;

/// Bits of the packed value spent on the per-second tail. Fifteen bits
/// distinguish over thirty thousand ids minted within one second before the
/// tail overflows into the next second.
const TAIL_BITS: u32 = 15;

/// Characters of Crockford base32 the text form occupies, five bits each. The
/// timestamp and tail together are forty-five bits, so the encoding is exactly
/// this wide with no padding.
const WIDTH: usize = ((TIME_BITS + TAIL_BITS) / 5) as usize;

/// The instant the timestamp counts from: 2024-01-01T00:00:00Z, in Unix
/// seconds. A recent epoch keeps the timestamp inside [`TIME_BITS`] far longer
/// than the Unix epoch would.
const EPOCH_SEC: u64 = 1_704_067_200;

/// The largest tail value; the next increment past it advances the timestamp
/// instead.
const TAIL_MASK: u64 = (1 << TAIL_BITS) - 1;

/// The lowercase Crockford base32 alphabet: the digits and the consonants that
/// stay legible in a terminal, omitting `i`, `l`, `o`, and `u`.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A short, creation-ordered identifier. Ordered by its packed value, which is
/// the timestamp in the high bits and the tail in the low bits, so the natural
/// order and the lexical order of the text form both match creation order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct ShortId(u64);

impl ShortId {
    /// Mints a fresh id from the current time. Within one second the tail
    /// increments from the last id minted in this process rather than being
    /// drawn afresh, so a burst never repeats and never goes backwards even if
    /// the system clock does; a new second re-seeds the tail at random.
    ///
    /// Under the deterministic recording mode the id instead follows the fixed
    /// sequence the [`determinism`](crate::determinism) facility hands out.
    pub fn new() -> Self {
        if let Some(id) = crate::determinism::deterministic_short_id() {
            return id;
        }
        static LAST: Mutex<Option<(u64, u64)>> = Mutex::new(None);
        let now = now_sec();
        let mut last = LAST.lock().expect("short id clock");
        let (sec, tail) = match *last {
            Some((last_sec, last_tail)) if now <= last_sec => {
                let tail = last_tail + 1;
                if tail > TAIL_MASK {
                    (last_sec + 1, 0)
                } else {
                    (last_sec, tail)
                }
            }
            _ => (now, random_tail()),
        };
        *last = Some((sec, tail));
        Self::from_parts(sec, tail)
    }

    /// The instant this id was minted, decoded from its timestamp.
    pub fn minted_at(self) -> OffsetDateTime {
        let sec = EPOCH_SEC + (self.0 >> TAIL_BITS);
        OffsetDateTime::from_unix_timestamp(i64::try_from(sec).unwrap_or(i64::MAX))
            .expect("short id timestamp is a valid instant")
    }

    /// Builds an id whose timestamp is `at` and whose tail is `seq`, for the
    /// deterministic recording facility that needs a reproducible sequence.
    pub(crate) fn from_instant_and_seq(at: OffsetDateTime, seq: u64) -> Self {
        let sec = u64::try_from(at.unix_timestamp())
            .unwrap_or(0)
            .saturating_sub(EPOCH_SEC);
        Self::from_parts(sec, seq)
    }

    /// Packs a second offset past [`EPOCH_SEC`] and a tail into the value. A
    /// timestamp beyond [`TIME_BITS`] saturates at the maximum the encoding can
    /// hold rather than wrapping into the tail.
    fn from_parts(sec: u64, tail: u64) -> Self {
        let sec = sec.min((1 << TIME_BITS) - 1);
        Self(sec << TAIL_BITS | (tail & TAIL_MASK))
    }
}

impl Default for ShortId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ShortId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut text = [0u8; WIDTH];
        let mut value = self.0;
        for slot in text.iter_mut().rev() {
            *slot = ALPHABET[(value & 0x1f) as usize];
            value >>= 5;
        }
        // Every byte is drawn from the ASCII alphabet, so the buffer is UTF-8.
        f.write_str(std::str::from_utf8(&text).expect("crockford text is ascii"))
    }
}

/// Reports that a string is not a well-formed [`ShortId`]: the wrong length, or
/// a character outside Crockford base32.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a valid short id")]
pub struct ParseShortIdError(String);

impl std::str::FromStr for ShortId {
    type Err = ParseShortIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.len() != WIDTH {
            return Err(ParseShortIdError(text.to_string()));
        }
        let mut value = 0u64;
        for byte in text.bytes() {
            let digit = crockford_value(byte).ok_or_else(|| ParseShortIdError(text.to_string()))?;
            value = value << 5 | u64::from(digit);
        }
        Ok(Self(value))
    }
}

impl From<ShortId> for String {
    fn from(id: ShortId) -> Self {
        id.to_string()
    }
}

impl TryFrom<String> for ShortId {
    type Error = ParseShortIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

/// Seconds past [`EPOCH_SEC`] for the current instant, floored at zero for a
/// system clock set before the epoch.
fn now_sec() -> u64 {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    u64::try_from(now).unwrap_or(0).saturating_sub(EPOCH_SEC)
}

/// A random tail seeded from the platform's ULID randomness, reused here rather
/// than pulling in a second source of entropy.
fn random_tail() -> u64 {
    (ulid::Ulid::new().random() as u64) & TAIL_MASK
}

/// The value of a Crockford base32 digit, accepting the case-insensitive
/// alphabet and Crockford's aliases (`i`/`l` for `1`, `o` for `0`).
fn crockford_value(byte: u8) -> Option<u8> {
    match byte.to_ascii_lowercase() {
        b'0' | b'o' => Some(0),
        b'1' | b'i' | b'l' => Some(1),
        digit @ b'2'..=b'9' => Some(digit - b'0'),
        letter => {
            let index = ALPHABET.iter().position(|&c| c == letter)?;
            Some(index as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an id from explicit parts, the way minting does, for tests that
    /// pin a known timestamp and tail.
    fn id(sec: u64, tail: u64) -> ShortId {
        ShortId::from_parts(sec, tail)
    }

    #[test]
    fn text_form_is_nine_lowercase_characters() {
        let rendered = id(0, 0).to_string();
        wince::assert_eq!(rendered, "000000000".to_string());
        let rendered = id(1, 1).to_string();
        wince::assert_eq!(rendered, "000001001".to_string());
    }

    #[test]
    fn display_and_parse_round_trip() {
        let cases = [
            id(0, 0),
            id(1, 0),
            id(1_000, 42),
            id((1 << TIME_BITS) - 1, TAIL_MASK),
        ];
        let round_tripped: Vec<ShortId> = cases
            .iter()
            .map(|original| original.to_string().parse().expect("parses back"))
            .collect();
        wince::assert_eq!(round_tripped, cases.to_vec());
    }

    #[test]
    fn parse_accepts_crockford_aliases_case_insensitively() {
        let canonical: ShortId = "0000000hj".parse().expect("canonical parses");
        let aliased: ShortId = "OOOOOOOHJ".parse().expect("aliased parses");
        wince::assert_eq!(aliased, canonical);
    }

    #[test]
    fn parse_rejects_wrong_length_and_bad_characters() {
        let outcomes: Vec<Result<ShortId, ParseShortIdError>> =
            ["", "00000000", "0000000000", "00000000i", "00000000!"]
                .into_iter()
                .map(str::parse)
                .collect();
        let rendered: Vec<String> = outcomes
            .iter()
            .map(|outcome| match outcome {
                Ok(id) => format!("ok {id}"),
                Err(err) => err.to_string(),
            })
            .collect();
        wince::assert_eq!(
            rendered,
            vec![
                "\"\" is not a valid short id".to_string(),
                "\"00000000\" is not a valid short id".to_string(),
                "\"0000000000\" is not a valid short id".to_string(),
                // A trailing `i` is Crockford's alias for 1, so this one parses
                // and renders in the canonical alphabet as a trailing 1.
                "ok 000000001".to_string(),
                "\"00000000!\" is not a valid short id".to_string(),
            ]
        );
    }

    #[test]
    fn lexical_order_matches_creation_order() {
        let earlier = id(1_000, TAIL_MASK);
        let later = id(1_001, 0);
        assert!(earlier < later);
        assert!(earlier.to_string() < later.to_string());
        // Same second, later tail sorts after.
        let first = id(1_000, 1);
        let second = id(1_000, 2);
        assert!(first < second);
        assert!(first.to_string() < second.to_string());
    }

    #[test]
    fn timestamp_decodes_to_the_epoch_offset() {
        let epoch =
            OffsetDateTime::from_unix_timestamp(i64::try_from(EPOCH_SEC).expect("in range"))
                .expect("epoch is valid");
        wince::assert_eq!(id(0, 0).minted_at(), epoch);
        wince::assert_eq!(
            id(1_000, 0).minted_at(),
            epoch + time::Duration::seconds(1_000)
        );
    }

    #[test]
    fn minting_is_unique_and_monotonic_in_a_burst() {
        let ids: Vec<ShortId> = (0..10_000).map(|_| ShortId::new()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        // No two ids in the burst collide, and they were minted in order.
        wince::assert_eq!(sorted.len(), ids.len());
        wince::assert_eq!(ids, {
            let mut ascending = ids.clone();
            ascending.sort();
            ascending
        });
    }

    #[test]
    fn serde_uses_the_text_form() {
        let id = id(1_000, 42);
        let json = serde_json::to_string(&id).expect("serializes");
        wince::assert_eq!(json, "\"0000z801a\"".to_string());
        let parsed: ShortId = serde_json::from_str(&json).expect("deserializes");
        wince::assert_eq!(parsed, id);
    }
}
