//! The review being edited: the diff, its committed comments, and the buffered
//! draft edits layered over them.
//!
//! A [`Review`] pairs the renderer and its inputs with a [`DraftBuffer`], so an
//! edit made in the TUI folds into the buffer and the next [`document`](Review::document)
//! reflects it without touching the session log. Rendering applies the drafts
//! over the committed comments and badges the ones that carry an uncommitted
//! change, so pending work is visible until it is committed.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::LineOrigin;
use wiff_core::draft::{DraftBuffer, EffectiveComment, draft_create};
use wiff_core::record::{
    Author, CommentTarget, Description, Disposition, RecordBody, Seq, VersionNumber,
};
use wiff_core::review::{CommentState, DescriptionState};
use wiff_diff::{Diff, LiveHighlighter, Side};

use crate::highlight::BackgroundHighlighter;
use crate::render::{
    BeforeOrigins, CommentOrigins, DescriptionBox, DiffView, Document, FileHighlights, ParsedFile,
    ReviewInputs, ViewLayout,
};

/// The id placed on the synthesized [`CommentState`] that renders the review's
/// description. The description is addressed as
/// [`BoxId::Description`](crate::render::BoxId), never by a comment id, so this
/// is a render-only placeholder that is never read for identity. Its base32
/// encoding contains the token `DESCR1PT10N` so that, should any code ever read
/// it, the value is visibly synthetic rather than shadowing a real comment.
fn description_render_id() -> Ulid {
    Ulid::from_string("0000000000DESCR1PT10N00000").expect("a valid synthetic ULID literal")
}

/// A comparison against an earlier version: the reference version whose after
/// content the presented before side shows, and, per file, the version and side
/// that before content came from so a comment placed on it anchors correctly.
struct Comparing {
    from: u32,
    before_origin: HashMap<String, LineOrigin>,
}

/// How a reload of committed comments differed from the set already shown, so a
/// reviewer can be told what another actor changed while they were reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommentSync {
    /// Comments that appeared that were not shown before.
    pub added: usize,
    /// Comments shown before whose content changed.
    pub changed: usize,
    /// Comments shown before that are gone, withdrawn by another actor.
    pub removed: usize,
    /// Whether the committed description differs from the one shown before.
    pub description_changed: bool,
}

impl CommentSync {
    /// Whether nothing changed, so there is nothing to report.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.changed == 0 && self.removed == 0 && !self.description_changed
    }

    /// Compare the previously shown comments to the freshly loaded set, matching
    /// by identity and counting a comment as changed when its latest event moved.
    fn between(before: &[CommentState], after: &[CommentState]) -> Self {
        let seen: HashMap<Ulid, Seq> = before.iter().map(|c| (c.id, c.updated_seq)).collect();
        let kept: HashSet<Ulid> = after.iter().map(|c| c.id).collect();
        let mut sync = CommentSync::default();
        for comment in after {
            match seen.get(&comment.id) {
                None => sync.added += 1,
                Some(&updated_seq) if updated_seq != comment.updated_seq => sync.changed += 1,
                Some(_) => {}
            }
        }
        sync.removed = before.iter().filter(|c| !kept.contains(&c.id)).count();
        sync
    }
}

/// A diff under review together with its committed comments and the buffered
/// edits layered over them.
pub struct Review {
    view: DiffView,
    diff: Diff,
    /// The parsed scope operations per file: the costly, theme-independent part
    /// of highlighting. `None` until its parse results have been computed.
    parsed: Vec<Option<ParsedFile>>,
    /// The colored highlight per file, colored from `parsed`. `None` until its
    /// parse results have been computed; a file with none yet renders plain.
    highlights: Vec<Option<FileHighlights>>,
    /// The pool parsing files in the background. Present when the review defers
    /// highlighting; absent when it highlights eagerly on construction, which
    /// enables more deterministic testing.
    highlighter: Option<BackgroundHighlighter>,
    /// The author newly drafted comments are attributed to.
    author: Author,
    /// The diff version drafted comments are authored against.
    version: u32,
    /// The live comments already persisted to the session log.
    committed: Vec<CommentState>,
    /// The description already persisted to the session log, or `None` when the
    /// review has none yet.
    committed_description: Option<DescriptionState>,
    /// The buffered, uncommitted edits over the committed comments.
    drafts: DraftBuffer,
    /// The active comparison against an earlier version, or `None` when the
    /// review shows the latest version's own diff against its baseline.
    comparing: Option<Comparing>,
}

