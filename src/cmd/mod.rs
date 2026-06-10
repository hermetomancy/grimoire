//! Thin CLI command handlers with no subsystem of their own: each module is one `grm`
//! command (or a small family) dispatched from `main`.

pub(crate) mod clean;
pub(crate) mod doctor;
pub(crate) mod files;
pub(crate) mod man;
pub(crate) mod prefer;
pub(crate) mod query;
pub(crate) mod setup;
