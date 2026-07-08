//! The wiff core: the append-only session [`session`] log, its [`record`]
//! schema, and project [`identity`] resolution. This crate owns persistence and
//! discovery; diff parsing and the diff model come from `wiff-diff`.

pub mod error;
pub mod hash;
pub mod identity;
pub mod record;
pub mod session;

pub use error::{Error, Result};
pub use hash::SidebandHash;
pub use identity::ProjectIdentity;
pub use session::{SessionLock, SessionLog};
