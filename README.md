<p align="center">
  <img src="logo.svg" alt="Grimoire" width="128" height="128">
</p>

<h1 align="center">Grimoire</h1>

<p align="center">
  An imperative package manager growing toward a fixed-store, rollback-able distro model.
</p>

---

Grimoire is a self-contained Rust package manager for Linux, macOS, and Windows that embeds Nushell
to run package recipes called **runes**. It installs verified prebuilt packages when they exist,
builds from source when they do not, and manages package catalogs as ordinary git repositories
called **tomes**.

The long-term target is sharper than a user-local package manager: Grimoire is designed to become
both a secondary package manager on existing systems and the primary package manager of its own
Linux distribution. The destination is a fixed content-addressed store at `/grm/store`, profiles,
generations, rollback, garbage collection, imperative recipes, and a signed binary cache.

Today, Grimoire still uses a user-local install root and profile-like shims. That current layout is
documented in [docs/layout.md](docs/layout.md). The future store model is documented in
[docs/design/store-and-distro-model.md](docs/design/store-and-distro-model.md).

## Highlights

- **Imperative runes.** Package recipes are Nushell `build` functions, closer to PKGBUILDs or
  ebuilds than to a functional package DSL.
- **Git-native catalogs.** Tomes are git repositories you can fork, diff, pin, and update.
- **Binary-first installs.** Grimoire prefers a signed/checksum-verified prebuilt package for your
  target and falls back to source builds when needed.
- **Native machinery.** Grimoire does not shell out to `git`, `tar`, `zstd`, `curl`, or `nu` for
  its own work; those capabilities are linked Rust crates or the embedded Nushell engine.
- **Managed build dependencies.** Source builds install declared build dependencies, put their
  `bin/` directories on `PATH`, and clean up build-only dependencies afterwards.
- **Reproducible state.** Lockfiles record installed packages, versions, hashes, dependencies, tome
  commits, and addenda.
- **Trustable binaries.** Tome indexes can be minisign-signed and TOFU-pinned, so an index
  signature authenticates every archive hash it publishes.
- **Future fixed store.** The roadmap moves from today's user-local layout to `/grm/store`,
  profiles, generations, rollback, and GC roots.

## Install

During early development, install from source:

```sh
cargo install grimoire
```

This installs the `grm` command. Release archives and `grm self-update` are planned once signed
multi-platform releases exist.

## Quick Start

```sh
# Add a package catalog
grm tome add https://github.com/grimoire-of-glass/tome-core --ref main

# Keep catalogs fresh
grm tome update

# Find and inspect packages
grm search hello
grm info hello

# Install, upgrade, and remove
grm install hello
grm upgrade          # also updates configured tomes first
grm remove hello     # also removes runtime deps nothing else still needs

# Preview without touching state
grm install hello --dry-run
grm upgrade --dry-run

# Hold a package back from upgrade
grm hold hello
grm unhold hello

# Check and clean
grm list
grm doctor
grm clean
```

Common aliases exist: `grm in hello`, `grm rm hello`, `grm up`, `grm s hello`, `grm ls`. Run
`grm --help` for the full command tree.

Progress and diagnostics go to stderr; command results go to stdout. Use `--quiet` / `-q` to
suppress progress, or `--verbose` for persistent progress lines and full build output.

## Authoring Tomes

Create a catalog and package a program:

```sh
grm tome init mytome --path ./mytome
grm tome rune widget --path ./mytome
grm tome add ./mytome --ref main
grm install widget --from-source

# Publish prebuilt packages into the tome's dist/ directory
grm tome build widget --path ./mytome
grm tome build --all --path ./mytome
```

`grm tome init` creates:

- `tome.rn`: the tome manifest
- `runes/`: package recipes
- `sources/`: optional local source payloads
- `dist/`: git-ignored package index + prebuilt archives
- `.gitignore`

`grm tome build` writes a `.tar.zst` archive and upserts it into `dist/index.nuon`. To publish,
upload `dist/` to a static host and point `tome.rn` at it:

```nu
packages: {
  repo: "https://example.com/mytome"
  format: "http"
  index: "index.nuon"
}
```

For local testing, use `format: "local"` and a filesystem path.

## Signing a Tome

The package index records each archive's checksum, so signing the index authenticates every
prebuilt package it names. Grimoire verifies minisign signatures; authors sign with `minisign`.

```sh
minisign -G
grm tome build --all --path ./mytome
minisign -S -m ./mytome/dist/index.nuon
```

Declare the public key in `tome.rn`:

```nu
packages: {
  repo: "https://example.com/mytome"
  format: "http"
  index: "index.nuon"
  signer: "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"
}
```

On first sync, Grimoire pins the key. Later syncs must verify with the same key; a missing
signature, invalid signature, or changed key is refused. Unsigned tomes still work, but official
and security-sensitive tomes should be signed.

## Rune Build Context

A rune exports package metadata and, when source builds are supported, a `build` function:

