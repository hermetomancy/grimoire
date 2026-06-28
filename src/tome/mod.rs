//! Tomes: the git-backed catalogs of runes packages are installed from.
//!
//! This module adds, updates, lists, and removes tomes (cloning and pinning via [`git`]), reads
//! their manifests and package indexes, and authors/publishes them: `tome init`/`tome rune`
//! scaffold a new catalog, `tome build` compiles runes into verified archives recorded in a
//! git-untracked `dist/index.nuon` served either from a local path or over HTTP, `tome lint`
//! validates a local tome's runes and manifest before publishing, and `tome sign` writes and
//! signs the `runes-manifest.nuon` plus every built archive with a minisign secret key.

pub(crate) mod git;
pub(crate) mod news;

mod authoring;
mod lint;
mod output_lint;
mod publish;
mod sign;
mod sync;
mod verify;

pub use authoring::*;
pub use lint::*;
pub use publish::*;
pub use sign::*;
pub use sync::*;
pub use verify::*;
