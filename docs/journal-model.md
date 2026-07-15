# wiff journal model evolution

This note records the next iteration of wiff's on-disk journal model. It builds
on the record schema described in `spec.md` (the `Record { seq, at, body }`
append-only log folded into a `ReviewState`) and extends it to cover five
things the current model does not:

- discussion threads (replies to a comment),
- an actor and time on every event, not just create/resolve/delete,
- an author-written description (the PR title and body equivalent),
- an approve / request-changes disposition on any comment,
- mirroring a forge pull request (GitHub first, GitLab and Codeberg later),
  including importing its comment history and publishing local comments back.

It also revises how a git change is identified so that `wiff refresh` keeps
tracking the same logical change after an amend, a rebase, or added commits.

The schema is not yet stable and there is currently a single user who removes a
session soon after a round of work, so this note assumes we may break the
on-disk format freely and bump `FORMAT_VERSION` whenever a change alters it.
There is no migration path and no compatibility shim in either direction: `fold`
requires the log's version to match the binary exactly and rejects any other,
older or newer, directing the user to discard the session and re-capture rather
than risk reading an old log under new semantics. Within a matching version,
`fold` keeps its strict stance that an unrecognized record is corrupt.

## Guiding principles

- **Event-sourced.** The log stays append-only; current state is the fold of an
  event chain. Every new concept is a new record type or a new field folded in,
  never an in-place mutation.
- **Forge-neutral core.** Nothing in `wiff-core` names GitHub. Forge-specific
  identifiers, positions, and verbs live in per-forge adapters behind the
  existing async `DiffSource` (plus a future publish trait). The test: adding
  Codeberg should be a new adapter and a new forge-id string, touching no record
  schema. If a forge feature would force a new field on a core record, the
  neutral shape is wrong.
- **One place per concept.** A `u64` that is actually a generation, a string
  that is actually an external id: introduce a newtype and document it once,
  rather than re-explaining it at each field. This note assumes several such
  newtypes rather than bare primitives:

  - `Seq(u64)`: a record's position in the session log and stable id within it,
    used by `Record.seq` and the `created_seq` / `updated_seq` on a folded
    comment.
  - `VersionNumber(u32)`: which captured diff version a comment was authored
    against, an index into the session's `DiffVersion` sequence.
  - `RevisionId(String)`: an opaque identifier for a captured endpoint state (a
    git or forge commit sha today, but not assumed to be a sha or even a hash).
  - `ChangeId(String)`: a stable identity for a logical change independent of
    its current revision (a jj change id, a `Change-Id:` trailer value, or one
    wiff minted).
  - `ForgeId(String)` and `ScmId(String)`: open string labels naming a forge
    ("github", "gitlab", "codeberg") or a source-control system ("git", "jj",
    "hg"), kept as strings so a new one needs no schema change.

## The comment event envelope

Today each comment mutation is its own record body (`CommentEdit`,
`CommentResolve`, `CommentDelete`, `CommentReanchor`), and they disagree about
metadata: `author` is present on resolve and delete but missing on edit and
reanchor, and the mutation time is only recoverable from the enclosing
`Record.at`. That drift is the root of several gaps here.

Unify every comment mutation under one envelope with the metadata common to all
of them:

```rust
/// One event in a comment's history. The `id` names the comment the event
/// applies to; a create event introduces that id.
pub struct CommentEvent {
    /// The comment this event applies to.
    pub id: Ulid,
    /// Who performed the event.
    pub author: Author,
    /// When the event was authored, when that differs from when wiff recorded
    /// it. Absent for a locally originated event, whose time is the enclosing
    /// `Record.at`. Present for an imported event, whose authoritative time is
    /// its time on the originating forge.
    pub authored_at: Option<OffsetDateTime>,
    /// Where the event came from, for a mirrored review. Absent for a local
    /// event.
    pub origin: Option<ExternalRef>,
    /// The upstream version this event reconciled with, opaque and
    /// adapter-interpreted (an `updated_at` or etag). Set by a push and by
    /// each imported edit or resolve; absent for a purely local event.
    pub synced_marker: Option<String>,
    /// What the event does.
    pub kind: CommentEventKind,
}

pub enum CommentEventKind {
    Create(CommentCreate),
    Edit { body: String },
    Resolve { resolved: bool },
    Delete,
    Reanchor(CommentReanchor),
    SetDisposition { disposition: Option<Disposition> },
    /// Bind a locally-authored comment to the forge object created for it.
    /// `forge_ref` is the object the publish returned, distinct from the
    /// envelope's `origin`: `origin` is an event's own provenance (absent for a
    /// local event), while this names the forge object a local comment was
    /// published as. Folds to `CommentState.origin`; changes nothing the
    /// reviewer wrote.
    Link { forge_ref: ExternalRef },
}

/// The initial state of a comment, as its create event. `target` records a
/// reply via `CommentTarget::Comment`; a disposition set here is the comment's
/// opening verdict, revised later by `SetDisposition`.
pub struct CommentCreate {
    pub target: CommentTarget,
    /// The diff version the comment was authored against.
    pub version: VersionNumber,
    /// The diff position the comment is anchored to. Absent for a reply, which
    /// takes its position from the parent named in `target`.
    pub anchor: Option<Anchor>,
    /// The revision the anchor was captured against: the version's head for a
    /// comment authored in the ordinary view, an intermediate commit when
    /// authored against one commit of the range. Reanchoring moves it forward to
    /// the latest head; publish-back uses it as the forge's `commit_id`.
    pub authored_revision: Option<RevisionId>,
    pub body: String,
    pub disposition: Option<Disposition>,
}
```

`Session` and `DiffVersion` stay outside the envelope; they have no author.
`ExternalRef` and `Disposition` are defined below.

A comment acquires its `origin` one of two ways. An imported comment brings the
`ExternalRef` on its `Create` envelope, and fold sets `CommentState.origin` from
there. A locally-authored comment starts with no `origin`; publishing it creates
the forge object and appends a `Link` event bearing the returned `ExternalRef`,
and fold sets `origin` from that. Either way, `origin` is immutable once set: a
later `Link`, or an imported event whose `origin` differs from the object the
comment is already bound to, is a corrupt log, not a silent re-point, because
re-sync deduplicates on the bound reference and a moved binding would split or
merge histories. `fold` rejects a `Link` against an unknown comment as fatal
(the same stance as any mutation against an unknown comment), allows one against
a tombstoned comment, and rejects a `Link` whose `ExternalRef` is already bound
to a different comment, so one forge object is never owned by two comments. A
genuinely mistyped or migrated reference is corrected by discarding and
re-importing, not by re-pointing a live binding.

The `synced_marker` on the envelope, updated by a push
and by each imported edit or resolve, folds to `CommentState.synced_marker`,
which re-sync reads to tell an unchanged upstream object from a genuinely
edited one.

Two consequences fold straight into `CommentState`:

- **Actor on every event.** Edit and reanchor now record who did it, folded to
  `updated_by` alongside `updated_at`, so a reader shows the actor behind the
  most recent change even when it was not the original author (`resolved_by` and
  `deleted_by` still name those specific acts). Reanchor is
  machine-driven (the rebaser during a refresh), so its author is whoever ran
  the refresh: the human who invoked `wiff refresh` directly, or the agent that
  ran it.
- **Time on every event.** `fold` populates per-event timestamps
  (`created_at`, `resolved_at`, `deleted_at`, `updated_at`) from `authored_at`
  when present, otherwise from `Record.at`. The earlier idea of inferring event
  time from a ULID is dropped: a mutation never had its own ULID, and an
  imported event's local ULID would be the import time, not the authored time.

`CommentState` grows the folded fields:

```rust
pub struct CommentState {
    // ... existing id, author, target, version, anchor, body ...
    /// The revision the anchor was captured against; an intermediate commit when
    /// authored in the per-commit view, the version's head otherwise.
    pub authored_revision: Option<RevisionId>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// Who made the most recent change of any kind (edit, reanchor, resolve,
    /// delete, disposition). The original author stays in `author`.
    pub updated_by: Author,
    pub resolved: bool,
    pub resolved_by: Option<Author>,
    pub resolved_at: Option<OffsetDateTime>,
    pub deleted: bool,
    pub deleted_by: Option<Author>,
    pub deleted_at: Option<OffsetDateTime>,
    pub disposition: Option<Disposition>,
    pub reply_to: Option<Ulid>,
    pub origin: Option<ExternalRef>,
    pub synced_marker: Option<String>,
    pub confidence: Option<Confidence>,
    pub created_seq: Seq,
    pub updated_seq: Seq,
}
```

Any actor may edit, resolve, or delete any comment; fold records who did it and
refuses nothing on the grounds of authorship. The log is a cooperative local
artifact shared by a human and their agents, not an adversarial multi-tenant
store, and wiff has no identity to authenticate a writer against, so an
enforcement rule would buy nothing and would block the ordinary case of an agent
tidying a human's note or the reverse. The one exception is `SetDisposition`,
which fold still requires to come from the target comment's creator: a verdict
means its author's stance, so a disposition written by anyone else is a corrupt
log rather than a permitted edit.

## Replies and threads

A reply is a comment whose target is another comment. Model it by reusing the
comment machinery rather than a parallel record type: a reply is edited,
resolved, deleted, and folded exactly like any comment.

Add a target variant:

