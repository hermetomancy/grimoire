//! Tomes: the git-backed catalogs of runes packages are installed from.
//!
//! This module adds, updates, lists, and removes tomes (cloning and pinning via [`git`]), reads
//! their manifests and package indexes, and authors/publishes them: `tome init`/`tome rune`
//! scaffold a new catalog, and `tome build` compiles runes into verified archives recorded in a
//! git-untracked `dist/index.nuon` served either from a local path or over HTTP.

pub(crate) mod git;
pub(crate) mod news;

mod authoring;
mod lint;
mod publish;
mod sync;
mod verify;

pub use authoring::*;
pub use publish::*;
pub use sync::*;
pub use verify::*;
