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
    // Packages
    // -----------------------------------------------------------------------
    /// Build a package archive from a rune without installing it.
    #[command(visible_alias = "b")]
    Build(BuildArgs),
    /// Install a package by name, from a local archive, or from a rune.
    #[command(visible_aliases = ["in", "i"])]
    Install(InstallArgs),
    /// Remove an installed package. Runtime dependencies installed solely for this package —
    /// that no other installed package still requires — are removed too.
    #[command(visible_aliases = ["rm", "uninstall"])]
    Remove(PackageArg),
    /// Upgrade installed packages to the latest available version. Packages held with
    /// `grm hold` are skipped (or, if named explicitly, refused with an error).
    #[command(visible_alias = "up")]
    Upgrade(UpgradeArgs),
    /// Hold an installed package back from `grm upgrade` until it is released.
    #[command(visible_alias = "pin")]
    Hold(PackageArg),
    /// Release a held package so it is eligible for `grm upgrade` again.
    #[command(visible_alias = "unpin")]
    Unhold(PackageArg),

    // -----------------------------------------------------------------------
    // Query
    // -----------------------------------------------------------------------
    /// List installed packages with their versions and targets.
    #[command(visible_alias = "ls")]
    List,
    /// Search configured tomes for packages.
    #[command(visible_aliases = ["s", "find"])]
    Search(QueryArg),
    /// Show detailed information about a package.
    Info(PackageArg),

    // -----------------------------------------------------------------------
    // Profiles
    // -----------------------------------------------------------------------
    /// Roll back to the previous generation.
    #[command(visible_alias = "rb")]
    Rollback,
    /// Switch to a specific generation.
    #[command(visible_alias = "sw")]
    Switch(SwitchArgs),
    /// List generations.
    #[command(visible_alias = "gens")]
    Generations,
    /// Garbage-collect unreferenced store paths and old generations.
    #[command(visible_alias = "gc")]
    CollectGarbage(GcArgs),
    /// Delete a specific generation by ID. The currently active generation cannot be deleted.
    #[command(visible_alias = "del-gen")]
    DeleteGeneration(DeleteGenerationArgs),

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------
    /// Check the health of the grimoire installation.
    #[command(visible_alias = "dr")]
    Doctor,
    /// Reclaim disk under the install root by emptying the source/archive/build caches and
    /// any leftover transaction staging directories. Installed packages, profiles, state, tomes,
    /// addenda, and the lockfile are untouched; the next install will re-fetch what it needs.
    Clean,
    /// Create the fixed Grimoire store directory (/grm on Unix, C:\grm on Windows).
    /// On Linux this creates the directory and adjusts ownership. On macOS it registers
    /// the directory in /etc/synthetic.conf and prompts for a reboot.
    #[command(visible_alias = "st")]
    Setup,

    // -----------------------------------------------------------------------
    // Catalogs
    // -----------------------------------------------------------------------
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

    // -----------------------------------------------------------------------
    // Hidden
    // -----------------------------------------------------------------------
    /// Print the content-addressed store hash a package resolves to (its rune plus its runtime
    /// dependency closure). The address a prebuilt must carry to be a valid substitute.
    StoreHash(PackageArg),
    /// Print a shell completion script for `grm` to stdout. Redirect it where your shell
    /// expects completions (e.g. `grm completions bash > ~/.local/share/bash-completion/completions/grm`).
    Completions(CompletionsArgs),
    /// Render man pages for `grm` and every subcommand into a directory.
    Man(ManArgs),
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
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Package to install: a bare name (resolved from tomes, preferring a verified binary
    /// archive for this target), a path to a local `.tar.zst` archive, or a `.rn` rune to
    /// build from source. Runtime dependencies are installed automatically.
    pub package: String,
    /// Build from source even when a pre-built binary archive is available. Build
    /// dependencies are installed first.
    #[arg(short = 's', long)]
    pub from_source: bool,
    /// Expected archive hash (`sha256:<hex>` or bare hex). When set, the archive is verified
    /// against it before being read or extracted; a mismatch is a hard failure.
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
    /// Name of the installed package to operate on.
    pub package: String,
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
    /// Generation ID to activate.
    pub id: u64,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Number of recent generations to keep (including the current one).
    #[arg(short, long, default_value = "5")]
    pub keep: usize,
}

#[derive(Debug, Args)]
pub struct DeleteGenerationArgs {
    /// Generation ID to delete.
    pub id: u64,
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
    /// Rebuild the binary package index (`index.nuon`) from existing archives in `dist/`
    /// without building any packages.
    #[arg(long, conflicts_with_all = ["package", "all"])]
    pub index: bool,
}

#[derive(Debug, Args)]
pub struct TomeAddArgs {
    /// Git URL (or local path) of the repository to clone. The tome is registered under the
    /// `name` declared in its `tome.rn` manifest.
    pub git_url: String,
    /// Git ref (branch, tag, or commit) to track.
    #[arg(short = 'r', long = "ref", default_value = "main")]
    pub ref_name: String,
}

#[derive(Debug, Args)]
pub struct TomeUpdateArgs {
    /// Tome to update. If omitted, every configured tome is updated.
    pub name: Option<String>,
}

#[derive(Debug, Args)]
pub struct TomeRemoveArgs {
    /// Name of the configured tome to remove.
    pub name: String,
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