impl Review {
    /// A review over `diff` with its already-committed `comments` and
    /// `description`, and no pending drafts. Comments authored in the TUI are
    /// attributed to `author` and anchored against diff `version`. The caller
    /// filters out withdrawn committed comments.
    pub fn new(
        view: DiffView,
        diff: Diff,
        author: Author,
        version: u32,
        comments: Vec<CommentState>,
        description: Option<DescriptionState>,
    ) -> Self {
        let mut review = Self {
            view,
            diff,
            parsed: Vec::new(),
            highlights: Vec::new(),
            highlighter: None,
            author,
            version,
            committed: comments,
            committed_description: description,
            drafts: DraftBuffer::new(),
            comparing: None,
        };
        review.highlight_eagerly();
        review
    }

    /// Build a review over `diff` that parses its syntax highlighting on
    /// background worker threads instead of on construction. Every file starts
    /// plain; the caller polls [`poll_highlights`](Review::poll_highlights) to
    /// fold in each parse as it completes.
    pub fn deferred(
        view: DiffView,
        diff: Diff,
        author: Author,
        version: u32,
        comments: Vec<CommentState>,
        description: Option<DescriptionState>,
    ) -> Self {
        let highlighter = BackgroundHighlighter::new(view.parser());
        let mut review = Self {
            view,
            diff,
            parsed: Vec::new(),
            highlights: Vec::new(),
            highlighter: Some(highlighter),
            author,
            version,
            committed: comments,
            committed_description: description,
            drafts: DraftBuffer::new(),
            comparing: None,
        };
        review.start_background();
        review
    }

    /// Recompute the highlights for the current diff the way this review is
    /// configured to: in the background when it has a pool, eagerly otherwise.
    fn rehighlight(&mut self) {
        if self.highlighter.is_some() {
            self.start_background();
        } else {
            self.highlight_eagerly();
        }
    }

    /// Parse and color every file now, on the calling thread, for a review that
    /// highlights eagerly.
    fn highlight_eagerly(&mut self) {
        let parsed = self.view.parse(&self.diff);
        self.highlights = parsed
            .iter()
            .map(|p| Some(self.view.recolor_file(p)))
            .collect();
        self.parsed = parsed.into_iter().map(Some).collect();
    }

    /// Clear the highlight caches to all-plain and hand the current diff to the
    /// background pool, abandoning any parse still running for an earlier diff.
    fn start_background(&mut self) {
        let count = self.diff.files.len();
        self.parsed = (0..count).map(|_| None).collect();
        self.highlights = (0..count).map(|_| None).collect();
        if let Some(highlighter) = self.highlighter.as_mut() {
            highlighter.start(Arc::new(self.diff.clone()));
        }
    }

    /// Fold any parse results that have arrived into the highlight cache,
    /// coloring each with the current theme, and return the indices of the files
    /// whose highlight just became ready.
    pub fn poll_highlights(&mut self) -> Vec<usize> {
        let ready = match self.highlighter.as_mut() {
            Some(highlighter) => highlighter.drain(),
            None => return Vec::new(),
        };
        let mut changed = Vec::with_capacity(ready.len());
        for parsed in ready {
            self.highlights[parsed.index] = Some(self.view.recolor_file(&parsed.parsed));
            self.parsed[parsed.index] = Some(parsed.parsed);
            changed.push(parsed.index);
        }
        changed
    }