```rust
pub enum CommentTarget {
    Lines { file: String, side: Side, start_line: LineNo, end_line: LineNo },
    File { file: String },
    Review,
    /// A reply to another comment. The reply inherits the parent's anchor; it
    /// has no anchor of its own.
    Comment { id: Ulid },
}
```

The parent pointer names the specific comment being replied to, not a
pre-flattened thread root. The flat display order the TUI shows is a rendering
choice; the precise pointer is what a forge's `in_reply_to` maps to, so
preserving it keeps publish-back faithful and loses nothing. Thread membership
is reconstructed at fold time by walking `reply_to` to a root. Within a thread
the flattened replies order by their comment ULID, which encodes creation time
and is globally unique, so the same events yield the same order on every machine
regardless of fold or arrival order. `authored_at` is display metadata, not the
sort key, since it can be absent or skewed; a reply authored offline sorts by
its ULID, where it was created, not when it merged.

`fold` validates that a reply references a comment it has already folded a
create for, the same way `require_comment` treats a missing target as a corrupt
log. "Already folded a create" is what existence means here: a later `Delete` on
the parent does not un-exist it (deletes are tombstones, so the id is still
known, and a reply may sit under a deleted parent, shown as a tombstone). This
makes `fold` single-pass and order-dependent: an event that references a comment
must appear after that comment's create in the log. The local command path
guarantees this by construction (you cannot reply to a comment that does not yet
exist), and the forge importer emits parents before their replies, which the
chronological order forge APIs return already gives. If a future source cannot
promise that order, the fix is a two-pass fold (gather ids, then resolve
references), not a relaxation of the corruption stance.

**Resolve is per-comment** (decided). There is no separate thread-resolve
concept. A forge that resolves whole threads (GitHub, GitLab discussions) has
its adapter reconcile our per-comment resolve against the forge's coarser
thread resolve. The mapping keys on the thread's aggregate resolved and blocking
state, not on any single resolve: the adapter resolves the forge thread only
once every local comment in it is resolved and no actor's current verdict
remains RequestChanges, and reopens it whenever a comment is reopened or an
objection returns. Reopening is independent of verdicts, so a thread of only
neutral notes still reopens upstream when a local comment goes back to
unresolved, not just when something blocks; imported thread resolution is
mirrored by marking the thread's local comments resolved.
This matters when actors disagree within a thread. If Alice objects on the root
and Bob objects on a reply, Bob withdrawing his own reply couples to clearing
only Bob's verdict; Alice's still stands, the thread is still blocking, and the
forge thread stays open. Bob's own-reply resolve is local until the last
objection lifts, so local blocking state and the forge's resolved state never
disagree.

Who resolves decides whether the verdict moves with it. When someone other than
the objecting author resolves a thread (typically the change author, signalling
"I believe my edit fixed this"), it is housekeeping: it records who and when but
leaves every verdict untouched, so a block stands until its own author lifts it.
When the objecting author resolves their own thread, resolving means
"withdrawn," so it clears their verdict. This coupling is derived at fold, not
written as a second record: `fold` reads the objecting author's own resolve as
clearing that author's verdict for the target, so the resolve stays a single
append that cannot be left half-done (see Durability and recovery). This is the one
path that clears a block, matching the rule that only the objecting author
reduces their own. Because the cleared verdict is derived from the resolve rather
than stored beside it, the disposition can never silently disagree with the
resolved state.
Withdrawing is neutral, not endorsement; turning a block into an Approve stays a
separate explicit act.

## Description

A review has an author-written description, equivalent to a PR title plus body,
or a single commit's message. It is distinct from reviewer commentary: it is the
author's statement of intent.

```rust
pub struct Description {
    pub title: String,
    pub body: String,
}
```

Title plus body is the shape every forge and a commit message share: a commit
subject becomes the title and the remainder the body; a GitHub PR has title and
body; a GitLab MR has title and description. We keep it freeform, not structured
into named fields (Phabricator's Summary / Test Plan / Reviewers), because
GitHub, GitLab, and Codeberg bodies are all freeform markdown, and the neutral
shape is what travels.

One serialization to a commit message is fixed so every writer produces the same
text: the title, then a blank line, then the body, with no trailing blank line;
an empty body yields the title alone with no separator. A commit message read in
(stdin front matter, `--change`'s default) inverts it, the subject to the title
and the remainder after the blank line to the body. Identity trailers
(`Change-Id:`, `Differential Revision:`) are parsed from and written to the
body's final trailer block, so `wiff commit` and `wiff amend` find and preserve
them there rather than inventing a second convention.

The description is mutable (the author edits it; re-syncing a mirrored PR
updates it upstream), so it is a foldable event, not a field baked into the
immutable `SessionHeader`:

```rust
pub enum RecordBody {
    // ... Session, DiffVersion, CommentEvent ...
    /// Set or revise the review description. The last one folded wins.
    Description(DescriptionRecord),
}

pub struct DescriptionRecord {
    pub author: Author,
    pub authored_at: Option<OffsetDateTime>,
    pub origin: Option<ExternalRef>,
    /// The upstream last-modified marker when this came from a forge, the same
    /// `updated_at`-or-etag that comment events hold. Re-sync reads the folded
    /// value to tell an unchanged upstream description from an edited one.
    pub synced_marker: Option<String>,
    pub description: Description,
}
```

`ReviewState` gains `description: Option<Description>` plus its author, time,
folded `origin`, and folded `synced_marker`. A mirrored description is attributed
to the PR author with its upstream time; a locally authored one is a local event.
This reuses the same author, time, provenance, and sync-marker machinery as
comment events rather than inventing a parallel one, so re-sync detects an
upstream description edit the same way it detects an edited comment.

A local `wiff describe` after an imported description works like editing a linked
comment: the new record's `origin` is None and its `synced_marker` does not
advance, but the folded `origin` sticks from the earlier imported record, so the
description stays bound to the pull request body while its folded content now
differs from the last-synced marker. That difference is exactly what push reads
to publish the changed body, and what a later pull reads to leave the pending
local edit in place rather than overwrite it with an unchanged upstream value.

## Disposition

An optional disposition expresses a verdict on any comment:

```rust
pub enum Disposition {
    Approve,
    RequestChanges,
}
```

Absence is a neutral note. One field serves both cases we want:

- On a **review-level** comment it is the review verdict, mapping directly to a
  forge's review state (GitHub `APPROVE` / `REQUEST_CHANGES`, `COMMENT` when
  absent; GitLab approve / unapprove).
- On a **line or file** comment, `RequestChanges` means blocking and absence
  means a non-blocking note (a nit). This is the blocking / non-blocking
  distinction reviewers want on an individual comment, without a second axis.

Disposition folds like resolve: the `CommentEventKind::SetDisposition` event
sets it, an author's latest wins, and it may also be set on the create event.
Because a comment (root or reply) has one author, the disposition on it is that
author's verdict. This holds only because a disposition event must be authored
by the comment's creator: `fold` treats a `SetDisposition` whose author differs
from the comment it targets as a corrupt log, the same `InconsistentLog` stance
as a mutation against an unknown comment, so no one can clear or forge another
actor's verdict by writing onto their comment. An author walks back an earlier
blocking verdict retroactively by appending a new disposition to their own
comment; the new one supersedes only their own prior verdict and touches no
other author's.

**Verdicts are per actor; there is no global approved/blocked bit.** Each
`(target, actor)` pair has at most one current verdict, the actor's latest.
The per-comment `disposition` is the raw material; the per-actor verdict is
derived, not separately stored. A target is a thread (its root and replies) for
a line or file comment, or the review as a whole for review-level comments.
Within a target, collect every disposition-bearing event by a given author --
a Create that set a disposition, a later `SetDisposition`, or the author's
own `Resolve` of a comment then holding their RequestChanges, which counts as
clearing it (the withdraw coupling) -- across all their comments in the target,
and take the one with the greatest log sequence: that event's disposition is the
actor's current verdict. Deriving the withdrawal from the resolve itself, rather
than a separately appended `SetDisposition(None)`, is what lets the objecting
author's resolve be a single append that cannot be left half-done. Ordering by the disposition
event's `Seq`, not by the comment's ULID, is what makes a `SetDisposition` that
revises an older comment outrank a newer comment the same author left without
one; a comment's ULID fixes only where its replies sort, never whose verdict
is current. An actor is identified by its local `Author` key, the kind and name;
the forge `handle`, when present, is provenance, not the grouping key. wiff
assumes the local human whose name is the invoking `$USER` is the same person as
the forge account the push token authenticates as, so on import the local user's
own forge-authored events fold onto their local identity rather than becoming a
separate actor. That assumption is what lets a reviewer update a verdict they
first made on the forge; other forge handles stay distinct imported actors,
shown but not locally editable. Two reviewers can hold an Approve and a
RequestChanges on the same target without conflict, and the native UI shows the
full per-actor status rather than reducing it to one value. On a review-level
comment the verdict is the actor's review state; on a line or file comment a
RequestChanges is a blocking objection and its absence a non-blocking note.

**A thread is blocking when any actor's current verdict in it is
RequestChanges**, root or reply, and stays blocking until that actor reduces
their own verdict; resolving does not clear it. The review has outstanding
blockers when any thread is blocking or any actor's review-level verdict is
RequestChanges.

A review-level verdict need not sit on a comment. An imported forge review that
approves or requests changes with no inline body, or a locally submitted bare
verdict, is a `Verdict` record: an author, its optional forge `origin` and
`synced_marker` (kind `Verdict`), and a disposition. It folds into the same
per-actor derivation as a review-level comment's disposition, greatest `Seq`
winning, so an actor's review verdict is the latest of their review-level
comment dispositions and their `Verdict` records. The record holds the
external identity a bare review-level comment would otherwise lack, so re-sync
deduplicates an imported verdict and push avoids submitting it twice.

```rust
pub enum RecordBody {
    // ... Session, DiffVersion, CommentEvent, Description, SourceChange, ForgeLink ...
    /// A review-level verdict with no comment body of its own.
    Verdict(VerdictRecord),
}

pub struct VerdictRecord {
    pub author: Author,
    pub authored_at: Option<OffsetDateTime>,
    pub origin: Option<ExternalRef>,
    pub synced_marker: Option<String>,
    pub disposition: Option<Disposition>,
}
```

**A single overall status**, for a reader that wants one line, is derived
deterministically and never compares timestamps across machines, which a
distributed log cannot trust. It is blocked if any human's current verdict is
RequestChanges (an Approve from another human does not clear it; only the
objecting author reduces their own); else approved if any human's is Approve;
else, with no human verdicts, blocked if any agent's is RequestChanges, else
approved if any agent's is Approve, else neutral. The reduction rests solely on
per-actor latest, never on cross-machine recency, so it holds under clock skew
and out-of-order arrival. This aggregate is a local display value only. It is
never submitted to a forge as one person's review: a forge review belongs to a
single account, so push submits only the local user's own per-actor verdict (see
the forge layer), and other actors' verdicts stay read-only mirror state.

