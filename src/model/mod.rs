//! The data model: typed representations of Grimoire's on-disk NUON documents.
//!
//! Package metadata, dependency requirements, package indexes, installed-package state, tome
//! manifests, and the lockfile all live here, with `from_value`/`to_value` conversions to and
//! from Nushell `Value`s. Construction validates structure (names, targets, semver) so the rest
//! of the codebase works with already-checked data. Split per concern: see each submodule.

mod catalog;
mod deps;
mod index;
mod package;
pub(crate) mod preferences;
mod state;
mod value;

pub use catalog::*;
pub use deps::*;
pub use index::*;
pub use package::*;
pub use state::*;
pub use value::*;