    /// Whether files are still being parsed in the background.
    pub fn highlighting(&self) -> bool {
        self.highlighter
            .as_ref()
            .is_some_and(BackgroundHighlighter::in_progress)
    }

    /// Block until the next background parse result arrives or `timeout`
    /// elapses. Returns true if a background parse result arrived, false if the
    /// timeout was reached. If true is returned, the background parse result can
    /// be obtained by calling [`poll_highlights`](Review::poll_highlights).
    pub fn wait_for_highlight(&mut self, timeout: std::time::Duration) -> bool {
        self.highlighter
            .as_mut()
            .is_some_and(|highlighter| highlighter.wait(timeout))
    }

    /// An incremental highlighter for `token`'s syntax under the review's
    /// current theme, for coloring the inline editor as it is typed.
    pub fn live_highlighter(&self, token: &str) -> LiveHighlighter {
        self.view.live_highlighter(token)
    }

    /// Render the effective review -- the committed comments with the pending
    /// drafts applied -- into a fresh document, badging the comments that carry
    /// an uncommitted change. Comment bodies wrap to `layout`, and the diff
    /// content wraps too when the layout asks for it; a zero-width layout leaves
    /// everything unwrapped, for use before a real width is known.
    pub fn document(&self, layout: ViewLayout) -> Document {
        let effective = self.drafts.apply(&self.committed);
        let description = self.description_box();
        let comments: Vec<CommentState> = effective
            .iter()
            .map(|entry| entry.comment.clone())
            .collect();
        let pending: Vec<Ulid> = effective
            .iter()
            .filter(|entry| entry.pending)
            .map(|entry| entry.comment.id)
            .collect();
        let origins = self.comment_origins();
        let inputs = ReviewInputs {
            comments: &comments,
            pending: &pending,
            origins: &origins,
            describe_hint: description.is_none(),
            description: description
                .as_ref()
                .map(|(comment, pending)| DescriptionBox {
                    comment,
                    pending: *pending,
                }),
        };
        self.view
            .render_review_origins(&self.diff, inputs, &self.highlights, layout)
    }

    /// The description as a synthesized [`CommentState`] for rendering, paired
    /// with whether an uncommitted edit is buffered. `None` when no description
    /// is set. The synthesized comment's id is a [`description_render_id`]
    /// placeholder, never read: the description is addressed as
    /// [`BoxId::Description`](crate::render::BoxId), not by a comment id.
    fn description_box(&self) -> Option<(CommentState, bool)> {
        let effective = self
            .drafts
            .effective_description(self.committed_description.as_ref())?;
        let body = effective.content.to_message();
        let author = effective.author;
        let comment = CommentState {
            id: description_render_id(),
            author: author.clone(),
            target: CommentTarget::Review,
            version: VersionNumber(self.version),
            anchor: None,
            body,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: author,
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(0),
            updated_seq: Seq(0),
        };
        Some((comment, effective.pending))
    }

    /// Whether no description, committed or buffered, is set.
    pub fn description_absent(&self) -> bool {
        self.drafts
            .effective_description(self.committed_description.as_ref())
            .is_none()
    }

    /// How comments map onto the presented sides: the after side is always the
    /// review's own version, and the before side is either that version's
    /// baseline or, in a comparison, each file's recorded before origin.
    fn comment_origins(&self) -> CommentOrigins {
        let after = LineOrigin {
            version: self.version,
            side: Side::After,
        };
        let before = match &self.comparing {
            None => BeforeOrigins::Baseline(self.version),
            Some(comparing) => BeforeOrigins::PerFile(comparing.before_origin.clone()),
        };
        CommentOrigins::Origins { after, before }
    }

