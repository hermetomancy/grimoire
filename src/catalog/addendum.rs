//! Disabled addendum command stubs.
//!
//! Addenda can mutate package metadata, which makes them part of the resolver/build/hash contract.
//! That design is not settled, so the public command group is kept as a stub instead of applying
//! partial overlays.

use anyhow::{Result, bail};

use crate::cli::{TomeAddArgs, TomeRemoveArgs, TomeUpdateArgs};

fn disabled() -> Result<()> {
    bail!("addenda are disabled while the overlay design is reworked")
}

pub fn add(_args: TomeAddArgs) -> Result<()> {
    disabled()
}

pub fn remove(_args: TomeRemoveArgs) -> Result<()> {
    disabled()
}

pub fn list() -> Result<()> {
    disabled()
}

pub fn update(_args: TomeUpdateArgs) -> Result<()> {
    disabled()
}