**Collapsing a resolved thread keys on whether it still blocks.** The TUI and
the CLI or agent listing alike collapse a thread when it is resolved and not
blocking, and keep it expanded otherwise. Both cases you would expect follow
from this one rule. A thread the change author resolved while an objector's
RequestChanges still stands is resolved but blocking, so it stays visible for
everyone, the reviewer who must verify the fix included. A thread whose objector
withdrew their own verdict, or that only ever held nits, is resolved and not
blocking, so it collapses for everyone, the change author included. No
per-viewer rule is needed: the difference the change author and the reviewer see
comes from the resolve coupling above, where only the objector's own resolve
clears the block.

## Change identity and the source model

`wiff refresh` re-runs the source and rebases comments onto the new capture. The
rebaser is robust to content drift; the fragile part is what the source
re-resolves to. `SourceKind::GitRev { rev }` stores the rev as the user named it
(`"HEAD"`), which is a moving pointer. It survives `git commit --amend` and a
clean rebase only because HEAD happens to still point at the change; it silently
starts reviewing a different change once a commit is added on top, or once HEAD
becomes a merge commit.

The fix has two layers, mirroring what arcanist, Gerrit, and jj all converged
on.

### A change is a fixed tip plus a base rule

A review's scope is a range. Following arcanist's commit-range model, the tip is
fixed by the source (the working tree, or the change under review) and only the
base needs resolving. The base is a rule re-evaluated at each refresh, not a
pinned sha:

The source is framed around a neutral source-control system rather than git
specifically, so a jj or hg change slots into the same shape. `ScmId` ("git",
"jj", "hg") names which one; the per-scm `DiffSource` knows the plumbing.

```rust
pub enum SourceKind {
    /// A change captured from a source-control system.
    Scm(ScmSource),
    Stdin,
    /// A pull request the forge itself regenerates, for a session with no local
    /// repo to diff against. Refresh means re-fetching the linked pull request
    /// and appending a fresh diff version; there is no local branch, so build
    /// and test are unavailable and the review is the web-like experience. The
    /// pull request it mirrors is the session's `forge` linkage, not a field
    /// here: an in-repo import diffs local commits through `Scm` and keeps the
    /// same linkage, so which pull request and how the diff regenerates stay
    /// separate concerns.
    Forge,
}

pub struct ScmSource {
    pub scm: ScmId,
    pub selector: ScmSelector,
}

pub enum ScmSelector {
    /// The uncommitted working copy.
    Worktree,
    /// The staged index against its parent. Git-specific; an scm without a
    /// staging area does not offer it.
    Index,
    /// A change under review: everything from the resolved base up to the tip.
    Change {
        /// How to resolve the base of the range, as a comma-separated ruleset.
        base: String,
        /// A stable identity for the change, independent of its current
        /// revision, when one is available. Absent when the change cannot be
        /// identified beyond its range, as for a piped diff or an uncommitted
        /// working copy.
        change_id: Option<ChangeId>,
        /// How refresh finds the current tip of the reviewed range, persisted so
        /// it need not assume the current branch.
        tip: TipRule,
    },
}

/// How refresh re-resolves the tip of a reviewed range.
pub enum TipRule {
    /// A named ref: a branch or bookmark. Refresh re-resolves it every time, so
    /// an amend, a rebase, or a commit added on that ref is picked up with no
    /// wiff action, whether the rewrite was wiff's or the user's own git or jj.
    /// This is the branch-under-review case and the one a forge pull request
    /// maps to. The ref alone is the tip: a present `change_id` never overrides
    /// it, because pinning the tip to the one commit that bears the trailer
    /// would drop every commit added above it. `change_id` identifies the
    /// change for provenance only. If the ref is later renamed or deleted,
    /// refresh cannot recover the branch tip from `change_id` alone, since a
    /// trailer search finds only the one commit that bears it and not the
    /// commits stacked above; refresh reports the tip ref is gone and asks the
    /// user to re-point the session at a new ref, appending a `SourceChange`,
    /// rather than silently reviewing a shrunken range.
    Ref(String),
    /// Resolve a change id to its current commit each refresh, for an scm whose
    /// change id names a commit directly, jj foremost. The id lives in the
    /// variant so this tip is always resolvable; it equals the change's
    /// `change_id` provenance. This is the single change reviewed with no ref to
    /// follow, a jj working change with no bookmark; it follows that one change's
    /// own rewrites but reviews only it, not descendants stacked above, since
    /// extending the review to added commits is a ref tip's job. Git has no
    /// native change-id resolution: a `Change-Id:` trailer search needs a ref to
    /// search from and can match several cherry-picks of the same trailer, so a
    /// detached git change with no ref uses `Pinned`, not this variant.
    ChangeId(ChangeId),
    /// A pinned revision, for a detached `wiff new --change REV` that has
    /// neither a ref to follow nor a change id. It tracks no rewrite performed
    /// outside wiff: only `wiff commit` and `wiff amend` update it, by appending
    /// a `SourceChange`. An amend or rebase done outside wiff orphans it, and
    /// refresh reports the pinned revision is gone rather than reviewing a stale
    /// prefix of the change.
    Pinned(RevisionId),
}
```

The base ruleset takes the lesson arcanist learned: a base is not a pinned sha
but an ordered list of rules, tried left to right until one resolves to a
commit. A rule that resolves nothing falls through to the next, so one ruleset
works across repos with differently shaped histories (an `'origin/main'` rule
and a legacy `'origin/master'` rule can both sit in the list; whichever exists
wins).

There is a single ruleset, not a stack of per-repo config layers. Code review is
personal, and `trunk` already auto-detects each repo's default branch, so one
personal ruleset resolves correctly almost everywhere. Its precedence is just:
the `--base` flag for a one-off run, else the `base_revision_rules` setting in
the user's global config, else the built-in default. A repo whose base auto-detection is
wrong is handled by passing `--base` once at `wiff new` (the session then
remembers it, see below) or by a wrapper script, not by a config file committed
to the repo. Avoiding a committed config also means a checked-out repo can never
set an executable option like the editor command.

A ruleset is a comma-separated string. Each rule has an optional scm prefix, an
operator, and, for the range operators, a ref argument:

```
git:merge-base(trunk), git:merge-base(upstream), prompt
```

The grammar factors into three parts that combine freely.

The scm prefix names which source-control system resolves the rule. It is
optional and defaults to the session's active scm; an explicit prefix pins the
rule to one scm's resolver, which matters in a mirrored repo (a Sapling working
copy synced out to git through Mononoke) where the same ruleset legitimately
mixes `sl:` and `git:` rules and falls through from one endpoint to the next.

The operator turns a reference into a base:

| Operator | Meaning |
| --- | --- |
| `ref(X)` | Use commit `X` directly as the base. |
| `merge-base(X)` | Use the common ancestor of `X` and the tip. Almost always the intended one: it is your commits since you forked, even when `X` moved ahead. |
| `empty` | Use the empty tree, reviewing the whole history to the root. Takes no reference. |
| `prompt` | Ask the user to name the base interactively. Takes no reference. |

The reference `X` inside `ref(...)` and `merge-base(...)` is either a bare
computed symbol or a single-quoted literal ref. The quoting is what keeps them
apart: a bare word is always one of wiff's few symbols, and a literal ref is
always quoted, so a branch named `trunk` is `'trunk'` and never collides with
the `trunk` symbol.

| Reference | Resolves to |
| --- | --- |
| `'origin/main'`, `'trunk'`, ... (quoted) | that literal ref, via the scm |
| `trunk` | the repo's default branch (git's remote `HEAD`, jj's `trunk()`, Sapling/hg's default) |
| `upstream` | this branch's configured tracking tip (git's `@{upstream}`); no distinct meaning under jj, where it degrades to `trunk` |
| `@` | the tip under review |

There is no bare `upstream` rule; the intent is written in full as
`merge-base(upstream)`, so the operator and the reference each read plainly.
When a symbol resolves nothing (no upstream configured, no remote `HEAD`), its
rule no-matches and falls through, the same path as an unresolvable literal ref.