    /// Recolor the review to `theme`, replaying the parsed diff through the new
    /// palette so the next [`document`](Review::document) reflects it without
    /// re-parsing. Files still parsing in the background are colored with the
    /// new theme once their parse arrives. Leaves the review unchanged on an
    /// unknown syntax theme.
    pub fn set_theme(
        &mut self,
        theme: crate::theme::Theme,
    ) -> Result<(), wiff_diff::HighlightError> {
        self.view.set_theme(theme)?;
        self.highlights = self
            .parsed
            .iter()
            .map(|parsed| parsed.as_ref().map(|parsed| self.view.recolor_file(parsed)))
            .collect();
        Ok(())
    }

    /// Flip the resolved state of comment `id`, buffering the change.
    pub fn toggle_resolved(&mut self, id: Ulid) {
        let resolved = self
            .drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .is_some_and(|entry| entry.comment.resolved);
        self.drafts.resolve(id, !resolved, self.author.clone());
    }

    /// Cycle the verdict on comment `id` through none, approve, and request
    /// changes, buffering the change. A verdict is the comment author's own, so
    /// this does nothing unless the review's author authored the comment;
    /// returns whether the cycle applied.
    pub fn cycle_disposition(&mut self, id: Ulid) -> bool {
        let Some(entry) = self
            .drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
        else {
            return false;
        };
        if entry.comment.author != self.author {
            return false;
        }
        let next = match entry.comment.disposition {
            None => Some(Disposition::Approve),
            Some(Disposition::Approve) => Some(Disposition::RequestChanges),
            Some(Disposition::RequestChanges) => None,
        };
        self.drafts.set_disposition(id, next, self.author.clone());
        true
    }

    /// Toggle the deleted state of comment `id`, buffering the change, and
    /// return whether it is now deleted. A deleted comment stays in the review
    /// shown as withdrawn until it is committed or restored.
    pub fn toggle_deleted(&mut self, id: Ulid) -> bool {
        let deleted = self
            .drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .is_some_and(|entry| entry.comment.deleted);
        if deleted {
            self.drafts.restore(id);
        } else {
            self.drafts.delete(id, self.author.clone());
        }
        !deleted
    }

    /// Draft a new comment on `target` with `body`, attributed to this review's
    /// author and recorded against its diff version without an anchor, and return
    /// its identity.
    pub fn add_comment(&mut self, target: CommentTarget, body: String) -> Ulid {
        let (version, target) = self.anchor_target(target);
        let event = draft_create(
            self.author.clone(),
            target,
            VersionNumber(version),
            None,
            body,
        );
        self.drafts.add(event)
    }

    /// Whether comment `id` can take a reply: it is present in the effective
    /// set and not withdrawn. A withdrawn comment refuses a reply, matching the
    /// CLI authoring rule.
    pub fn can_reply(&self, id: Ulid) -> bool {
        self.drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .is_some_and(|entry| !entry.comment.deleted)
    }

    /// The version and target a comment authored on `target` at the cursor is
    /// anchored against. On the after side, and outside a comparison, that is the
    /// review's own version. On a comparison's before side it is the version and
    /// side that file's presented before content came from, so the comment
    /// rebases forward from the version it truly belongs to.
    fn anchor_target(&self, target: CommentTarget) -> (u32, CommentTarget) {
        if let Some(comparing) = &self.comparing
            && let CommentTarget::Lines {
                file,
                side: Side::Before,
                start_line,
                end_line,
            } = &target
            && let Some(origin) = comparing.before_origin.get(file)
        {
            return (
                origin.version,
                CommentTarget::Lines {
                    file: file.clone(),
                    side: origin.side,
                    start_line: *start_line,
                    end_line: *end_line,
                },
            );
        }
        (self.version, target)
    }

    /// Present `diff` with its comments placed by `before_origin`, the version
    /// and side each file's before content came from, so the review shows the
    /// changes since version `from` rather than the latest diff. Passing `None`
    /// returns to the latest version's own diff. The review's own version, its
    /// committed comments, and its drafts are untouched: only what is shown and
    /// where comments attach change.
    pub fn show_diff(
        &mut self,
        diff: Diff,
        comparison: Option<(u32, HashMap<String, LineOrigin>)>,
    ) {
        self.diff = diff;
        self.comparing = comparison.map(|(from, before_origin)| Comparing {
            from,
            before_origin,
        });
        self.rehighlight();
    }

