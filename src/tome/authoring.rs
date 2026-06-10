//! Scaffolding for tome authors: `tome init`, `tome rune`, and the templates they write.

use anyhow::{Result, bail};
use std::fs;

use crate::{
    cli::{TomeInitArgs, TomeRuneArgs},
    model::{validate_package_name, validate_package_version, validate_tome_name},
    util::progress::report,
};

/// Scaffolds a new tome: a self-naming `tome.rn` manifest, empty `runes/` and `sources/`
/// directories, a git-untracked `dist/` publish directory, and a `.gitignore` that keeps `dist/`
/// out of git. The git repository holds only runes and `tome.rn`; `grm tome build` writes built
/// archives and `index.nuon` into `dist/`, which the author uploads to the host in `packages.repo`.
pub fn init(args: TomeInitArgs) -> Result<()> {
    validate_tome_name(&args.name)?;

    let root = &args.path;
    let manifest_path = root.join("tome.rn");
    if manifest_path.exists() {
        bail!("{} already contains a tome.rn", root.display());
    }

    fs::create_dir_all(root.join("runes"))?;
    fs::create_dir_all(root.join("sources"))?;
    fs::create_dir_all(root.join("dist"))?;

    let description = args
        .description
        .unwrap_or_else(|| format!("{} tome", args.name));
    fs::write(
        &manifest_path,
        tome_manifest_template(&args.name, &description),
    )?;

    let gitignore_path = root.join(".gitignore");
    if !gitignore_path.exists() {
        fs::write(&gitignore_path, "/dist/\n")?;
    }

    report(&format!("created tome {} in {}", args.name, root.display()));
    report(&format!(
        "next: add a package with `grm tome rune <name> --path {}`",
        root.display()
    ));
    Ok(())
}

/// Scaffolds a starter rune (`runes/<name>.rn`) in an existing tome. The template is a valid,
/// buildable package definition with placeholders for the author to fill in.
pub fn rune(args: TomeRuneArgs) -> Result<()> {
    validate_package_name(&args.name)?;
    validate_package_version(&args.version)?;

    let root = &args.path;
    if !root.join("tome.rn").exists() {
        bail!("{} is not a tome (missing tome.rn)", root.display());
    }

    let runes_dir = root.join("runes");
    fs::create_dir_all(&runes_dir)?;
    let rune_path = runes_dir.join(format!("{}.rn", args.name));
    if rune_path.exists() {
        bail!("rune already exists: {}", rune_path.display());
    }

    fs::write(&rune_path, rune_template(&args.name, &args.version))?;
    report(&format!(
        "created rune {} in {}",
        args.name,
        rune_path.display()
    ));
    Ok(())
}

pub(crate) fn tome_manifest_template(name: &str, description: &str) -> String {
    const TEMPLATE: &str = r#"export const tome = {
  name: "{NAME}"
  description: "{DESCRIPTION}"

  # `grm tome build` writes archives and index.nuon into the git-untracked dist/ directory.
  # Upload dist/ to a webserver and point `repo` at the base URL that serves it. For local
  # testing `repo` may instead be an absolute path to the dist/ directory.
  packages: {
    repo: "https://example.com/{NAME}"
    format: "http"
    index: "index.nuon"
  }
}
"#;
    TEMPLATE
        .replace("{NAME}", name)
        .replace("{DESCRIPTION}", &escape_nu_string(description))
}

pub(crate) fn rune_template(name: &str, version: &str) -> String {
    const TEMPLATE: &str = r##"export const package = {
  name: "{NAME}"
  version: "{VERSION}"
  summary: "TODO: one-line summary of {NAME}"
  # Declare sources here; each is fetched and checksum-verified before `build` runs.
  # sources: {
  #   main: {
  #     url: "https://example.com/{NAME}-{VERSION}.tar.gz"
  #     sha256: "sha256:..."
  #   }
  # }
  sources: {}

  deps: {
    build: {}
    runtime: []
  }
}

export def build [ctx] {
  # Assemble the package under `$ctx.package_dir`. Verified sources are available at
  # `$ctx.sources.<name>.path`. Replace this stub with the real build steps.
  let bin_dir = ($ctx.package_dir | path join "bin")
  mkdir $bin_dir
  "#!/usr/bin/env sh\nprintf '{NAME} is not implemented yet\n'" | save ($bin_dir | path join "{NAME}")
}
"##;
    TEMPLATE
        .replace("{NAME}", name)
        .replace("{VERSION}", version)
}

/// Escapes a value for embedding inside a double-quoted Nushell string in a generated `.rn`.
pub(crate) fn escape_nu_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
