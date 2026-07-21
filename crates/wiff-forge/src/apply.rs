//! Running a planned push to completion against a forge.
//!
//! Planning ([`plan_push`]) decides one step of sendable writes as a pure
//! function of the folded review; this module performs those writes and folds
//! their results back into the log, re-planning until nothing remains. It exists
//! because a push cannot be computed up front: a reply's forge object depends on
//! its parent's, so the work unfolds only as earlier writes are published and
//! recorded. Each event is appended immediately after the forge call it records
//! succeeds; an interrupted standalone write leaves a single unlinked forge
//! object, while an interrupted batched review can leave several, both
//! reconciled by the next pull rather than re-sent.

use std::collections::HashSet;

use anyhow::{Result, anyhow};
use ulid::Ulid;
use wiff_core::ReviewState;
use wiff_core::comment::{link_event, sync_marker_event};
use wiff_core::description::synced_description;
use wiff_core::record::{Author, ForgeUrl, RecordBody, VerdictSyncRecord, comment_body_marker};
use wiff_core::session::{LockWait, SessionLog};

use crate::Unsupported;
use crate::push::{PushPlan, plan_push};

/// What a push accomplished, for the caller to report to the user.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    /// Local comments newly bound to a forge object this push created.
    pub created: Vec<Ulid>,
    /// Linked comments whose local body edit reached the forge.
    pub edited: Vec<Ulid>,
    /// Linked comments whose local resolution reached the forge.
    pub resolved: Vec<Ulid>,
    /// Whether the pusher's verdict was submitted.
    pub verdict_submitted: bool,
    /// Whether the description update reached the forge.
    pub description_published: bool,
    /// Writes the forge declined because it does not implement them.
    pub declined: Vec<DeclinedWrite>,
}

/// A write the forge could not apply because it lacks support for that
/// operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclinedWrite {
    /// A resolution change for a linked comment.
    Resolution(Ulid),
    /// The description update.
    Description,
}

/// Publish `author`'s local review to the forge pull request `pr`, folding each
/// write's result into `log` and re-planning until nothing remains.
///
/// The loop settles because every pass either records an event, which moves a
/// comment past the state that planned it (a created comment becomes linked, a
/// sent edit or resolve advances its marker), or marks an operation the forge
/// declined, which drops it from the next pass. Both shrink the work a re-plan
/// finds, and the review holds finitely many comments, so the passes are bounded.
pub async fn push(
    forge: &dyn crate::Forge,
    log: &mut SessionLog,
    pr: &ForgeUrl,
    author: &Author,
) -> Result<PushOutcome> {
    let mut driver = Push {
        forge,
        pr,
        author,
        outcome: PushOutcome::default(),
        declined_resolves: HashSet::new(),
        description_declined: false,
    };

    loop {
        let state = ReviewState::load(log.path())?;
        let plan = driver.actionable(plan_push(&state, author)?);
        if plan.is_empty() {
            break;
        }
        driver.run_plan(log, &plan).await?;
    }

    Ok(driver.outcome)
}

/// One run of the push loop against `forge` for pull request `pr` as `author`,
/// accumulating what published into `outcome` and remembering the operations the
/// forge declined this run.
struct Push<'a> {
    forge: &'a dyn crate::Forge,
    pr: &'a ForgeUrl,
    author: &'a Author,
    outcome: PushOutcome,
    /// Comment ULIDs whose resolution the forge declined this run.
    declined_resolves: HashSet<Ulid>,
    /// Whether the forge declined the description update this run.
    description_declined: bool,
}