    /// The reference version the review is comparing against, or `None` when it
    /// shows the latest version's own diff.
    pub fn comparing_from(&self) -> Option<u32> {
        self.comparing.as_ref().map(|comparing| comparing.from)
    }

    /// The latest captured version, which the review's after side always shows
    /// and which new comments anchor against.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Buffer a new `body` for comment `id`, attributed to the reviewer.
    pub fn edit_comment(&mut self, id: Ulid, body: String) {
        self.drafts.edit(id, self.author.clone(), body);
    }

    /// Buffer a new description parsed from the commit-message-shaped `body`,
    /// attributed to the reviewer.
    pub fn edit_description(&mut self, body: String) {
        self.drafts
            .edit_description(self.author.clone(), Description::from_message(&body));
    }

    /// The current body of comment `id` with pending drafts applied, for seeding
    /// an edit. Absent when no such comment is in the effective set.
    pub fn comment_body(&self, id: Ulid) -> Option<String> {
        self.drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .map(|entry| entry.comment.body)
    }

    /// The commit-message form of the current description with pending drafts
    /// applied, for seeding an edit. Empty when no description is set.
    pub fn description_body(&self) -> String {
        self.drafts
            .effective_description(self.committed_description.as_ref())
            .map(|description| description.content.to_message())
            .unwrap_or_default()
    }

    /// Whether any uncommitted draft edits are buffered, so the reviewer is
    /// warned before leaving that leaving without committing loses them.
    pub fn has_drafts(&self) -> bool {
        !self.drafts.is_empty()
    }

    /// The effective comments -- the committed comments with the buffered drafts
    /// applied -- each flagged whether it has an uncommitted change, in the order
    /// they were created.
    pub fn comment_states(&self) -> Vec<EffectiveComment> {
        self.drafts.apply(&self.committed)
    }

    /// How many effective comments are still open: neither resolved nor
    /// withdrawn, with pending drafts applied so the tally follows the
    /// reviewer's uncommitted edits.
    pub fn open_comments(&self) -> usize {
        self.drafts
            .apply(&self.committed)
            .into_iter()
            .filter(|entry| !entry.comment.resolved && !entry.comment.deleted)
            .count()
    }

    /// Recapture the review over `diff` as version `version`, replacing the diff
    /// and its committed `comments` and rebasing pending drafts forward onto it.
    /// A drafted line comment moves through `old_diff`, which yields the parsed
    /// diff a draft was authored against so its anchored lines can be relocated.
    pub fn refresh(
        &mut self,
        diff: Diff,
        comments: Vec<CommentState>,
        version: u32,
        mut old_diff: impl FnMut(u32) -> wiff_core::Result<Diff>,
    ) -> wiff_core::Result<()> {
        self.drafts
            .rebase(VersionNumber(version), &diff, |v| old_diff(v.get()))?;
        self.diff = diff;
        // A refresh moves to the new latest diff, ending any active comparison.
        self.comparing = None;
        self.rehighlight();
        self.committed = comments;
        self.version = version;
        Ok(())
    }

    /// Take the buffered drafts as the append events that persist them, emptying
    /// the buffer. Used at commit time when the reviewer keeps the session.
    pub fn take_drafts(&mut self) -> Vec<RecordBody> {
        std::mem::take(&mut self.drafts).into_records()
    }

    /// The append events that would persist the buffered drafts, without
    /// emptying the buffer.
    pub fn draft_records(&self) -> Vec<RecordBody> {
        self.drafts.clone().into_records()
    }

