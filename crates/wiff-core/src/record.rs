//! The record schema written to a session's append-only log.
//!
//! Every line of a session `.jsonl` is one [`Record`]: a sequence number, a
//! timestamp, and a tagged [`RecordBody`]. The `seq` is the record's 0-based
//! position in the file and its stable id within the session. Comment mutations
//! are append-only events under one [`CommentEvent`] envelope keyed by an
//! annotation [`Ulid`]; a comment's current state is the fold of its event
//! chain.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;
use wiff_diff::{FileStatus, LineNo, Side};

use crate::base_ruleset::BaseRuleset;
use crate::hash::SidebandHash;
use crate::identity::ScmType;

/// The session format version, bumped when the record schema changes
/// incompatibly. A log whose header version differs from this, older or newer,
/// is refused rather than misread.
pub const FORMAT_VERSION: u32 = 4;

/// A record's 0-based position in its session log and its stable id within the
/// session. The same integer is a comment's `created_seq`/`updated_seq` and the
/// ordering key a later phase uses for verdict derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// The underlying integer.
    pub fn get(self) -> u64 {
        self.0
    }

    /// The next sequence number after this one.
    pub fn next(self) -> Self {
        Seq(self.0 + 1)
    }
}

impl std::fmt::Display for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A comment's short, human-friendly handle within one review: its 1-based
/// position among the review's comments in the order they were created. Stable
/// for a committed comment because the log is append-only and withdrawals are
/// tombstones. It is scoped to a single review, not a cross-session identity;
/// the [`Ulid`] remains that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommentNumber(pub u32);

impl std::fmt::Display for CommentNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A comment named on the command line: either its full [`Ulid`] or its
/// review-scoped [`CommentNumber`]. Parse it via `FromStr`, which reads a `#N`
/// or bare-decimal number and a 26-character ULID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentRef {
    /// A full, cross-session ULID.
    Ulid(Ulid),
    /// A review-scoped number.
    Number(CommentNumber),
}

impl std::str::FromStr for CommentRef {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let text = text.trim();
        if let Some(digits) = text.strip_prefix('#') {
            return parse_comment_number(digits)
                .map(CommentRef::Number)
                .ok_or_else(|| format!("{text} is not a valid comment number"));
        }
        if let Ok(id) = Ulid::from_string(text) {
            return Ok(CommentRef::Ulid(id));
        }
        parse_comment_number(text)
            .map(CommentRef::Number)
            .ok_or_else(|| format!("{text} is not a comment number or ULID"))
    }
}

/// Read a comment number in its canonical decimal form, the grammar
/// [`CommentNumber`] displays: at least one ASCII digit, no sign, no leading
/// zero. Numbering is 1-based, so a leading zero (including a lone `0`) is
/// rejected, matching what `Display` can emit.
fn parse_comment_number(digits: &str) -> Option<CommentNumber> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.starts_with('0') {
        return None;
    }
    digits.parse::<u32>().ok().map(CommentNumber)
}

/// The number of a captured diff version, matching its sideband `vN.diff`. `v0`
/// is the first capture; each refresh increments it. A distinct type so a
/// version number is never confused with a sequence number or a raw count.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct VersionNumber(pub u32);

impl VersionNumber {
    /// The underlying integer.
    pub fn get(self) -> u32 {
        self.0
    }

    /// The next version number after this one.
    pub fn next(self) -> Self {
        VersionNumber(self.0 + 1)
    }
}

impl std::fmt::Display for VersionNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A commit that an scm resolved a rule or tip to, in that scm's own notation
/// (a git object name, a jj commit id), read and handed back verbatim without
/// parsing its interior.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(pub String);

impl RevisionId {
    /// Returns the revision text as the scm named it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RevisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A first-class change in an scm that has the concept, in that scm's own change
/// addressing, read and handed back verbatim. Distinct from a [`RevisionId`]: a
/// change survives the rewrites (amend, rebase) that give it a new commit, so it
/// names the moving change rather than one of its commits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeId(pub String);

impl ChangeId {
    /// Returns the change text as the scm named it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChangeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One line of a session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The record's 0-based position in the file and stable id.
    pub seq: Seq,
    /// When the record was appended.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The record's payload.
    pub body: RecordBody,
}

/// The payload of a [`Record`]. An unrecognized `type` deserializes to
/// [`RecordBody::Unknown`] rather than failing outright, so a line can still be
/// read; folding then rejects it as corrupt, since a matching-version log never
/// writes one.
// The common variant (`CommentEvent`) is the large one; the rarer `Unknown`,
// `Session`, and `DiffVersion` are small. Boxing the large variant would move
// the hot path to the heap to shrink a cold one, so the size spread is kept.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordBody {
    /// The session header, always the first record.
    Session(SessionHeader),
    /// A captured diff snapshot.
    DiffVersion(DiffVersionRecord),
    /// One event in a comment's history (create, edit, resolve, delete,
    /// re-anchor).
    CommentEvent(CommentEvent),
    /// A revision of the review's description.
    Description(DescriptionRecord),
    /// An unrecognized record type. A compatible-version log should never
    /// contain one, so folding rejects it as corrupt.
    #[serde(other)]
    Unknown,
}

