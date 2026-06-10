//! The rune command set: Grimoire's curated replacement for `nu-command`.
//!
//! Runes target a *defined subset* of Nushell, documented in docs/rune-authoring.md — the
//! core language from `nu-cmd-lang` plus the commands registered here. This is deliberate:
//! `nu-command` drags ~200 crates (spreadsheets, clipboard, compression formats, …) that no
//! build script needs, and a fixed surface keeps rune behavior stable across nu upgrades.
//! Adding a command here means documenting it in rune-authoring.md in the same commit
//! (AGENTS.md §15.4).

mod external;
mod fs;
mod values;

use nu_protocol::engine::{EngineState, StateWorkingSet};

/// Registers the rune command set on top of the core-language context, mirroring the shape
/// of `nu_command::add_shell_command_context`.
pub(crate) fn add_rune_command_context(mut engine_state: EngineState) -> EngineState {
    let delta = {
        let mut working_set = StateWorkingSet::new(&engine_state);
        macro_rules! bind {
            ($($command:expr),* $(,)?) => {
                $(working_set.add_decl(Box::new($command));)*
            };
        }
        bind!(
            // system
            external::External,
            external::Complete,
            // filesystem
            fs::Mkdir,
            fs::Save,
            fs::Open,
            fs::Rm,
            fs::Cp,
            fs::Ls,
            fs::Cd,
            // path
            values::PathSelf,
            values::PathJoin,
            values::PathExists,
            values::PathType,
            values::PathBasename,
            values::PathDirname,
            // strings
            values::StrSelf,
            values::StrStartsWith,
            values::StrEndsWith,
            values::StrTrim,
            values::StrReplace,
            // filters
            values::Get,
            values::Merge,
            values::Columns,
            values::Lines,
            values::First,
            values::IsEmpty,
        );
        working_set.render()
    };

    if let Err(err) = engine_state.merge_delta(delta) {
        // Registration is static; a failure here is a programming error, not user input.
        eprintln!("error registering rune commands: {err:?}");
    }
    engine_state
}
