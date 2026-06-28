//! Command-line interface: the `clap` types that parse `grm`'s arguments into typed commands.
//!
//! Each subcommand maps to an `Args` struct consumed by the matching module entry point (see
//! `main.rs` for dispatch). Doc comments on the fields double as the `--help` text users see.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "grm")]
#[command(about = "Git-native package manager with Nushell/NUON package definitions")]
#[command(
    long_about = "Grimoire installs packages from tomes: git repositories of Nushell `.rn` \
package definitions and pre-built binary archives. A bare package name is resolved against \
your configured tomes, preferring a verified binary archive for the current target and \
falling back to a source build. Installs are transactional and verified before extraction; \
nothing requires administrator privileges."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Suppress progress and result confirmations. Explicitly requested data (list/search/info/
    /// doctor output) and errors are still printed. Mutually exclusive with `--verbose`.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Print granular step-by-step progress on stderr on top of the normal output.
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    // -----------------------------------------------------------------------
    // Package shortcuts — the common `pkg` operations, mirrored at the root for
    // convenience. Each shares its Args and handler with the matching `grm pkg <verb>`.
    // -----------------------------------------------------------------------
    /// Install a package by name, from a local archive, or from a rune.
    #[command(visible_aliases = ["ins", "add"])]
    Install(InstallArgs),
    /// Upgrade installed packages to the latest available version. Packages held with
    /// `grm pkg hold` are skipped (or, if named explicitly, refused with an error).
    #[command(visible_alias = "up")]
    Upgrade(UpgradeArgs),
    /// Remove an installed package and any runtime dependencies nothing else needs (same
    /// transaction). Store contents are untouched (so switching back still works); `grm clean`
    /// reclaims the disk space.
    #[command(visible_aliases = ["rm", "del"])]
    Remove(MutatePackagesArgs),
    /// List installed packages with their versions and targets. By default only the linked
    /// environment is shown; `--all` includes store-only packages (cached build deps).
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Search configured tomes for packages.
    #[command(visible_alias = "sea")]
    Search(QueryArg),
    /// Show detailed information about a package.
    Info(PackageArg),
    /// Build a package archive from a rune without installing it.
    Build(BuildArgs),

    // -----------------------------------------------------------------------
    // Noun groups
    // -----------------------------------------------------------------------
    /// Package operations: the full set, including the root shortcuts above plus hold/unhold,
    /// file-ownership queries, and provider preference.
    Pkg {
        #[command(subcommand)]
        command: PkgCommand,
    },
    /// Manage tomes: the git repositories packages are resolved from.
    Tome {
        #[command(subcommand)]
        command: TomeCommand,
    },
    /// Manage addenda: data-only overlays that patch tome rune definitions.
    #[command(visible_alias = "ad")]
    Addendum {
        #[command(subcommand)]
        command: AddendumCommand,
    },
    /// Manage generations: the environment snapshots every mutating command records, and
    /// switching between them.
    #[command(visible_alias = "gen")]
    Generation {
        #[command(subcommand)]
        command: GenerationCommand,
    },

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------
    /// Check the health of the grimoire installation.
    #[command(visible_alias = "dr")]
    Doctor,
    /// Reclaim disk under the install root: sweep unused dependencies nothing references
    /// (cached build deps, residue from failed installs), delete old generations (keeping the
    /// most recent ones plus the switch-back target), delete store paths no retained generation
    /// references, and empty the source/archive/build caches and leftover transaction staging
    /// directories. Installed packages, recent generations, tomes, addenda, and the lockfile
    /// are untouched; the next install or build re-fetches what it needs.
    Clean(CleanArgs),
    /// Create the fixed Grimoire store directory (/grm on POSIX systems).
    /// On Linux this creates the directory and adjusts ownership. On macOS it registers
    /// the directory in /etc/synthetic.conf and prompts for a reboot.
    #[command(visible_alias = "st")]
    Setup(SetupArgs),

    // -----------------------------------------------------------------------
    // Hidden
    // -----------------------------------------------------------------------
    /// Print the content-addressed store hash a package resolves to (its rune plus its runtime
    /// dependency closure). The address a prebuilt must carry to be a valid substitute.
    #[command(hide = true)]
    StoreHash(PackageArg),
    /// Print a shell completion script for `grm` to stdout. Redirect it where your shell
    /// expects completions (e.g. `grm completions bash > ~/.local/share/bash-completion/completions/grm`).
    Completions(CompletionsArgs),
    /// Render man pages for `grm` and every subcommand into a directory.
    #[command(hide = true)]
    Man(ManArgs),
}