/// The first record of a session: format version, project, and how the diff was
/// obtained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// The session's ULID (also its file name stem).
    pub ulid: Ulid,
    /// The record schema version.
    pub version: u32,
    /// The project bucket the session lives under.
    pub project: String,
    /// The repository root the project was derived from, if any.
    pub repo_root: Option<String>,
    /// The directory the session was created from.
    pub cwd: String,
    /// How the diff was captured, and whether it can be regenerated.
    pub source: SourceKind,
}

/// How a session's diff is obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceKind {
    /// A diff captured from a source-control system: a base and a tip rule,
    /// re-resolved against the repository on every refresh.
    Scm(ScmSource),
    /// A unified diff read from stdin.
    Stdin,
    /// A diff fetched from a forge pull request.
    Forge,
}

impl SourceKind {
    /// Whether a new diff version can be captured for this source.
    pub fn regenerable(&self) -> bool {
        match self {
            SourceKind::Scm(_) => true,
            // A stdin diff is a one-shot snapshot, and forge recapture is not
            // yet wired.
            SourceKind::Stdin | SourceKind::Forge => false,
        }
    }

    /// A short human-facing description of the source.
    pub fn describe(&self) -> String {
        match self {
            SourceKind::Scm(source) => format!("{} {}", source.scm, source.tip.describe()),
            SourceKind::Stdin => "stdin".to_string(),
            SourceKind::Forge => "forge".to_string(),
        }
    }
}

/// A source-control range to review: the scm it lives in, and the base and tip
/// rules that re-resolve to concrete commits on every refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmSource {
    /// The source-control system the range is resolved against.
    pub scm: ScmType,
    /// How the base of the reviewed range is resolved, re-evaluated each
    /// refresh.
    pub base: BaseRuleset,
    /// How the tip of the reviewed range is resolved.
    pub tip: TipRule,
}

/// How refresh re-resolves the tip of a reviewed range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum TipRule {
    /// The uncommitted working copy.
    Worktree,
    /// The staged index against its base. Git-specific.
    Index,
    /// A named branch or bookmark, re-resolved every refresh.
    Ref {
        /// The ref name to resolve.
        name: String,
    },
    /// A first-class change in an scm that has the concept, resolved to its
    /// current commit each refresh through that scm's native change addressing.
    ChangeId {
        /// The change to track.
        id: ChangeId,
    },
    /// A fixed revision that neither follows a ref nor tracks a change; it
    /// resolves to the same commit until that commit no longer exists.
    Pinned {
        /// The revision held under review.
        revision: RevisionId,
    },
}

impl TipRule {
    /// A one-word name for the tip, distinguishing the kinds of scm review in a
    /// listing or header.
    pub fn describe(&self) -> &'static str {
        match self {
            TipRule::Worktree => "worktree",
            TipRule::Index => "index",
            TipRule::Ref { .. } | TipRule::ChangeId { .. } | TipRule::Pinned { .. } => "revision",
        }
    }
}

/// A captured diff snapshot. The raw diff text lives in the sideband
/// `vN.diff`; this record holds a lightweight index plus its content hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffVersionRecord {
    /// The version number, matching the sideband `vN.diff`.
    pub number: VersionNumber,
    /// The hash of the sideband diff text.
    pub diff_hash: SidebandHash,
    /// The base commit this version was captured against, when the source has an
    /// authoritative base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<RevisionId>,
    /// The tip commit this version was captured at, when the source has an
    /// authoritative tip. Absent for a working-tree or index capture, whose tip
    /// is the uncommitted state rather than a commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_revision: Option<RevisionId>,
    /// A per-file index of the captured diff.
    pub files: Vec<FileSummary>,
}

/// A lightweight index entry for one file in a diff version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSummary {
    /// The before-side path.
    pub old_path: String,
    /// The after-side path.
    pub new_path: String,
    /// How the file changed.
    pub status: FileStatus,
    /// The number of hunks in the file.
    pub hunk_count: u32,
}