    /// Empty the draft buffer (called once its records are durably committed).
    pub fn clear_drafts(&mut self) {
        self.drafts = DraftBuffer::new();
    }

    /// Replace the committed comments and `description` with the freshly folded
    /// live state after the pending drafts were persisted. The caller has
    /// already cleared the drafts, so the review now reflects them as committed.
    pub fn set_committed(
        &mut self,
        comments: Vec<CommentState>,
        description: Option<DescriptionState>,
    ) -> CommentSync {
        let mut sync = CommentSync::between(&self.committed, &comments);
        sync.description_changed = self.committed_description != description;
        self.committed = comments;
        self.committed_description = description;
        sync
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use wiff_core::record::{Author, AuthorKind, CommentTarget, Seq, VersionNumber};

    use super::{CommentState, CommentSync};

    /// A committed comment with identity `id` whose most recent event is `seq`,
    /// the only fields the sync tally compares.
    fn committed(id: u128, updated_seq: u64) -> CommentState {
        let author = Author {
            name: "agent".to_string(),
            kind: AuthorKind::Agent,
        };
        CommentState {
            id: ulid::Ulid(id),
            author: author.clone(),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: "note".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: author,
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(updated_seq),
            updated_seq: Seq(updated_seq),
        }
    }

    #[test]
    fn a_sync_tally_counts_added_changed_and_removed_comments() {
        // Against a set holding comments 1 and 2, a reload that keeps 1 as it
        // was, advances 2 to a later event, and introduces 3 counts one added
        // and one changed; comment 1 is unchanged and comment 2 is not removed.
        let before = vec![committed(1, 5), committed(2, 5)];
        let after = vec![committed(1, 5), committed(2, 9), committed(3, 1)];
        wince::assert_eq!(
            CommentSync::between(&before, &after),
            CommentSync {
                added: 1,
                changed: 1,
                removed: 0,
                description_changed: false,
            }
        );

        // Dropping comment 2 from the reloaded set counts as one removed, and an
        // identical reload reports nothing at all.
        wince::assert_eq!(
            CommentSync::between(&before, &[committed(1, 5)]),
            CommentSync {
                added: 0,
                changed: 0,
                removed: 1,
                description_changed: false,
            }
        );
        wince::assert_eq!(CommentSync::between(&before, &before).is_empty(), true);
    }

    /// Block until `review` finishes parsing every file in the background,
    /// folding in each result as it arrives.
    fn finish_highlighting(review: &mut super::Review) {
        while review.highlighting() {
            assert!(
                review.wait_for_highlight(std::time::Duration::from_secs(30)),
                "timed out waiting for a background parse result"
            );
            review.poll_highlights();
        }
    }

    #[test]
    fn a_deferred_review_opens_plain_and_colors_in_to_match_an_eager_one() {
        use wiff_diff::{Diff, FileStatus, LineKind};

        use crate::render::ViewLayout;
        use crate::render::testutil::{dump, file, theme};

        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Added, "let y = 2;", 2),
                ],
            )],
        };
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let view = || crate::render::DiffView::new(theme()).expect("view");
        let layout = ViewLayout::default();

        // A deferred review opens with every file plain, before any parse arrives.
        let mut deferred =
            super::Review::deferred(view(), diff.clone(), author.clone(), 0, Vec::new(), None);
        let plain = dump(&deferred.document(layout).lines);
        #[rustfmt::skip]
        wince::snapshot_str!(
            plain,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#c0c5ce|-|->let x = 1;\n",
            "<#9ea1a9|#414a4a|->        2 + <#c0c5ce|#414a4a|->let y = 2;\n",
        );

        // Once every file's parse arrives, the deferred review colors in to the
        // exact same document an eager review produces up front.
        finish_highlighting(&mut deferred);
        let eager = super::Review::new(view(), diff, author, 0, Vec::new(), None);
        wince::assert_eq!(
            dump(&deferred.document(layout).lines),
            dump(&eager.document(layout).lines)
        );
    }
}
