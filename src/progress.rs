/// Progress reporting shared by every command. Progress goes to stderr so stdout carries only
/// command results (AGENTS.md §7); both helpers respect `--quiet`.
pub fn status(quiet: bool, message: &str) {
    if !quiet {
        eprintln!("grimoire: {message}");
    }
}

pub fn success(quiet: bool, message: &str) {
    if !quiet {
        eprintln!("grimoire: ok: {message}");
    }
}