/// A revision of the review's description, appended whenever it is set or
/// edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DescriptionRecord {
    /// Who set this revision.
    pub author: Author,
    /// When the revision was authored, when that differs from when wiff recorded
    /// it: absent for a local edit, present for one mirrored from a forge.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub authored_at: Option<OffsetDateTime>,
    /// The forge object this description mirrors, when it came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExternalRef>,
    /// The upstream version this revision reconciled with, opaque and
    /// adapter-interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_marker: Option<String>,
    /// The description text this revision sets.
    #[serde(flatten)]
    pub description: Description,
}

/// A review's description: a one-line title and an optional body, the same shape
/// as a commit message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    /// The one-line title.
    pub title: String,
    /// The body below the title, empty for a title alone.
    #[serde(default)]
    pub body: String,
}

impl Description {
    /// Format the description as a commit message: the title, then the body
    /// below a blank line, or the title alone when the body is empty.
    pub fn to_message(&self) -> String {
        if self.body.is_empty() {
            self.title.clone()
        } else {
            format!("{}\n\n{}", self.title, self.body)
        }
    }

    /// Parse a commit message into a description: the first line is the title,
    /// the rest the body. Surrounding whitespace is normalized away, so a
    /// message padded with blank lines does not survive a round-trip through
    /// `to_message` unchanged.
    pub fn from_message(message: &str) -> Self {
        let message = message.trim();
        let mut parts = message.splitn(2, '\n');
        let title = parts.next().unwrap_or_default().trim().to_string();
        let body = parts.next().unwrap_or_default().trim().to_string();
        Self { title, body }
    }
}

/// Who authored an annotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// The author's display name.
    pub name: String,
    /// Whether the author is a human or an agent.
    pub kind: AuthorKind,
}

/// Whether an annotation's author is a human or an agent.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// A human reviewer.
    #[default]
    Human,
    /// An automated agent.
    Agent,
}

impl AuthorKind {
    /// The stable identifier for this author kind, matching its serialized form.
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthorKind::Human => "human",
            AuthorKind::Agent => "agent",
        }
    }
}

/// A forge instance: a provider family plus its normalized host, so two
/// installations of the same provider never collide. Kept as strings rather
/// than a closed enum so a new forge needs no schema change.
///
/// Unused until a later phase populates forge provenance; defined now so the
/// record schema is fixed at the format break.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForgeId {
    /// The provider family: "github", "gitlab", "codeberg", ...
    pub provider: String,
    /// The normalized host of this instance, so github.com and a self-hosted
    /// enterprise install are distinct: "github.com", "git.example.com".
    pub host: String,
}

/// The neutral class of forge object an [`ExternalRef`] names, translated per
/// adapter to the forge's own object types (a `ReviewComment` is a GitHub
/// review comment or a GitLab diff-note; a `Verdict` is a GitHub review
/// submission or a GitLab approval).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalKind {
    /// A review comment or discussion note.
    ReviewComment,
    /// The review description (a pull-request body or commit message).
    Description,
    /// A review-level verdict submission.
    Verdict,
}

/// A stable name for an object on a forge, kept opaque so a new forge needs no
/// schema change. `(forge, kind, id)` names the object; `url` is a presentation
/// link to it.
///
/// Unused until a later phase mirrors forge state; defined now so the record
/// schema is fixed at the format break.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalRef {
    /// The forge instance the object lives on.
    pub forge: ForgeId,
    /// What kind of object it is.
    pub kind: ExternalKind,
    /// The forge's identifier for the object, a string so it covers numeric ids
    /// and global-node ids alike.
    pub id: String,
    /// A link to the object, when the forge gives one. Presentation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// What an annotation is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum CommentTarget {
    /// A line range within a file, on one side of the diff.
    Lines {
        /// The file's display path.
        file: String,
        /// Which side the lines are on.
        side: Side,
        /// The first line of the range.
        start_line: LineNo,
        /// The last line of the range, inclusive.
        end_line: LineNo,
    },
    /// A whole file.
    File {
        /// The file's display path.
        file: String,
    },
    /// The review overall.
    Review,
    /// A reply to another comment, identified by its parent's id. A reply has no
    /// anchor of its own.
    Comment {
        /// The comment being replied to.
        id: Ulid,
    },
}

/// The captured content a line-range comment rebases against: the anchored
/// lines plus a window of surrounding context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// The exact text of the anchored lines.
    pub snippet: Vec<String>,
    /// Context lines immediately before the snippet.
    pub context_before: Vec<String>,
    /// Context lines immediately after the snippet.
    pub context_after: Vec<String>,
}