/// `grm pkg <command>`: the full set of package operations. The common ones (install, upgrade,
/// remove, list, search, info, build) are also exposed directly at the root.
#[derive(Debug, Subcommand)]
pub enum PkgCommand {
    /// Install a package by name, from a local archive, or from a rune.
    Install(InstallArgs),
    /// Upgrade installed packages to the latest available version. Packages held with
    /// `grm pkg hold` are skipped (or, if named explicitly, refused with an error).
    Upgrade(UpgradeArgs),
    /// Remove an installed package. Runtime dependencies installed solely for this package —
    /// that no other installed package still requires — leave in the same transaction. A
    /// package something still requires is kept and demoted to dependency status instead, so
    /// it is removed automatically once nothing needs it. Store contents are untouched (so
    /// switching back still works); `grm clean` reclaims the disk space.
    Remove(MutatePackagesArgs),
    /// List installed packages with their versions and targets. By default only the linked
    /// environment is shown; `--all` includes store-only packages (cached build deps).
    List(ListArgs),
    /// Search configured tomes for packages.
    Search(QueryArg),
    /// Show detailed information about a package.
    Info(PackageArg),
    /// Build a package archive from a rune without installing it.
    Build(BuildArgs),
    /// Hold an installed package back from `grm upgrade` until it is released.
    #[command(visible_alias = "pin")]
    Hold(MutatePackagesArgs),
    /// Release a held package so it is eligible for `grm upgrade` again.
    #[command(visible_alias = "unpin")]
    Unhold(MutatePackagesArgs),
    /// List the files an installed package placed in the store.
    Files(PackageArg),
    /// Show which installed package owns a file (a store path or a profile path).
    Owns(OwnsArgs),
    /// Show which packages provide a command or capability, installed or available.
    Provides(ProvidesArgs),
    /// Choose which package provides a contested capability or bin (e.g. `grm pkg prefer awk gawk`).
    /// With no arguments, lists preferences and currently contested capabilities. Note that
    /// `install --locked` still pins concrete providers from the lockfile; a changed preference
    /// cannot override a locked install.
    Prefer(PreferArgs),
}