impl Push<'_> {
    /// Drop from `plan` any writes already declined this run.
    fn actionable(&self, mut plan: PushPlan) -> PushPlan {
        plan.resolves
            .retain(|resolve| !self.declined_resolves.contains(&resolve.comment));
        if self.description_declined {
            plan.description = None;
        }
        plan
    }

    /// Perform one plan's writes in order and fold each result into the log:
    /// submit the batched review, post the standalone replies and fallbacks,
    /// then publish the edits, resolves, and description of already-linked
    /// comments.
    async fn run_plan(&mut self, log: &mut SessionLog, plan: &PushPlan) -> Result<()> {
        let (forge, pr, author) = (self.forge, self.pr, self.author);
        if let Some(review) = &plan.review {
            let submitted = forge.submit_review(pr, review).await?;
            // Bind every comment the review sent before recording any link. A
            // gap means the forge created the review but did not return an object
            // for one of its comments; leaving the bound ones unlinked and failing
            // here keeps the next pull able to reconcile the whole review by its
            // objects rather than a re-submit duplicating it.
            let mut links = Vec::with_capacity(review.comments.len());
            for outgoing in &review.comments {
                let forge_ref = submitted.comments.get(&outgoing.comment).ok_or_else(|| {
                anyhow!(
                    "review {} was submitted but did not bind comment {}; pull to reconcile it rather than pushing again",
                    submitted.review.id,
                    outgoing.comment
                )
            })?;
                links.push((outgoing.comment, forge_ref.clone(), &outgoing.body));
            }
            for (comment, forge_ref, body) in links {
                record(
                    log,
                    link_event(
                        comment,
                        author.clone(),
                        forge_ref,
                        comment_body_marker(body),
                    ),
                )?;
                self.outcome.created.push(comment);
            }
            if let Some(disposition) = review.disposition {
                record(
                    log,
                    RecordBody::VerdictSync(VerdictSyncRecord {
                        author: author.clone(),
                        disposition,
                    }),
                )?;
                self.outcome.verdict_submitted = true;
            }
        }

        for post in &plan.posts {
            let forge_ref = forge.post_comment(pr, post).await?;
            record(
                log,
                link_event(
                    post.comment,
                    author.clone(),
                    forge_ref,
                    comment_body_marker(&post.body),
                ),
            )?;
            self.outcome.created.push(post.comment);
        }

        for edit in &plan.edits {
            forge.edit_comment(&edit.at, &edit.body).await?;
            record(
                log,
                sync_marker_event(
                    edit.comment,
                    author.clone(),
                    Some(comment_body_marker(&edit.body)),
                    None,
                ),
            )?;
            self.outcome.edited.push(edit.comment);
        }

        for resolve in &plan.resolves {
            match forge.set_resolved(&resolve.at, resolve.resolved).await {
                Ok(()) => {
                    record(
                        log,
                        sync_marker_event(
                            resolve.comment,
                            author.clone(),
                            None,
                            Some(resolve.resolved),
                        ),
                    )?;
                    self.outcome.resolved.push(resolve.comment);
                }
                Err(err) if is_unsupported(&err) => {
                    self.declined_resolves.insert(resolve.comment);
                    self.outcome
                        .declined
                        .push(DeclinedWrite::Resolution(resolve.comment));
                }
                Err(err) => return Err(err),
            }
        }

        if let Some(description) = &plan.description {
            match forge.set_description(pr, description).await {
                Ok(()) => {
                    // Advance only the marker, not the content: a local edit
                    // made during the round-trip keeps its content and author
                    // while the marker still moves to what push sent.
                    record(log, synced_description(description.content_marker()))?;
                    self.outcome.description_published = true;
                }
                Err(err) if is_unsupported(&err) => {
                    self.description_declined = true;
                    self.outcome.declined.push(DeclinedWrite::Description);
                }
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }
}

/// Append `event` to the log, resyncing to the file's tail under the lock first
/// so a concurrent writer between passes cannot make the append diverge. The
/// lock is held only for the append, never across a forge call.
fn record(log: &mut SessionLog, event: RecordBody) -> Result<()> {
    let (mut lock, _records) = log.lock_and_sync(LockWait::Block)?;
    log.append(&mut lock, event)?;
    Ok(())
}

/// Whether `err` is the forge declining an operation it does not implement,
/// found through any `anyhow` context an adapter layered on top.
fn is_unsupported(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Unsupported>().is_some()
}