/// A reviewer's verdict on a comment: a sign-off or a request for
/// changes. A comment without one is a neutral remark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Signs off on the change.
    Approve,
    /// Asks for changes before the change is accepted.
    RequestChanges,
}

impl Disposition {
    /// The stable identifier for this verdict, matching its serialized form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Approve => "approve",
            Disposition::RequestChanges => "request_changes",
        }
    }
}

/// How confidently a comment was re-anchored onto a newer diff version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// The anchored lines were found unchanged.
    Exact,
    /// The anchored lines were found shifted or slightly altered.
    Approximate,
    /// The reviewed content itself could not be found, but its position was
    /// recovered by tracing the shared base across the two versions and mapping
    /// it back onto the new side. The comment names the place the reviewed code
    /// occupied, not that code.
    Relocated,
    /// The anchored lines could not be confidently located; the comment is
    /// retained but flagged.
    Outdated,
}

impl Confidence {
    /// The badge word shown for a re-anchored comment that needs a look, or
    /// `None` when it was found exactly and warrants no flag.
    pub fn flag(&self) -> Option<&'static str> {
        match self {
            Confidence::Exact => None,
            Confidence::Approximate => Some("shifted"),
            Confidence::Relocated => Some("moved"),
            Confidence::Outdated => Some("outdated"),
        }
    }
}

/// One event in a comment's history. `id` names the comment the event applies
/// to; a [`CommentEventKind::Create`] introduces that id. The metadata common
/// to every mutation lives here rather than on each variant, so an edit and a
/// re-anchor record who and when the same way a resolve and a delete do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentEvent {
    /// The comment this event applies to.
    pub id: Ulid,
    /// Who performed the event.
    pub author: Author,
    /// When the event was authored, when that differs from when wiff recorded
    /// it. Absent for a locally originated event, whose time is the enclosing
    /// [`Record::at`]; present for an imported event, whose authoritative time
    /// is its time on the originating forge.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub authored_at: Option<OffsetDateTime>,
    /// Where the event came from, for a mirrored review. Absent for a local
    /// event. Unpopulated until a later phase mirrors forge state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExternalRef>,
    /// The upstream version this event reconciled with, opaque and
    /// adapter-interpreted. Unpopulated until a later phase mirrors forge state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_marker: Option<String>,
    /// What the event does.
    #[serde(flatten)]
    pub kind: CommentEventKind,
}

/// What a [`CommentEvent`] does. A later phase adds `Link`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CommentEventKind {
    /// Introduce a new comment.
    Create(CommentCreate),
    /// Revise a comment's body.
    Edit {
        /// The new body text.
        body: String,
    },
    /// Change a comment's resolved state.
    Resolve {
        /// The new resolved state.
        resolved: bool,
    },
    /// Withdraw a comment (a tombstone).
    Delete,
    /// Set or clear the comment's verdict. Only the comment's author
    /// may write one; fold rejects one authored by anyone else.
    SetDisposition {
        /// The new verdict, or `None` to return the comment to neutral.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposition: Option<Disposition>,
    },
    /// Re-anchor a comment onto a newer diff version.
    Reanchor(CommentReanchor),
}

/// The initial state of a comment, as its create event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentCreate {
    /// What the comment is attached to.
    pub target: CommentTarget,
    /// The diff version it was authored against.
    pub version: VersionNumber,
    /// The captured content for rebasing, for line-range targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    /// The comment text.
    pub body: String,
    /// The comment's verdict from the moment it is created, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<Disposition>,
}