/// `grm generation <command>`: manage generations — the environment snapshots every mutating
/// command records, and switching between them.
#[derive(Debug, Subcommand)]
pub enum GenerationCommand {
    /// List generations.
    #[command(visible_alias = "ls")]
    List,
    /// Switch to a generation by ID (forward or back), or to the previous generation when no ID
    /// is given. Switching only re-points the profile and restores that generation's recorded
    /// state — nothing is rebuilt.
    #[command(visible_alias = "sw")]
    Switch(SwitchArgs),
    /// Export the current generation's lockfile (its packages, versions, and hashes) to a path,
    /// for sharing or reproducing the set elsewhere. The inverse of `restore --lockfile`.
    Lock(GenerationLockArgs),
    /// Restore the package set a lockfile records: install every requested package at its
    /// pinned version and hash, restore requested/held intent, and sweep anything the lock
    /// does not account for. Tomes must already be configured (and, for git tomes, synced at
    /// the lock's pinned commits).
    Restore(RestoreArgs),
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Target shell.
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct ManArgs {
    /// Output directory for the generated `*.1` files. Created if missing.
    #[arg(short, long, default_value = "target/grimoire-man")]
    pub output: PathBuf,
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Rune to build: a known package name (resolved from configured tomes) or a path to a
    /// `.rn` file. Declared sources are fetched and checksum-verified before the build runs.
    pub package: String,
    /// Directory to write the built archive into. The archive is named
    /// `<name>-<version>-<target>.tar.zst`.
    #[arg(short, long, default_value = "target/grimoire-packages")]
    pub output: PathBuf,
    /// Allow host build-tool discovery instead of using only the grimoire-managed toolchain.
    /// This is useful for bootstrapping before the managed core userland is installed.
    #[arg(long)]
    pub bootstrap: bool,
    /// Drop the POSIX ambient PATH tail (`/usr/bin`, `/bin`) so the build sees only declared
    /// deps and the managed core floor. Diagnostic for stage-2 self-hosting: a build that fails
    /// for a missing tool names a host-userland leak to package. Affects the store hash.
    #[arg(long, conflicts_with = "bootstrap")]
    pub hermetic: bool,
    /// Target triple to build for (defaults to the host target).
    #[arg(short, long)]
    pub target: Option<String>,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Packages to install: bare names (resolved from tomes, preferring verified binary
    /// archives for this target), paths to local `.tar.zst` archives, or `.rn` runes to
    /// build from source. Runtime dependencies are installed automatically.
    #[arg(num_args = 1..)]
    pub packages: Vec<String>,
    /// Build from source even when a pre-built binary archive is available. Build
    /// dependencies are installed first.
    #[arg(short = 's', long)]
    pub from_source: bool,
    /// Expected archive hash (`sha256:<hex>` or bare hex). When set, the archive is verified
    /// against it before being read or extracted; a mismatch is a hard failure. Only valid
    /// when installing a single local archive.
    #[arg(long = "sha256")]
    pub sha256: Option<String>,
    /// Reproduce the install recorded in `grimoire.lock.nuon`: every package in the resolved
    /// graph must match a locked version and archive hash, and nothing outside the lockfile may
    /// be pulled in. Fails if no lockfile exists. Ignored for a local-archive install.
    #[arg(long)]
    pub locked: bool,
    /// Resolve and print the install plan without touching state. Shows each package the
    /// solver chose, its version, and whether it would come from a binary archive or a
    /// source build.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct PackageArg {
    /// Names of the installed packages to operate on.
    #[arg(num_args = 1..)]
    pub packages: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Lockfile to restore from. Defaults to the install root's `state/grimoire.lock.nuon`.
    #[arg(long)]
    pub lockfile: Option<std::path::PathBuf>,
    /// Show what would be restored and swept without changing anything.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

/// Packages named for a mutating command, plus the shared preview flag.
#[derive(Debug, Args)]
pub struct MutatePackagesArgs {
    pub packages: Vec<String>,
    /// Show what would change without changing anything.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Include store-only packages (cached build deps, residue kept for reuse) alongside the
    /// linked environment.
    #[arg(short, long)]
    pub all: bool,
    /// List only packages you explicitly installed (the requested roots), excluding
    /// dependencies pulled in to satisfy them. This is the set `grm install` would rebuild.
    #[arg(short, long, conflicts_with = "all")]
    pub explicit: bool,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Show what setup would do without touching the system.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
    /// After store setup, add the core/world tomes and install grimoire through itself.
    #[arg(long)]
    pub bootstrap: bool,
}

#[derive(Debug, Args)]
pub struct PreferArgs {
    /// Capability or bin name to set the preferred provider for. Omit to list preferences.
    pub capability: Option<String>,
    /// Package that should provide the capability.
    pub package: Option<String>,
    /// Show what the preference change would do without applying it.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
    /// Clear the preference for the capability instead of setting one.
    #[arg(long)]
    pub unset: bool,
}

#[derive(Debug, Args)]
pub struct OwnsArgs {
    /// File path to resolve to its owning package. Profile paths are followed through the
    /// `current` symlink; store paths are matched directly.
    pub path: std::path::PathBuf,
}

#[derive(Debug, Args)]
pub struct ProvidesArgs {
    /// Package name, command name, or capability to look up providers for.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct QueryArg {
    /// Search term matched against package names and summaries across configured tomes.
    pub query: String,
}

#[derive(Debug, Args)]
pub struct UpgradeArgs {
    /// Packages to upgrade. If omitted, every installed package is upgraded.
    pub packages: Vec<String>,
    /// Show which packages would be upgraded and to which version, without touching state.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct SwitchArgs {
    /// Generation ID to activate. Omit to switch to the previous generation.
    pub generation: Option<u64>,
    /// Show the target generation and the package diff without switching.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct GenerationLockArgs {
    /// Path to write the lockfile to. Defaults to `grimoire.lock.nuon` in the current directory.
    #[arg(short, long, default_value = "grimoire.lock.nuon")]
    pub output: PathBuf,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Number of recent generations to keep (including the current one). The switch-back target
    /// is always kept.
    #[arg(short, long, default_value = "5")]
    pub keep: usize,
    /// Show what would be reclaimed without deleting anything.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum TomeCommand {
    /// Scaffold a new tome (manifest, `runes/`, `sources/`, empty index) in a directory.
    Init(TomeInitArgs),
    /// Scaffold a new rune (package definition) in a tome's `runes/` directory.
    Rune(TomeRuneArgs),
    /// Build a rune into a `.tar.zst` archive in the tome's package repo and register it in
    /// the tome's `index.nuon`, so the prebuilt package can be published from the tome.
    Build(TomeBuildArgs),
    /// Add a tome by cloning a git repository containing a `tome.rn` manifest at its root.
    /// The tome is registered under the name declared in that manifest.
    Add(TomeAddArgs),
    /// Sync configured tomes, fetching the latest commit for their tracked ref.
    #[command(visible_aliases = ["up", "sync"])]
    Update(TomeUpdateArgs),
    /// Remove a configured tome and its cached repository.
    #[command(visible_alias = "rm")]
    Remove(TomeRemoveArgs),
    /// List configured tomes with their URLs and tracked refs.
    #[command(visible_alias = "ls")]
    List,
    /// Read tome news items (`news/*.md` in the tome repository). By default prints unread
    /// items in full and marks them seen; `--all` re-reads everything without touching the
    /// seen marker.
    News(TomeNewsArgs),
    /// Show a tome's details: URL, tracked ref, pinned commit, description, and signer keys.
    Info(TomeInfoArgs),
}

#[derive(Debug, Args)]
pub struct TomeInfoArgs {
    /// Tome to show.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct TomeNewsArgs {
    /// Tome to read news from. If omitted, reads news from every configured tome.
    pub name: Option<String>,
    /// Print every news item, including already-seen ones, without advancing the marker.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct TomeInitArgs {
    /// Name the tome registers itself under. Must be a valid identifier (letters, digits,
    /// and `_.+-`, starting with a letter or digit).
    pub name: String,
    /// Directory to create the tome in. Created if missing; defaults to the current directory.
    #[arg(short, long, default_value = ".")]
    pub path: PathBuf,
    /// One-line description recorded in the tome manifest.
    #[arg(short, long)]
    pub description: Option<String>,
}

#[derive(Debug, Args)]
pub struct TomeRuneArgs {
    /// Package name for the new rune. Becomes `runes/<name>.rn` and the package's `name`.
    pub name: String,
    /// Tome directory to add the rune to (must contain `tome.rn`). Defaults to the current
    /// directory.
    #[arg(short, long, default_value = ".")]
    pub path: PathBuf,
    /// Initial package version recorded in the rune.
    #[arg(long, default_value = "0.1.0")]
    pub version: String,
}

#[derive(Debug, Args)]
pub struct TomeBuildArgs {
    /// Name of the rune to build, resolved as `runes/<name>.rn` within the tome. Omit it and
    /// pass `--all` to build every rune in the tome instead.
    pub package: Option<String>,
    /// Build every rune in the tome's `runes/` directory, registering each in the index. Cannot
    /// be combined with a named package.
    #[arg(long, conflicts_with = "package")]
    pub all: bool,
    /// Tome directory containing the rune (must contain `tome.rn`). Defaults to the current
    /// directory.
    #[arg(short, long, default_value = ".")]
    pub path: PathBuf,
    /// Allow host build-tool discovery instead of using only the grimoire-managed toolchain.
    #[arg(long)]
    pub bootstrap: bool,
    /// Drop the POSIX ambient PATH tail (`/usr/bin`, `/bin`) so each rune builds against only
    /// declared deps and the managed core floor. Diagnostic for stage-2 self-hosting: a rune
    /// that fails for a missing tool names a host-userland leak to package. Affects the store hash.
    #[arg(long, conflicts_with = "bootstrap")]
    pub hermetic: bool,
    /// Target triple to build for (defaults to the host target).
    #[arg(short, long)]
    pub target: Option<String>,
    /// Rebuild the binary package index (`index.nuon`) from existing archives in `dist/`
    /// without building any packages.
    #[arg(long, conflicts_with_all = ["package", "all"])]
    pub index: bool,
    /// Rebuild runes even if they already exist in the index.
    #[arg(long, conflicts_with = "index")]
    pub force: bool,
    /// Fail the build when the purity lint finds baked host paths (default: warn only).
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Args)]
pub struct TomeAddArgs {
    /// Git URL (or local path) of the repository to clone. The tome is registered under the
    /// `name` declared in its `tome.rn` manifest.
    pub git_url: String,
    /// Git ref (branch, tag, or commit) to track.
    #[arg(short = 'r', long = "ref", default_value = "main")]
    pub ref_name: String,
    /// Pin a minisign public key (base64) for this tome, skipping trust-on-first-use.
    /// May be given multiple times for multi-key setups.
    #[arg(long = "signer", action = clap::ArgAction::Append)]
    pub signer: Vec<String>,
    /// Show what would be added without cloning or registering anything.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct TomeUpdateArgs {
    /// Tome to update. If omitted, every configured tome is updated.
    pub name: Option<String>,
    /// Show what would be synced without fetching anything.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct TomeRemoveArgs {
    /// Name of the configured tome to remove.
    pub name: String,
    /// Show what removal would affect without removing anything.
    #[arg(long, visible_alias = "explain")]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum AddendumCommand {
    /// Add an addendum by cloning a git repository of data-only rune overlays.
    Add(TomeAddArgs),
    /// Remove a configured addendum.
    #[command(visible_alias = "rm")]
    Remove(TomeRemoveArgs),
    /// List configured addenda.
    #[command(visible_alias = "ls")]
    List,
    /// Sync configured addenda, fetching the latest commit for their tracked ref.
    #[command(visible_aliases = ["up", "sync"])]
    Update(TomeUpdateArgs),
}
