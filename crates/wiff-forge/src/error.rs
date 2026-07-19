//! The one forge failure the neutral layer reasons about.
//!
//! Every forge operation returns [`anyhow::Result`], so an adapter attaches
//! whatever context it likes (a 401 names a bad token, a 403 names the missing
//! permission) without a fixed taxonomy above it. The single exception is a
//! forge declining an operation it does not implement: that is a concrete
//! [`Unsupported`] error the push orchestration downcasts to and reports as not
//! propagated rather than failing the push.

/// The error a forge returns from an operation it does not implement. The
/// method that returns it identifies which operation was declined; the push
/// orchestration downcasts to this to skip that operation and report it as not
/// propagated. An adapter may layer `anyhow` context on top and the downcast
/// finds it through the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this forge does not support this operation")]
pub struct Unsupported;