For anything the neutral symbols cannot express, an scm-native escape hatch
passes a quoted query through verbatim to that scm's own resolver:
`git:rev('<expr>')` feeds a `git rev-parse` expression, `jj:revset('<expr>')` a
jj revset. wiff does not interpret the contents; the scm does. The neutral
operators and symbols cover the portable common cases, and the escape hatch
means a base expressible only in one scm's language is never out of reach.

`prompt` needs a terminal. In a non-interactive run (an agent, a pipeline) it
fails rather than blocking, and the base must instead be given by the `--base`
flag or config that resolves without it.

The built-in default, when the user configures nothing, resolves for both a
human and an agent: `merge-base(upstream), merge-base(trunk), prompt`. The first
two are deterministic and cover the stacked-branch and fork-from-default cases;
`prompt` is only reached interactively, and a non-interactive run that falls
through to it fails asking for an explicit `--base`.

Once `wiff new` resolves a ruleset, the resulting
`ScmSelector::Change { base, tip, .. }` stores the ruleset string and a `TipRule`
in the session source, so `wiff refresh` reuses that session's base without
re-consulting config and re-resolves the tip without assuming the current
branch. A weird base is paid at most once, at creation, not on every refresh.

A `Ref` tip survives amend, rebase onto a new base, and commits added to the
branch, because the range is recomputed from the base rule and the freshly
re-resolved ref each time, whether the rewrite was wiff's or the user's own git
or jj. A `ChangeId` tip follows that one change's own rewrites but stays scoped
to it, so a commit stacked above appears only once a ref tip covers it. A pinned
`--change REV` tip follows nothing outside wiff and refresh reports it gone
rather than reviewing a stale prefix. The `Ref` case is the same shape a forge
PR has (a branch against a base), so `Forge` and an scm `Change` share the
rebasing path.

### A stable change id for the stack case

A base rule alone cannot always tell which commit is the change after a stack is
reordered, split, or squashed. Every mature tool solves this with a stable
per-change token independent of the commit id: jj's change id (a native property
of a commit that survives rewrite), Gerrit's `Change-Id:` footer, or arcanist's
`Differential Revision:` trailer. Arcanist's own base-detection docs recommend
consulting the trailer (`arc:amended`) before topological detection precisely
because topology gets stacks wrong.

Arcanist's and Gerrit's ids point at a central server, but the token itself does
not need one. Gerrit's `Change-Id` is minted client-side by a local commit-msg
hook as a random value; the server only ever matches on it. wiff is
decentralized and offline-first, and the same approach fits: a `ChangeId` is a
locally generated random value (a ULID serves), so two people running wiff
independently never collide, and the id only has to mean anything once it is
written into a commit message and shared. Before that point each side just has
its own local session, and there is nothing to reconcile.

Crucially, wiff mints and stamps a `ChangeId` only when it makes a commit on the
user's behalf (see the source transition below), never silently at `wiff new`.
The priority order for a change's identity is then:

1. an existing `Change-Id:` or `Differential Revision:` trailer. When present it
   is the shared canonical identity that a Gerrit or Phabricator server and any
   collaborators key on, so it wins even in a jj repo that also has one:
   picking the private jj id there would fail to reconcile with the server's
   view.
2. the jj change id, when the repo is jj and no such trailer is present (native,
   free, and it survives rewrites).
3. one wiff minted and wrote into the commit message when it made the commit.
4. absent all of the above, none: an uncommitted working-copy draft or a piped
   diff has no change id, and refresh falls back to the base rule.

The `change_id` and the base rule are complementary: the rule finds the range
cheaply, and the id makes refresh correct under stack rewrites.

When wiff mints one, the stamp is a literal git trailer in the last paragraph of
the commit message, spelled `Change-Id:` after the established trailer
convention, so any tool that already reads that trailer interoperates and
priority 1 round-trips an existing one unchanged:

```
    Add the frobnicator

    Change-Id: 01KXGKBGBYV1TCAZ58FRJ7GCK5
```

The value is a freshly minted ULID. A ULID is not pure randomness: its 128 bits
are a 48-bit millisecond timestamp followed by 80 bits of randomness, so an id
has both a time component and enough entropy that two people minting
concurrently (80 random bits even within the same millisecond) will not in
practice collide. The value and the session id are deliberately distinct
concepts: a session ULID names one local review log and
differs between two people (or two checkouts) reviewing the same work, whereas
the `ChangeId` names the logical change itself and must be identical everywhere
it is seen. Deriving it from the committer's session id would tie a shared,
cross-person identity to one person's private log; minting a dedicated value
keeps them independent.

### The session source over its life: draft, commit, import

The two use cases wiff serves today both have a session whose source changes
over its life, so the source cannot be a fixed field in the immutable
`SessionHeader`. It becomes foldable state, set at creation and revised by a
later event.

**Agent draft, then commit.** An agent has no `.git` write access and usually is
not working against a commit at all; it iterates on the dirty working copy. It
starts a review with `wiff new --description` to draft a reviewable unit of work
(the description standing in for the eventual commit message) against a
`Worktree` source, and human and agent evolve the working copy together. When
the human is ready, a `wiff commit`, run where they hold their signing keys and
`.git` write access, makes the commit, applies the review description as its
message, and mints and stamps a `ChangeId`. At that point the session's source
transitions from `Worktree` to an scm `Change` whose `change_id` is that minted
value. The `Change` needs a concrete `tip` and `base` for the next refresh to
resolve a range, and `wiff commit` fills both without guessing. A working-copy
draft, whether `Worktree` or `Index`, has no base of its own (it diffs the
working tree or the index, not a range), so this is the first time a base is
needed. `wiff commit` resolves it once here from the same precedence `wiff new
--change` uses (a `--base` flag on the commit, else the configured ruleset) and
stores the resolved ruleset string in the `Change` selector; later refreshes
reuse that stored ruleset without re-reading config.
The `tip` follows where the new commit sits. Under jj the change id names its own commit, so the
tip is `ChangeId(change_id)`. Under git it depends on `HEAD`: a checked-out
branch gives `Ref(branch)`, so later commits on that branch are picked up; a
detached `HEAD` gives `Pinned(new commit)`, the honest no-ref case that only a
later `wiff amend` moves. The transition is a foldable event; the comment history
and description are preserved unchanged, and subsequent refreshes track the
committed change.

**PR import.** `wiff forge pull <number>` deduces the forge from the repo's git
remote, fetches the pull request, and builds the review state to match: its diff
as diff versions, its description as a `Description` event, its comments and
verdicts as events tagged with their `ExternalRef`. Every import records which
pull request it tracks as the session's forge linkage (below), and the two kinds
of import differ only in what the diff regenerates from. In a repo the fetched
branch becomes an `Scm(Change)` source: refresh diffs the local commits, so a
local rebase or amend is exactly what gets reviewed, and the change also builds
and tests. With no repo there is nothing local to diff, so the source is `Forge`
and refresh means re-fetching the pull request; the review is the web-like
experience, without build or test. Either way, review proceeds offline and local
until `wiff forge push` publishes local comments back.

The session is identified by its own id and forge linkage, and its comments by
their `ExternalRef`, never by a local branch ref, so the session outlives the
local branch. Deleting the fetched branch does not touch the session, and there
are two equivalent ways to take in new upstream commits: a `wiff forge pull`
re-sync that folds them into the existing session, or discarding the local
branch and re-fetching it, after which the next re-sync re-anchors the session's
comments by content onto the refreshed head. Either way the same session, with
its whole comment and verdict history, continues against the moved pull request.

The source transition is a record type of its own:

```rust
pub enum RecordBody {
    // ... Session, DiffVersion, CommentEvent, Description ...
    /// Re-point the session's source. The last one folded is the current
    /// source; the initial source still comes from the header.
    SourceChange(SourceChangeRecord),
}

pub struct SourceChangeRecord {
    pub author: Author,
    pub authored_at: Option<OffsetDateTime>,
    pub origin: Option<ExternalRef>,
    pub source: SourceKind,
}
```

`ReviewState` exposes the current source (the header's, or the last
`SourceChange`). `wiff commit` appends a `SourceChange` to `Scm(Change)` when it
makes the commit for a working-copy draft.

### The forge linkage

Which pull request a session reconciles with is separate from how its diff
regenerates. An in-repo import diffs local commits through an `Scm` source yet
still tracks a pull request; a session created by `wiff forge push --create`
keeps its `Scm(Change)` source and only gains a pull request to target. So the
linkage is its own foldable state, not part of `SourceKind`:

```rust
pub enum RecordBody {
    // ... Session, DiffVersion, CommentEvent, Description, SourceChange ...
    /// Point the session at the pull request it reconciles with. Set once; fold
    /// rejects a second `ForgeLink` naming a different pull request.
    ForgeLink(ForgeLinkRecord),
}

pub struct ForgeLinkRecord {
    pub author: Author,
    pub authored_at: Option<OffsetDateTime>,
    pub forge: ForgeRef,
}
```

`ReviewState` gains `forge: Option<ForgeRef>`, None until a `ForgeLink` folds.
`wiff forge pull` appends one at import; `wiff forge push --create` appends one
when it opens a pull request from an `Scm(Change)` session. A `Forge` source
reads this linkage to know which pull request to re-fetch, which is why the
`Forge` variant needs no `ForgeRef` of its own.

