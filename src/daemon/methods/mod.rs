//! Control-verb handlers, grouped by domain.
//!
//! Each module registers its verbs through `pub const` [`MethodDef`]s listed
//! in [`crate::daemon::router::METHODS`]. New verb families (input, spawn,
//! waits, diffs) get their own module here.

pub mod changes;
pub mod input;
pub mod output;
pub mod portal;
pub mod spawn;
pub mod status;
pub mod tree;
pub mod waits;
pub mod windows;
pub mod workspace;