```nu
export const package = {
  name: "widget"
  version: "1.2.3"
  sources: {
    main: {
      url: "https://example.com/widget-1.2.3.tar.gz"
      sha256: "sha256:..."
    }
  }
  deps: {
    build: { default: ["make"] }
    runtime: []
  }
  bins: { widget: "bin/widget" }
}

export def build [ctx] {
  let src = ($ctx.sources.main.dir | path join "widget-1.2.3")
  sh -c $"
    set -eu
    cd '($src)'
    ./configure --prefix='($ctx.prefix)'
    make
    make install PREFIX='($ctx.package_dir)'
  "
}
```

Current context fields:

- `ctx.sources.<name>.path`: verified source artifact.
- `ctx.sources.<name>.dir`: native extraction directory for `.tar.gz`, `.tar.xz`, and `.tar.zst`
  sources after path/link validation.
- `ctx.env.PATH`: build dependency `bin/` directories plus the allowed host boundary.
- `ctx.package_dir`: current staging/package root.
- `ctx.work_dir`: scratch build directory.
- `ctx.prefix`: currently the package staging prefix. This is scheduled to change to the final
  intended prefix as part of the store migration.
- `ctx.target`: current target triple, for target-specific build choices.
- `ctx.build_flags`: inert string key/value build options after addenda are applied.

### Upcoming Build Contract Change

The fixed-store model requires a stricter contract:

```text
ctx.prefix      = final store path known before build
ctx.package_dir = staging root
```

Configure-style builds should then use:

```sh
./configure --prefix="$ctx.prefix"
make
make install DESTDIR="$ctx.package_dir"
```

That is the next major compatibility step because binaries must bake their final store path, not a
temporary staging directory.

## Concepts

- **Runes** are package definitions: metadata plus optional imperative source-build logic.
- **Tomes** are git-backed catalogs of runes and package indexes.
- **Addenda** are data-only overlays that patch package metadata without executing hooks.
- **Binary indexes** list prebuilt archives, hashes, targets, and dependencies.
- **The future store** is `/grm/store/<hash>-<name>-<version>`, with activation through profiles
  and generations rather than direct mutation of installed package directories.

## Future Store Model

The target architecture borrows the store/generation ideas from Nix and Guix, while keeping rune
authoring closer to Arch/Gentoo:

- Packages live at fixed content-addressed paths under `/grm/store`.
- Profiles are symlink trees that expose a selected package set.
- Every install, remove, or upgrade creates a new generation.
- Rollback repoints to an older generation; it does not rebuild.
- GC roots keep retained generations alive.
- Binaries are trusted through signed binhosts.
- On the Grimoire distro target, executable FHS paths like `/bin` and `/usr/bin` are generated as
  symlink views, while libraries stay isolated in the store.

Read the full design in [docs/design/store-and-distro-model.md](docs/design/store-and-distro-model.md).

## Addenda

An addendum repository has an `addendum.nuon` at its root:

```nuon
{
  name: mypatches
  patches: [
    {
      tome: core
      package: hello
      version: "2.12.1"
      sources: {
        main: {
          url: "hello-2.12.1.tar.gz"
          sha256: "sha256:..."
        }
      }
      deps: {
        build: { default: ["make"] }
        runtime: []
      }
      build_flags: {
        configure: "--disable-nls"
      }
    }
  ]
}
```

Manage addenda with:

```sh
grm addendum add <git-url-or-local-path>
grm addendum list
grm addendum remove <name>
```

Addenda are inert NUON data. They can patch package metadata, but they cannot run hooks.

## Comparison

| Tool | What Grimoire Borrows | What Grimoire Changes |
| --- | --- | --- |
| Nix / Guix | Fixed store, generations, rollback, binary cache direction | Imperative runes instead of a functional package language |
| Gentoo / Portage | Source builds, feature flags, overlays, binhost trust model | Store/generation rollback as a first-class goal |
| Arch / Pacman | Fast binary packages and simple user experience | Git-native tomes and source fallback |
| Homebrew | Git-backed package catalogs and developer ergonomics | Fixed-store trajectory and Linux distro ambitions |
| GoboLinux | Per-package-version store spirit and FHS symlink friendliness | Content addressing, generations, signed binhost |

## Further Reading

- [Target store + distro design](docs/design/store-and-distro-model.md)
- [Current operating layout](docs/layout.md)
- [Threat model](docs/threat-model.md)
- [TODO roadmap](TODO.md)

## Status

Grimoire is early but functional. Current support includes git-native tomes, source builds,
verified downloads, binary indexes, signed indexes with TOFU pinning, version-aware dependency
resolution, build dependencies, addenda, lockfiles, locked installs, holds, dry-run plans,
autoremove, health checks, shell completions, man pages, live build output, and cache cleanup.

The next architectural step is the store-prep build contract: final prefix separate from staging,
so the eventual `/grm/store` migration does not require rewriting every rune twice.

## License

[MIT](LICENSE)