/// A re-anchoring of a comment onto a newer diff version. The comment's id lives
/// on the enclosing [`CommentEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentReanchor {
    /// The diff version it now anchors to.
    pub version: VersionNumber,
    /// Its target in the new version.
    pub target: CommentTarget,
    /// How confidently it was relocated.
    pub confidence: Confidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comment_reference_reads_a_number_with_or_without_a_hash_and_a_ulid() {
        let ulid = "00000000000000000000000042";
        let parsed: Vec<std::result::Result<CommentRef, String>> =
            ["3", "#3", " #3 ", ulid, "#nope", "not-a-ulid"]
                .into_iter()
                .map(str::parse)
                .collect();
        wince::assert_eq!(
            parsed,
            vec![
                Ok(CommentRef::Number(CommentNumber(3))),
                Ok(CommentRef::Number(CommentNumber(3))),
                Ok(CommentRef::Number(CommentNumber(3))),
                Ok(CommentRef::Ulid(
                    Ulid::from_string(ulid).expect("a valid ULID")
                )),
                Err("#nope is not a valid comment number".to_string()),
                Err("not-a-ulid is not a comment number or ULID".to_string()),
            ]
        );
    }

    #[test]
    fn a_comment_number_admits_only_its_canonical_decimal_form() {
        // A sign, a leading zero, or an inner space is not how a number ever
        // renders, so none of these read as a number. A lone 0 is rejected too:
        // numbering is 1-based, so #0 is not a value Display ever emits.
        let parsed: Vec<std::result::Result<CommentRef, String>> =
            ["+3", "-3", "0003", "3 5", "0", "#+3", "#03", "#0"]
                .into_iter()
                .map(str::parse)
                .collect();
        wince::assert_eq!(
            parsed,
            vec![
                Err("+3 is not a comment number or ULID".to_string()),
                Err("-3 is not a comment number or ULID".to_string()),
                Err("0003 is not a comment number or ULID".to_string()),
                Err("3 5 is not a comment number or ULID".to_string()),
                Err("0 is not a comment number or ULID".to_string()),
                Err("#+3 is not a valid comment number".to_string()),
                Err("#03 is not a valid comment number".to_string()),
                Err("#0 is not a valid comment number".to_string()),
            ]
        );
    }

    #[test]
    fn a_comment_number_displays_with_a_leading_hash() {
        wince::assert_eq!(CommentNumber(7).to_string(), "#7".to_string());
    }

    #[test]
    fn an_scm_source_round_trips_through_json() {
        let source = SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("parent(@)"),
            tip: TipRule::Ref {
                name: "HEAD".to_string(),
            },
        });
        let json = serde_json::to_string(&source).expect("serialize");
        wince::assert_eq!(
            json,
            r#"{"kind":"scm","scm":"git","base":"parent(@)","tip":{"rule":"ref","name":"HEAD"}}"#
                .to_string()
        );
        let back: SourceKind = serde_json::from_str(&json).expect("deserialize");
        wince::assert_eq!(back, source);
        wince::assert_eq!(source.regenerable(), true);
        wince::assert_eq!(source.describe(), "git revision".to_string());
    }

    #[test]
    fn a_description_round_trips_through_its_commit_message_form() {
        let cases = [
            (
                Description {
                    title: "Tidy the parser".to_string(),
                    body: "Split the lexer out.\n\nCover it with tests.".to_string(),
                },
                "Tidy the parser\n\nSplit the lexer out.\n\nCover it with tests.",
            ),
            (
                Description {
                    title: "Just a title".to_string(),
                    body: String::new(),
                },
                "Just a title",
            ),
        ];
        for (description, message) in cases {
            wince::assert_eq!(description.to_message(), message.to_string());
            wince::assert_eq!(Description::from_message(message), description);
        }
    }

    #[test]
    fn parsing_a_message_takes_the_first_line_as_the_title() {
        wince::assert_eq!(
            Description::from_message("  Only a subject line  "),
            Description {
                title: "Only a subject line".to_string(),
                body: String::new(),
            }
        );
        wince::assert_eq!(
            Description::from_message("Subject\nbody with no blank line"),
            Description {
                title: "Subject".to_string(),
                body: "body with no blank line".to_string(),
            }
        );
    }

    #[test]
    fn parsing_normalizes_carriage_returns_and_surrounding_blank_lines() {
        // A CRLF-terminated message (as piped stdin can be on Windows) leaves no
        // trailing carriage return on either part.
        wince::assert_eq!(
            Description::from_message("Subject\r\n\r\nbody\r\n"),
            Description {
                title: "Subject".to_string(),
                body: "body".to_string(),
            }
        );
        // Blank lines padding the body are stripped, so a body with its own
        // leading or trailing blank lines does not survive a round-trip: parsing
        // this message yields the same description as the un-padded body would.
        wince::assert_eq!(
            Description::from_message("Subject\n\n\n\nbody\n\n\n"),
            Description {
                title: "Subject".to_string(),
                body: "body".to_string(),
            }
        );
    }

    #[test]
    fn a_create_event_round_trips_through_json() {
        let event = CommentEvent {
            id: Ulid::nil(),
            author: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            authored_at: None,
            origin: None,
            synced_marker: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: CommentTarget::Review,
                version: VersionNumber(0),
                anchor: None,
                body: "looks good".to_string(),
                disposition: None,
            }),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        wince::assert_eq!(
            json,
            r#"{"id":"00000000000000000000000000","author":{"name":"wez","kind":"human"},"event":"create","target":{"target":"review"},"version":0,"body":"looks good"}"#
                .to_string()
        );
        let back: CommentEvent = serde_json::from_str(&json).expect("deserialize");
        wince::assert_eq!(back, event);
    }
}
