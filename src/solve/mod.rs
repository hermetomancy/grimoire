//! Version-aware dependency resolution.
//!
//! Given one or more root requirements, the resolver picks a concrete version for every package
//! in the runtime dependency graph that satisfies all accumulated semver requirements. Candidate
//! versions for a package merge, per version, the prebuilt archives a tome's index offers for the
//! current target with the source rune that defines that version (the rune being authoritative for
//! its runtime dependencies); the highest satisfying version is preferred. Selection backtracks when
//! a choice cannot satisfy a transitive requirement. The result is an install plan ordered so
//! dependencies precede dependents — each step carrying its rune and the prebuilt substitutes the
//! installer then chooses between by store hash.

mod candidates;
mod capabilities;
mod plan;
mod resolver;

pub(crate) use candidates::*;
pub use capabilities::*;
pub use plan::*;
pub use resolver::*;