The linkage is set once, not last-wins: a second `ForgeLink` that names a pull
request equal to the current one is idempotent, but one naming a different pull
request is a corrupt log, since silently retargeting a session would strand
every `origin` and `synced_marker` folded against the old pull request. A
session is deliberately relinked by discarding it and re-importing, not by
appending over the link. A repeated identical link (a re-import of the same pull
request) folds cleanly.

### Provenance on diff versions

Each `DiffVersionRecord` records the resolved base and head revisions of the
capture it came from:

```rust
pub struct DiffVersionRecord {
    // ... existing number, diff_hash, files ...
    /// The resolved endpoints this version was captured between, when the
    /// source has them. Informational for a non-regenerable stdin source;
    /// authoritative for scm and forge sources.
    pub base_revision: Option<RevisionId>,
    pub head_revision: Option<RevisionId>,
}
```

These are needed for forge mapping and publish-back anyway, and they let refresh
detect and report a history rewrite (the base rule now resolving elsewhere)
instead of silently switching what is under review. A `Worktree` or `Index`
capture has no authoritative head (the working tree or index is not a commit) but
does have an authoritative base: it records `base_revision` as the `HEAD` it sits
on. That captured base is the durable pre-commit anchor `wiff commit` reads to
recover from a crash between the commit and its `SourceChange` (below), not a
separate recorded tip.

### Per-commit view

A change assembled from several commits (a messy git branch, a sequence of
fixups) is reviewed as one unit, but a reviewer often wants to read a single
commit's diff on its own. The version picker gains this: alongside the historical
bases it already offers, it lists the commits of the latest session version's
range, and selecting one renders that commit's diff in isolation rather than the
grounded base-to-head diff. Only the latest version's commits are offered, so the
commits shown are always ones the session still stands behind.

Reading a commit in isolation moves the view's right-hand side off the session
head, but the persisted comment anchor stays grounded on it. A comment authored
in this view records the commit it was seen against as its `authored_revision`,
and the same content match `wiff refresh` performs then reanchors it onto the
latest head. When the commented line still exists at head the comment lives there
like any other; when it existed only transiently (a line a later commit removed)
the reanchor finds no home and the comment is detached, the same state a comment
reaches when a later refresh removes its line. The `authored_revision` is kept in
both cases: it gives the reanchor a precise starting point and, on publish, names
the forge commit the comment belongs to.

Which endpoints a selected commit diffs between (its own delta, or that commit
onward) and how a merge commit reads are display details settled when the view is
built; they change no persisted state.

## The forge layer

`wiff forge pull` imports a forge pull request into an ordinary session: its
diff becomes diff versions, its description a `Description` event, its comments
comment events, its review verdicts dispositions. (The reverse direction,
publishing local state back, is covered under "Publishing local review state
back" below.) What marks an event as imported rather than locally authored is
its `origin`, an opaque forge-namespaced reference:

```rust
/// A stable name for an object on a forge, kept opaque so a new forge needs no
/// schema change.
pub struct ExternalRef {
    /// The forge instance the object lives on, provider plus host, so an id from
    /// github.com is never confused with the same id on an enterprise host.
    pub forge: ForgeId,
    /// What kind of object it is (review comment, discussion note, description,
    /// verdict).
    pub kind: ExternalKind,
    /// The forge's identifier for the object. A string, not a number, so it
    /// covers numeric ids and global-node ids alike; compound when a forge
    /// needs more than one part (a GitLab project plus iid).
    pub id: String,
    /// A link to the object, when the forge gives one. Presentation only, and
    /// deliberately outside the object's identity: a host migration or an
    /// adapter that normalizes URLs can change it while the object is the same.
    pub url: Option<String>,
}
```

The object's identity for deduplication is `(forge, kind, id)` only; `url` is
never part of it. Re-sync keys on that triple, so a moved or rewritten link
never makes a known object look new and duplicate its imported events.

```rust
/// A forge instance, provider plus normalized host, so two installations of the
/// same provider never collide. The provider is an open string rather than a
/// closed enum so a new forge does not force a schema bump.
pub struct ForgeId {
    /// The provider family: "github", "gitlab", "codeberg", ...
    pub provider: String,
    /// The normalized host of this instance, so github.com and a self-hosted
    /// GitHub Enterprise are distinct: "github.com", "git.example.com".
    pub host: String,
}

/// The neutral class of forge object an `ExternalRef` names. A small closed set
/// of the object kinds wiff mirrors, translated per adapter to the forge's own
/// object types (a `ReviewComment` is a GitHub review comment or a GitLab
/// diff-note; a `Verdict` is a GitHub review submission or a GitLab approval).
pub enum ExternalKind {
    ReviewComment,
    Description,
    Verdict,
}
```

A `ForgeRef`, the session's forge linkage, names the pull request in neutral
terms:

```rust
pub struct ForgeRef {
    pub forge: ForgeId,
    pub namespace: String,
    pub project: String,
    pub number: u64,
}
```

The tuple (forge instance, namespace, project, number) covers a GitHub PR, a GitLab MR
(project plus iid), and a Gitea or Codeberg PR (owner, repo, index). The
per-forge `DiffSource` knows how to turn it into a fetch.

Supporting concepts:

- **Author identity across forges.** `Author` gains an optional external handle
  so an imported reviewer's login is preserved for faithful round-tripping:

  ```rust
  pub struct Author {
      pub name: String,
      pub kind: AuthorKind,
      /// The author's account on the originating forge, for a mirrored event.
      /// The `forge` inside `ForgeHandle` is the instance (provider plus host),
      /// so `"alice"` on github.com and `"alice"` on an enterprise host are
      /// distinct accounts rather than an ambiguous bare string.
      pub handle: Option<ForgeHandle>,
  }

  pub struct ForgeHandle {
      pub forge: ForgeId,
      pub handle: String,
  }
  ```

- **Re-sync folds, it does not just dedup.** Keying on `ExternalRef` stops a
  re-import from duplicating an object, but a known ref whose upstream content
  changed (an edited body, a newly resolved thread) must still update local
  state. The importer compares the upstream object's last-modified marker
  against the `synced_marker` last folded for that object (a comment's, or the
  review's folded description marker), and appends the corresponding event (an
  `Edit`, a `Resolve`, a fresh `Description`, each stamping the new marker) when
  it moved. Idempotent means no new event when nothing changed upstream,
  not skipping a genuine change. A local comment starts with no `origin`;
  publishing it creates the object on the forge and appends a `Link` event
  bearing the returned `ExternalRef`.

- **An upstream deletion tombstones its local mirror.** When a re-sync's
  authoritative enumeration of the pull request no longer returns a comment that
  was previously imported (with an `origin`), the importer appends a `Delete`
  authored by the forge, so the local mirror follows the upstream removal. This
  fires only from a complete enumeration: an adapter that can only page or filter
  the object set must not infer deletion from an object's absence, since a
  filtered page is not an authoritative "gone." A local-only comment (no
  `origin`) is never touched by this. When an adapter cannot enumerate
  authoritatively at all, re-sync leaves the mirror in place and does not crash.

- **A session-level sync cursor detects new upstream objects.** A per-object
  `synced_marker` cannot notice a comment that was *created* upstream since the
  last pull, since there is no local object to compare it against. So a pull
  records a session-level cursor of the forge state it observed: the forge head
  revision and the authoritative set of object refs (or an adapter-kept digest
  or `updated_at` high-water mark). Push reads the cursor to refuse a stale
  view, catching a new upstream comment or a moved head before it posts against
  a review that no longer matches upstream.

- **Anchoring for publish-back stays neutral.** The common denominator across
  forges is (base revision, head revision, old/new path, line, side). We already
  store side, path, and line in `CommentTarget`, the snippet in `Anchor`, and
  now the revisions on `DiffVersionRecord`. Each forge adapter translates that
  to its own position model; no forge-specific position is ever persisted.

- **Verbs stay neutral.** Resolve (a bool) and disposition (Approve /
  RequestChanges) map per adapter to the forge's verb: GitHub submit-review and
  resolve-thread, GitLab approve and resolve-discussion, and so on.

### Publishing local review state back

`wiff forge push` is the reverse of pull: it sends whatever local review state is
unpublished or has changed since it was last synced, and records the result. A
comment with no `origin` becomes a new forge object; publishing it appends a
`Link` bearing the returned `ExternalRef` and stamps the `synced_marker`. A
change is not one-shot: an already-linked comment whose folded body, resolved
state, or disposition differs from what its `synced_marker` last recorded is
republished as the matching forge edit, resolve, or review update, and the
marker advances; a locally deleted linked comment is deleted or minimized
upstream. The review description publishes the same way, as the pull request's
body, guarded by its own `synced_marker`. A local verdict becomes a review
submission. So the marker is what makes push incremental: it sends exactly the
local changes the forge has not yet seen, and a comment or description unchanged
since the last sync sends nothing.

**Push sends only the local user's own authored state.** wiff takes the actor
whose local `Author` is the invoking human `$USER` to be the account the token
authenticates as, and publishes only that actor's comments and verdict. Events
authored by an agent, or mirrored from another forge user, are never pushed: a
forge has no faithful place for an agent's review and posting one on the human's
behalf would be spam, and another user's imported state is theirs to publish,
not this token's. The forge login is read from the token (a `GET /user` call)
rather than configured, so no identity mapping is required; a future option
could widen or override this, but the simple rule is the default. The same
command serves a reviewer (their comments and verdict) and a maintainer who also
amended a fix (those plus the branch).

**Comments anchor to the forge's head, never to a local amendment.** A review
comment critiques the code as reviewed. When a maintainer pulls a pull request,
comments on it, then amends a suggested fix and `wiff refresh` re-anchors those
comments onto the amended commit, the comments still belong on the pull
request's head commit, where the criticized code is still present, not on the
fix, where the line may no longer exist. So push re-fetches the forge head at
push time and locates each comment's snippet (the `Anchor` already captured) in
that current forge-head diff, by the same content match refresh uses. Anchoring
against the head fetched now, rather than the commit that was current when the
comment was authored, is what makes the review resilient to local history
rewrites: a reviewer can pull a pull request, rebase it onto main, amend, and
comment at any point in between, and each comment still resolves onto whatever
the forge head is when the push finally runs. For a combined
push, comments post against the current forge head first, then the branch push
advances it; the forge keeps each comment on its original commit, marking it
outdated if the line later changes, which is the human experience of commenting
on code and then pushing a fix.

**A comment publishes against the commit it was authored on.** Placement is
decided by the pre-push content match against the current forge head, not by the
commit hash that was current when the comment was authored, so local rebases and
amends between authoring and push do not strand it. For a comment authored in
the ordinary view that match finds the snippet at the forge head and it
posts inline on the current diff. For one authored in the per-commit view whose line no longer exists
at head, `authored_revision` names an earlier commit still in the pull request,
and the comment posts there with that commit as its `commit_id`; the forge shows
it inline but outdated, keeping its file and hunk, with no demotion. Only when
that commit is itself gone from the pull request (squashed or dropped upstream
between authoring and publish, which the stale-view refusal usually catches
first) does the comment fall back to a pull-request-level comment quoting the
file, line, and snippet as text, so context is preserved rather than lost.

**A push is resumable, not atomic.** A forge REST API offers no transaction that
creates many comments in one call, so a network failure after posting the k-th
comment leaves those k objects on the server. Rather than pretend at atomicity,
push is safe to re-run: each posted comment appends a `Link` recording its
returned `ExternalRef`, and push skips any comment that already has one. A
pre-flight pass validates every comment's placement (the content match against
the current forge head) before any is posted, so the common case of a comment
that cannot be placed is caught up front rather than halfway through a partial
post. A later failure can still come from elsewhere (the head moving under us, a
revoked permission, a rate limit, an API rejecting a single create), and the
`Link` records make all of these safe: re-running posts only the remainder.

