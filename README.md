<p align="center">
  <img src="logo.png" alt="Grimoire" width="128" height="128">
</p>

<h1 align="center">Grimoire</h1>

<p align="center">
  An imperative package manager with a fixed content-addressed store, generations, and rollback.
</p>

---

Grimoire is a self-contained Rust package manager that embeds Nushell to run package recipes
called **runes**. It installs verified prebuilt packages when they exist, builds from source
when they do not, and manages package catalogs as ordinary git repositories called **tomes**.

## Highlights

- **Imperative runes.** Package recipes are Nushell `build` functions — closer to PKGBUILDs or
ebuilds than to a functional DSL.
- **Git-native catalogs.** Tomes are git repositories you can fork, diff, pin, and update.
- **Binary-first installs.** Grimoire prefers a signed/checksum-verified prebuilt package and
falls back to source builds when needed.
- **Native machinery.** Grimoire does not shell out to `git`, `tar`, `zstd`, `curl`, or `nu` for
its own work.
- **Managed build dependencies.** Source builds install declared build dependencies and cache
them store-only for later builds.
- **Reproducible state.** Lockfiles record installed packages, versions, hashes, dependencies,
tome commits, and addenda.
- **Trustable binaries.** Tome indexes can be minisign-signed and TOFU-pinned, so an index
signature authenticates every archive hash it publishes.
- **Generations and rollback.** Every install/remove/upgrade creates a new generation;
`grm rollback` switches back instantly without rebuilding.

## Positioning

| Axis | Grimoire | Like |
|---|---|---|
| Store / install / upgrade | immutable fixed-path content-addressed store, generations, rollback | **Nix / Guix** |
| Recipe authoring | imperative Nushell `build` function | **Portage / Pacman** (PKGBUILD/ebuild) |
| Build-time customization | `build_flags` | **Portage USE flags** |
| Catalogs / overlays | tomes + addenda | **AUR / overlays** |
| Build / trust | host-toolchain builds, signed binhost | **Pacman / Gentoo binhost** |

## Install

```sh
cargo install grimoire
```

This installs the `grm` command.

## Quick Start

```sh
# Add a package catalog
grm tome add https://github.com/grimoire-of-glass/tome-core --ref main

# Search, inspect, install
grm search hello
grm info hello
grm install hello

# Upgrade, hold, clean up
grm upgrade
grm hold hello
grm unhold hello
grm remove hello
grm clean
```

Run `grm --help` for the full command tree.

## Authoring Tomes

```sh
grm tome init mytome --path ./mytome
grm tome rune widget --path ./mytome
grm tome add ./mytome --ref main
grm install widget --from-source

# Build prebuilt archives
grm tome build widget --path ./mytome
grm tome build --all --path ./mytome
```

`grm tome build` writes `.tar.zst` archives into `dist/` and records them in `dist/index.nuon`.
Publish by uploading `dist/` to a static host and pointing `tome.rn` at it.

## Rune Build Context

A rune exports package metadata and a `build` function:

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
    make install DESTDIR='($ctx.package_dir)'
  "
}
```

Build context fields:

- `ctx.sources.<name>.path` / `.dir`: verified source artifact and extraction directory
- `ctx.package_dir`: staging root, used as `DESTDIR`
- `ctx.prefix` / `ctx.store_path`: final store path (`/grm/store/<hash>-<name>-<version>`)
- `ctx.env.PATH`: build dependency `bin/` directories plus the POSIX ambient PATH and host compiler
boundary
- `ctx.target`: current target triple
- `ctx.build_flags`: key/value build options after addenda are applied

## Store Layout

Grimoire installs into `~/.grimoire` by default (override with `GRIMOIRE_ROOT`). The canonical
store is at `/grm/store/<hash>-<name>-<version>`. Active packages are surfaced through generation
profiles:

```
~/.grimoire/
├── store/<hash>-<name>-<version>/   # package contents
├── profiles/
│   ├── current -> gen-3             # symlink to active generation
│   └── gen-3/                       # hard links into store
│       ├── bin/
│       └── share/man/
├── state/packages/<name>.nuon       # installed state
└── grimoire.lock.nuon               # lockfile
```

Put `~/.grimoire/profiles/current/bin` on your PATH.

## Signing a Tome

```sh
minisign -G
grm tome build --all --path ./mytome
minisign -S -m ./mytome/dist/index.nuon
```

Declare the signer in `tome.rn`:

```nu
packages: {
  repo: "https://example.com/mytome"
  format: "http"
  index: "index.nuon"
  signer: "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"
}
```

Grimoire pins the key on first sync and refuses later syncs with a missing, invalid, or rotated
key.

## Addenda

Addenda are data-only NUON overlays that patch package metadata (sources, deps, build flags)
without running hooks:

```sh
grm addendum add <git-url-or-local-path>
grm addendum list
grm addendum remove <name>
```

## Security

Grimoire's design eliminates many traditional package-manager risks by construction:
everything is user-local, checksum-verified, optionally index-signed, and installed without
arbitrary root execution. Remaining risks are documented in the design notes in
`docs/design/` and the agent guidelines in `AGENTS.md`.

Report implementation bugs in verification, extraction, or privilege boundaries privately via
GitHub's vulnerability reporting tab.

## Further Reading

- [Remaining work](TODO.md)
- [Agent guidelines](AGENTS.md)

## License

[MIT](LICENSE)
