use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "grimoire")]
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
    Build(BuildArgs),
    /// Install a package by name, from a local archive, or from a rune.
    Install(InstallArgs),
    /// Remove an installed package and its shims.
    Remove(PackageArg),
    /// List installed packages with their versions and targets.
    List,
    /// Check the health of the grimoire installation.
    Doctor,
    /// Search configured tomes for packages.
    Search(QueryArg),
    /// Show detailed information about a package.
    Info(PackageArg),
    /// Upgrade installed packages to the latest available version.
    Upgrade(UpgradeArgs),
    /// Manage tomes: the git repositories packages are resolved from.
    Tome {
        #[command(subcommand)]
        command: TomeCommand,
    },
    /// Manage addenda: data-only overlays that patch tome rune definitions.
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
    /// Add a tome by cloning a git repository containing a `tome.rn` manifest at its root.
    /// The tome is registered under the name declared in that manifest.
    Add(TomeAddArgs),
    /// Sync configured tomes, fetching the latest commit for their tracked ref.
    Update(TomeUpdateArgs),
    /// Remove a configured tome and its cached repository.
    Remove(TomeRemoveArgs),
    /// List configured tomes with their URLs and tracked refs.
    List,
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
    Remove(TomeRemoveArgs),
    /// List configured addenda.
    List,
}