A crash between the forge accepting a comment and the `Link` being appended
would otherwise re-post a duplicate on retry, since the journal has no record of
the accepted object. To close that window, each posted comment embeds a wiff
correlation token derived from the local comment id (an otherwise invisible
marker in the comment body, such as an HTML comment), and retry looks the comment
up by that token before posting, adopting the existing object and appending the
missing `Link` rather than creating a second. Recovery keys on that token, never
on body-and-anchor equality, which could match a different reviewer's identical
comment or an older duplicate. When a forge exposes a native idempotency key,
the adapter uses it instead; when it offers neither a queryable token nor an
idempotency key, the adapter reports the uncertain post rather than silently
adopting a match.

**Push refuses a stale view.** Mirroring `wiff commit`'s drift refusal, push
first checks the forge: if the head moved or upstream comments changed since the
last pull, it refuses and directs the user to `wiff forge pull` first, so local
state never posts against a view that no longer matches upstream. Because that
re-sync is non-destructive to the local branch, following the direction costs
the user none of their local rebase or amend work. `wiff forge status` shows the
same comparison and sends nothing.

**Maintainer edits reuse git.** A maintainer amends locally with `wiff amend`,
then publishes. When a pull request allows maintainer edits, its head
is a fork's repository and branch. `wiff forge push --code` reads those from the
pull request metadata and pushes the amended commit there with git
(`git push <fork-url> HEAD:<head-ref>`). The destination is always the exact
fork url and head ref read from the pull request metadata, never the repo's own
`origin` or default branch; push refuses if that head ref resolves to a
protected or default branch, so a code push cannot reach `origin/main` in the
belief it is updating a review branch. Because a rebase or amend rewrites
history, the branch update is not a fast-forward, so the push uses a lease tied
to the forge head fetched for this push (`--force-with-lease=<head-ref>:<expected>`):
it overwrites the head wiff just reviewed against, but fails rather than
clobbering a head someone else advanced in the meantime, directing the user to
pull. An unconditional force is never used. Reading the writable fork url, the
exact head ref, and the branch's protected or default status is an adapter
capability; if that status cannot be read, the push refuses rather than assuming
it is safe. GitHub is first.

**`--create` opens a pull request in a fixed order.** There is no forge head to
anchor against until the pull request exists, so `--create` pushes the change's
branch to the destination first, opens the pull request from it, appends the
`ForgeLink`, and only then posts comments and the verdict against the new pull
request's head. The destination is the head branch of the base repo (or the
user's fork for a fork-based contribution), read from configuration and the
repo's remotes; `--create` refuses when the change has no branch to publish (a
detached tip) or when no destination remote can be determined, rather than
guessing. This branch-first order does not contradict the update path, where
comments post against the current forge head before a code push advances it: an
update has a head to target already, while a create is establishing one.

**Talking to the forge is self-contained.** The adapter speaks the forge's API
directly over HTTPS with a Rust client, not by shelling out to `gh` or another
external tool; git and jj still do the source-control fetch and push. Deployment
stays a single binary plus the scm already required.

**The API token is sourced by forge-scoped composition**, the first hit
winning:

1. `--token-file <path>`, then `--token <value>` on the command line. These are
   unambiguous in context because the invocation already determines the forge.
2. The forge's conventional environment variables, each with a `_FILE` sibling
   checked ahead of the inline form. For GitHub that is `GH_TOKEN_FILE` /
   `GH_TOKEN`, then `GITHUB_TOKEN_FILE` / `GITHUB_TOKEN`; a GitLab adapter would
   read `GITLAB_TOKEN_FILE` / `GITLAB_TOKEN`. These names are inherently
   forge-scoped, so no wiff-specific variable is introduced; an unscoped one
   could not say which forge a token in the environment was for.

The `_FILE` form is preferred and documented first: a path does not leak the
secret into `ps`, child processes, or a crash dump the way a live token in the
environment does. Its content is read with trailing whitespace trimmed (the
common paste error). A token file that is named but unreadable is a hard error,
not a fall-through to the next tier, because a named file that we then ignore
hides the user's mistake. The git credential helper is deliberately not
consulted: most users authenticate git over ssh and would have no usable HTTPS
credential there, so the tier would rarely produce a token and is not worth the
machinery.

## Command-line changes

`wiff new` gains a description and a renamed change source:

- `--description <text>` sets the description explicitly, attributed to the local
  author. The description is the review's equivalent of a commit message (a
  title plus optional body, what you would pass to `git commit -m`); once the
  draft is committed it becomes the commit message verbatim.
- `--change [REV]` reviews a change: the tip is `REV` (defaulting to the current
  change, the branch tip, or `@` under jj) and the base comes from the base
  ruleset. This replaces `--head`. The concept is named a **change**, matching
  jj and Gerrit vocabulary and sidestepping the branch/bookmark mismatch across
  git, jj, Sapling, and hg. The `TipRule` follows from what `REV` names: a branch
  or bookmark becomes `Ref(name)` so later commits on it are picked up; an
  explicit revision or a detached `HEAD` becomes `Pinned(rev)`, since there is no
  ref to track; a jj change id becomes `ChangeId`. When `REV` resolves to both a
  branch and a revision the branch wins, the common intent being to follow it.
  Unlike `wiff commit`, `--change` may review a multi-commit range; such a
  session is fully reviewable but cannot be amended, since amend is single-node.
  When no `--description` is given, the description
  defaults to the change's commit message, attributed to the commit author with
  the commit date as its authored time (decided).
- `--base <ruleset>` sets the base rule for this run, ahead of config.

New commands drive the source transitions and the forge sync:

- `wiff commit` makes one new commit from the session's dirty
  files, applies the review description as its message (refusing before any
  mutation when the session has no description, since there is then no commit
  message to write), and appends a
  `SourceChange` re-pointing the session onto the committed change. `wiff commit`
  produces a single-commit change, so the new `Change` selector's base is the new
  commit's parent (the pre-commit `HEAD`), not the base ruleset: `base..tip` is
  exactly the one commit just made, which is exactly what the draft reviewed, and
  a branch that already had earlier commits above the ruleset base does not
  drag them into the review. Refresh recomputes the base as the resolved tip's
  first parent each time, so the change stays a single commit across a later
  amend or rebase. (The base ruleset stays the mechanism for `wiff new --change`,
  which may review a range; commit is deliberately the narrower single-commit
  path.) Because the commit contains exactly the reviewed content (the drift
  check and per-selector staging isolation guarantee it) the post-transition
  `parent..tip` diff reproduces the latest draft diff, so no diff version is
  recaptured and existing anchors are preserved unchanged. How it
  selects what to commit depends on the draft's selector. A `Worktree` draft commits only the files present in the
  session, building the commit from a temporary index that holds just those
  paths, so files already staged in the real index (unrelated work) are left for
  the user's own later commit rather than swept in, and never `git add -A`; under
  jj, which has no index, it commits those paths directly (`jj commit <paths>`),
  leaving unrelated edits behind. An `Index` draft was captured from the staged index, so it commits the
  index as it stands and does not re-stage from the working tree, which would
  pull in edits made to those paths after capture that the review never showed. Under git, when the description does not already
  have a `Change-Id:` or `Differential Revision:` trailer, it mints and stamps a
  `Change-Id:` one; an existing trailer (from a forge import or a hand-written
  draft) is adopted as the change id and left in place rather than duplicated.
  Under jj it adopts the native change id, which already survives rewrite. Run where the user
  holds their signing keys and `.git` write access; signing is left to the scm's
  configured backend (git's `commit.gpgsign`, jj's signing backend) rather than
  invoked directly. Before any mutating action it verifies the session still
  corresponds to what was captured (for a `Worktree` draft, the reviewed files
  exist and their working-tree content matches the latest diff version; for an
  `Index` draft, the staged index still matches it) and refuses on drift, so it
  cannot silently capture something the review never showed. It is valid only when
  nothing has been committed for the session yet; once the session's node (the
  commit under git, or the change under jj) exists it directs the user to `wiff
  amend`. The drift refusal is deliberate: when the reviewed
  files changed after capture (a suggested fix, reformatting), the workflow is
  `wiff refresh` to recapture, re-read the updated review, then `wiff commit`.
  If a crash interrupts the command after the commit is made but before the
  `SourceChange` is appended, re-running reconciles rather than committing twice:
  it searches the commits at or above the base revision the draft was captured on
  (the pre-commit `HEAD`, recorded on the latest diff version) for
  one bearing the change id it was going to use, and if it finds it, adopts that
  commit and appends the missing `SourceChange`. The change id, a minted or
  adopted `Change-Id:`, is the recovery key because it is unique; tree and message
  equality is not, so it is not used (see Durability and recovery).
