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
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build a package archive from a rune without installing it.
    #[command(visible_alias = "b")]
    Build(BuildArgs),
    /// Install a package by name, from a local archive, or from a rune.
    #[command(visible_aliases = ["in", "i"])]
    Install(InstallArgs),
    /// Remove an installed package and its shims.
    #[command(visible_aliases = ["rm", "uninstall"])]
    Remove(PackageArg),
    /// List installed packages with their versions and targets.
    #[command(visible_alias = "ls")]
    List,
    /// Check the health of the grimoire installation.
    #[command(visible_alias = "dr")]
    Doctor,
    /// Search configured tomes for packages.
    #[command(visible_aliases = ["s", "find"])]
    Search(QueryArg),
    /// Show detailed information about a package.
    Info(PackageArg),
    /// Upgrade installed packages to the latest available version.
    #[command(visible_alias = "up")]
    Upgrade(UpgradeArgs),
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
    /// Suppress progress output on stderr. Errors and command results are still printed.
    #[arg(short, long)]
    pub quiet: bool,
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
    /// Suppress progress output on stderr. Errors and command results are still printed.
    #[arg(short, long)]
    pub quiet: bool,
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
    /// Suppress progress output on stderr. Errors and command results are still printed.
    #[arg(short, long)]
    pub quiet: bool,
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
    /// Name of the rune to build. Resolved as `runes/<name>.rn` within the tome.
    pub package: String,
    /// Tome directory containing the rune (must contain `tome.rn`). Defaults to the current
    /// directory.
    #[arg(short, long, default_value = ".")]
    pub path: PathBuf,
    /// Suppress progress output on stderr. Errors and command results are still printed.
    #[arg(short, long)]
    pub quiet: bool,
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
    /// Suppress progress output on stderr. Errors and command results are still printed.
    #[arg(short, long)]
    pub quiet: bool,
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
}
