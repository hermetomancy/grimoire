//! The embedded Nushell layer: the only place that touches the Nushell engine.
//!
//! [`nuon_io`] reads and writes inert NUON state documents, and [`runtime`] evaluates `.rn`
//! package/tome definitions and runs build steps in-process (AGENTS.md §4). Everything else in
//! the crate works with the typed `model` values these produce, never the engine directly.

pub mod nuon_io;
pub mod runtime;