- `wiff amend` exists to apply the review
  description to the session's node (the commit under git, the change under jj);
  folding the dirty files into it is
  incidental. To fold a code fix in without touching the message, use `git commit
  --amend --all` outside wiff. Under git it rewrites the one node; under jj it
  re-describes the change and squashes a child working copy into it when the fix
  sits above the node (`jj describe` / `jj squash` by position). The change id is
  unchanged and only the commit hash moves, so the provenance gate keys on the
  id. Both `commit` and `amend` operate on exactly one node, so `amend` refuses,
  naming the reason, when the working copy is not on that node, when the node's
  provenance does not match the session (no matching change id, or not the head
  the session mirrored from the forge), when the reviewed unit spans more than
  one commit (a multi-commit range, whose squash-or-preserve shape is
  project-dependent and which wiff will not guess), or when the node has no
  associated wiff review. A multi-commit range stays fully reviewable (pull,
  comment, push comments, verdict); only the code-amend is withheld, and the
  history surgery -- squashing the range or splitting it into single-commit
  changes -- is left to the user's own git or jj workflow. Amend folds in only
  the session's reviewed files, with the same isolation and drift check as
  commit: a reviewed path whose content changed after the latest captured version
  makes amend refuse (run `wiff refresh` first), and dirty files outside the
  reviewed set are left out rather than swept into the node, so an amend never
  folds code no session version showed. Replacing the message
  with the description preserves the identity trailer: an existing `Change-Id:`
  or `Differential Revision:` is merged back into the new message rather than
  dropped, so provenance survives the rewrite.
- `wiff describe` revises the review description after creation, appending a new
  `DescriptionRecord` (latest wins). It is the local authoring path for the
  mutable description, the way `wiff comment` edits a comment, so a draft's
  eventual commit message evolves without re-creating the session; the TUI offers
  the same edit.
- `wiff forge pull [<number>]` mirrors a forge pull request into a session. With
  a number it deduces the forge from the git remote, fetches the pull request,
  and builds a session mirroring its diff, description, comments, and verdicts
  for offline review, recording the pull request as the session's forge linkage;
  without one it re-syncs the current session's pull request, folding upstream
  edits and re-anchoring local comments onto any moved head. A re-sync updates
  review state only: it never resets, rebases, or fast-forwards the local branch,
  so a local rebase or amend is never undone in the name of syncing. When the
  local branch has diverged from the forge head it updates the session's forge
  sync cursor (the forge head revision and observed object set) and warns,
  leaving code integration to the user's own git or a later restack. The forge
  head is not folded as a diff version in the local source's sequence: the
  session's diff versions describe the user's local branch, and mixing a remote
  head into that sequence would cross two histories and misanchor comments. In a
  repo the first import fetches the branch into a local branch and gives the
  session an `Scm(Change)` source tracking that local branch by
  `TipRule::Ref`, so refresh reviews the user's own commits and follows a local
  rebase or amend, while the forge head lives only in the sync cursor. Its base
  is the ruleset resolved against the pull request's base branch (its
  merge-base), stored in the selector like any other `Change`. A fork pull
  request with no local branch has one created from the fetched head; if the
  head or base refs cannot be fetched, import refuses rather than persisting a
  ref that will not resolve. With no repo the session has a `Forge` source and
  stays review-only, the web-like experience.
- `wiff forge push [--comments] [--code] [--create]` sends local review state to
  the pull request. With neither content flag it sends both as appropriate to the
  role: a reviewer typically has only comments, a maintainer amending a fix has
  both. `--comments` sends comments and verdicts alone, for publishing straight
  away while the code side takes its time; `--code` pushes the branch alone.
  `--create` opens a new pull request from the current change; a push without it
  targets the session's existing pull request. On success `--create` appends a
  `ForgeLink` recording the new pull request as the session's forge linkage and
  leaves the `Scm(Change)` source untouched, so a later `wiff forge push` knows
  which pull request to target while refresh keeps diffing the local change. The
  forge layer covers how comments anchor and how push guards a stale view.
- `wiff forge status` previews what a push would send and whether upstream has
  moved since the last pull, sending nothing.

### Stdin front matter

Piped input (`git show | wiff new`) sniffs the stream for commit front matter.
The common case is a single commit (`git show`), where the message precedes the
first `diff --git` and a one-shot split at that point separates front matter
from diff:

- From the `commit` / `Author:` / `Date:` header we extract the description
  (subject to title, remainder to body), the author, and the head revision. The
  description is attributed to the commit author with the commit date,
  consistent with `--change`.
- When no recognizable header precedes the diff, the stream is a bare diff as
  today.

A multi-commit `git log -p` stream interleaves each message with its diff, so
only the newest commit's message sits before the first `diff --git`; every
earlier one appears between hunks. Concatenating the messages therefore needs a
scanning parser that recognizes each `commit` / `Author:` / `Date:` block
wherever it appears, not a single split. This is a fast follow after the
single-commit case.

`git format-patch` mail format (`From `, `Subject: [PATCH] ...`, then a `---`
diffstat separator before the first `diff --git`) is a second fast follow. It
needs its own recognizer rather than a generic first-`---` split, because that
`---` separates the message from the diffstat and a naive split would misfile
the diffstat as diff.

Stdin remains non-regenerable, so its captured head revision is informational.

## Fold changes

`fold` and `ReviewState` extend to match:

- A `CommentEvent` create introduces a comment; edit, resolve, delete, reanchor,
  and set-disposition fold onto it, each stamping the folded time from
  `authored_at` or `Record.at` and recording the acting author.
- A reply (`CommentTarget::Comment`) must reference a known comment; `fold`
  reconstructs threads by walking `reply_to`.
- `Description` records fold to the latest description.
- `SourceChange` records fold to the current source; the header supplies the
  initial one.
- Import folds onto state keyed by `ExternalRef`: a re-sync appends an event
  only when the upstream object changed, so it neither duplicates an object nor
  skips a genuine upstream edit.
