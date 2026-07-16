//! The wiff core: the append-only session [`session`] log, its [`record`]
//! schema, and project [`identity`] resolution. This crate owns persistence and
//! discovery; diff parsing and the diff model come from `wiff-diff`.

pub mod capture;
pub mod comment;
pub mod compare;
pub mod config;
pub mod description;
pub mod draft;
pub mod error;
pub mod hash;
pub mod identity;
pub mod rebase;
pub mod record;
pub mod refresh;
pub mod review;
pub mod session;
pub mod source;

pub use capture::{create_session, write_diff_version};
pub use comment::{AddedComment, DraftComment, delete_comment, set_disposition, set_resolved};
pub use compare::{Comparison, LineOrigin, compare_versions};
pub use config::AuthorDefaults;
pub use description::set_description;
pub use draft::{DraftBuffer, EffectiveComment, EffectiveDescription, draft_create};
pub use error::{Error, Result};
pub use hash::SidebandHash;
pub use identity::{ProjectIdentity, ScmType};
pub use rebase::{RebaseOutcome, rebase_line_comment};
pub use refresh::{RefreshOutcome, refresh_session};
pub use review::{CommentState, DescriptionState, ReviewState, fold};
pub use session::{LockWait, SessionLock, SessionLog};
pub use source::{CapturedDiff, DiffSource, GitSource};