- An imported `Create` sets `CommentState.origin` from its envelope, and a `Link`
  sets it for a locally-published comment; origin is set once, and a later `Link`
  or imported event whose `origin` differs from the bound object is fatal, not a
  re-point. Any event bearing a `synced_marker` folds it onto the matching state
  (`CommentState.synced_marker`, or the review's folded description marker).
- A `Verdict` record folds into the per-actor review verdict alongside
  review-level comment dispositions, greatest `Seq` winning, and has its own
  `origin` and `synced_marker` for a bodyless forge review.
- A `ForgeLink` sets `ReviewState.forge` once; a second naming a different pull
  request is fatal, an identical one idempotent.

The strict corruption stance is unchanged for anything in the interior of the
log: an unknown record type, or a mutation against an unknown comment, is a fatal
`InconsistentLog`. A truncated final record is the one exception, covered next.

## Durability and recovery

The journal is an append-only log of review state, not a store of precious
source. The worst plausible cost of a crash is repeating some review work, or
rebuilding a session by re-importing the change it mirrors. That budget rules out
a write-ahead log or a two-phase commit; it asks instead for operations that
degrade gracefully and re-run safely. Three rules cover every multi-step
operation.

**A torn final record is dropped, not fatal.** `append` writes one record at a
time, so a crash can damage only the last line. `fold` discards an unparseable
trailing record and folds the rest. This is distinct from the strict stance on
interior corruption above: a malformed record with a valid record after it
signals a bug or tampering and stays fatal, while a truncated last line is the
ordinary signature of a crash mid-write. With this rule, any interrupted
multi-record operation leaves a shorter, valid log rather than a corrupt one.

`fold` does not audit the `Seq` and `VersionNumber` sequences for gaps,
duplicates, or ordering beyond what it already relies on (a mutation must follow
its target's create). A crash cannot produce a gapped or duplicated sequence,
since `append` writes whole records under a lock; such a log would come only from
a bug or tampering, the same class the interior-corruption stance already refuses
by the record it trips on. Adding standalone sequence validators would defend
reconstructible data against a fault that, if it occurred, points at a defect to
fix rather than a log to salvage, so it earns none of the budget a real recovery
case would.

**Reconcile against observable external state, never against an assumed
guarantee.** An operation with an external side effect (an scm commit, a forge
post) begins by asking the external system what already happened and does only
what is missing. `wiff commit` keys reconciliation on the `ChangeId` trailer it
minted or adopted, matched within the commits at or above the tip it recorded
before committing, so a resumed run adopts the commit it already made rather than
making a second; tree and message equality is deliberately not the key, since it
is not unique. A push keys on whether a comment already has a `Link`, and on
a forge-side idempotency lookup for the window between the forge accepting a post
and the `Link` being written. An import keys on the pull request number and each
object's `ExternalRef`. wiff assumes no atomicity or reliability from these
systems beyond what it can observe: git's single ref update is the only atomic
boundary it leans on, and it leans on nothing from a forge, which it does not
trust to be reachable or consistent from one call to the next. When the external
state cannot be observed right now (the forge is unreachable), the operation
refuses and asks to be re-run rather than guessing which branch of the unknown
outcome it is in.

**Re-run and re-import are always safe.** Because every reconciliation key is a
unique token and the data at risk is reconstructible, running a command again is
a legitimate recovery, not a fallback to avoid. `wiff forge pull` re-imports a
session from the pull request it mirrors, and re-running `wiff commit`,
`wiff refresh`, or a push completes or repeats the work without corrupting state.

These rules dissolve the couplings that would otherwise want atomic writes:

- **Session creation** appends the header, first diff version, and optional
  description in order. A crash before the description leaves a valid session
  without one, recovered by `wiff describe` or by re-running creation; nothing is
  lost.
- **A no-repo forge import** writes the header and `Forge` source, then the
  imported review records and the `ForgeLink` naming the pull request. A `Forge`
  source needs that linkage to know what to re-fetch, so an import interrupted
  before it is written is an incomplete session: it is detected at load (a `Forge`
  source with no folded `ForgeLink`) and re-run, which re-imports from the pull
  request. A session that never reached a usable state is rebuilt, never trusted.
- **The resolve-and-withdraw coupling** needs no paired write. `fold` derives the
  withdrawn block directly from the objecting author's own resolve, so the
  explicit `SetDisposition(None)` that once accompanied it is gone and a crash
  cannot leave a comment resolved while its verdict still blocks.

## Implementation sequencing

The pieces layer cleanly, foundation first: the `CommentEvent` envelope, then
replies, description, disposition, the change source model, and last the forge
layer. The envelope's shape is fixed at the format break that reshapes the
existing records; later layers populate fields it already reserves, and each
still bumps `FORMAT_VERSION` when it adds a record variant, since that alters the
on-disk format. Local-model work (through disposition) stands on its own; the
source model unlocks commit and amend; the forge layer trails.

The authoritative build order, with each layer's file-level workitems, tests,
and PR boundaries, is the companion phased plan, not this section. Where they
mention the same thing the plan is current; this paragraph is only the shape.

## Implementation map

Where the concepts live in the tree today, so a later reader can resume without
rediscovering it. All record and fold types are in `crates/wiff-core`; commands
are in `crates/wiff/src/command`.

| Concept | File(s) |
| --- | --- |
| `RecordBody`, `SourceKind`, `DiffVersionRecord`, the new records | `wiff-core/src/record.rs` |
| `fold`, `ReviewState`, `CommentState` | `wiff-core/src/review.rs` |
| Comment creation and mutation helpers | `wiff-core/src/comment.rs` |
| Reanchor authorship on refresh | `wiff-core/src/refresh.rs`, `wiff-core/src/rebase.rs` |
| Scm selector, base-rule resolution, `DiffSource` | `wiff-core/src/source/` |
| `ChangeId` minting and the `Change-Id:` trailer | `wiff-core/src/identity.rs` |
| Session load/save | `wiff-core/src/session.rs` |
| `--change` / `--description` / stdin front matter | `wiff/src/command/new.rs` |
| `wiff commit`, `wiff forge` (pull/push/status; new files) | `wiff/src/command/` |
| The global `base_revision_rules` setting | `crates/wiff-config` |
| Agent read/write access | `wiff/src/command/skill.rs` |
| TUI rendering of replies, disposition, description | `wiff/src/tui.rs`, `wiff/src/render/` |

What the cutover removes, since the format may break freely:

- `CommentRecord` and the separate `CommentEdit` / `CommentResolve` /
  `CommentDelete` / `CommentReanchor` bodies are deleted; `CommentEvent`
  replaces all of them.
- `SourceKind::{GitWorktree, GitIndex, GitRev}` (in `record.rs`) collapse into
  `Scm(ScmSource)` with an `ScmSelector`.
- `FORMAT_VERSION` bumps at stage 1 and again at each later stage that adds a
  record variant; there is no migration and no compatibility shim, and a log
  whose version does not match the binary is discarded rather than read.
- `Confidence` (already in `record.rs`) is pre-existing and unchanged; it moves
  onto `CommentState` as shown but is not a new concept.

The reader-facing outputs that change per stage, since a model the consumers
cannot see is invisible: the agent skill (`skill.rs`) and its JSON must expose
replies, disposition, description, and per-event actor and time; the TUI must
render them. Each stage ships with fold tests that assert the full rendered
`ReviewState` (never a single field) and cover the corruption cases: a reply to
an unknown id, and an event that arrives before the comment it references.

## Stacks and Depends-On (anticipated)

A wiff session is always exactly one reviewable unit with one change id. A stack
is not a session that holds several changes; it is a graph of related sessions,
each a unit in its own right, linked by dependency edges. This is the model arc
and Diffusion use and what GitHub approximates with a set of related pull
requests. None of it is v1; this section reserves the data model so the v1
record does not have to change when stacks arrive.

**Edges are keyed by change id.** A dependency names the change it depends on by
change id, never by commit hash, because a hash moves on every amend while the
change id is stable. A `Depends-On:` trailer records the edge in the commit
message; a change with several parents (a true DAG) writes one `Depends-On:`
line per parent change id, one edge each. The trailer is durable
and visible to collaborators and other tools without wiff, the same way
`Change-Id` is.

**The edge is mutable, so the session log holds it, not the trailer.** Unlike
`Change-Id`, a dependency changes routinely: a section gets deferred, or a peer
reworks a parent, and the user re-parents. Treating the trailer as the source of
truth would make every re-parent an amend of the commit message, rewriting the
commit and cascading a rebase onto every descendant. So the primary record is a
review-level `SetDependencies { on: Vec<ChangeId> }` event, folded latest-wins
with its actor and time like any other event; a re-parent is then just a new
event, cheap and reversible. The trailer is a projection of the current set,
written out when the user next runs `wiff commit` or `wiff amend`. Intent is
recorded immediately in the log, and the durable trailer catches up at the next
commit the user was going to make anyway.

**The graph is derived, not stored.** There is no central registry. Each session
records only its own out-edges, what it depends on, never who depends on it,
which is non-local knowledge. `wiff stack show` assembles the DAG in memory by
scanning the local sessions and reading each one's change id and dependency set,
and marks the current position by matching the checked-out commit's change id to
a session.

**Edges are captured automatically and kept distinct from the base rule.** When
`wiff new` starts a review whose base commit already has a change id, the new
session records a dependency on that base change id. The dependency is separate
from the base ruleset: the ruleset decides what the diff is taken against, the
dependency records ordering and navigation. They usually agree in a clean stack
but answer different questions, so neither is derived from the other.

**Propagation is the scm's job; wiff orchestrates it.** When a reviewer edits a
change in the middle of a stack and the user pulls and folds the fix with `wiff
amend`, that one node changes and its descendants go stale. Repairing them is an
explicit `wiff stack restack`, never a side effect of amend, which keeps the
one-node invariant intact. restack walks the derived DAG and, for each stale
descendant, drives the scm's own re-parent: automatic under jj and Sapling,
`git rebase --update-refs` under recent git. As it re-parents, restack also
rewrites each affected change's `Depends-On:` trailers to name the new parent
change ids, keeping the durable metadata in step with the reshaped graph. Where
the scm cannot do it (older
git without `--update-refs`), wiff reports the stale descendants and stops rather
than reimplementing a rebase engine, which keeps wiff a review tool rather than a
partial Graphite.

The v2 stack commands this anticipates: `wiff stack show`, `next` and `prev` to
move up and down the graph by checking out the neighboring change, `restack` to
propagate an amended change and reconcile a re-parented DAG, and `push` and
`pull` to reconcile local edits against the several pull requests a stack maps to
on the forge. Precise behavior is deferred to that phase.

## Future work

These are designed, not yet built. They are settled enough that v1 reserves the
data model for them and does not have to change to accommodate them later.

- **Stacks (v2).** First-class stacks are a graph of related sessions linked by
  `Depends-On`, as reserved above under "Stacks and Depends-On": the mutable
  edge held in the session log and projected to the trailer, the derived DAG,
  `wiff stack show` / `next` / `prev` / `restack` / `push` / `pull`, and the
  scm-driven propagation. Deliberately sequenced after single-unit review is
  solid.
- **Per-commit view.** The version picker's isolated per-commit rendering,
  described under "Per-commit view", is designed down to the anchor and
  publish-back behavior; what remains for the build is display detail (the
  endpoints a selected commit diffs between, and how a merge commit reads),
  which changes no persisted state.
