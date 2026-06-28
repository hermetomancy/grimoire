<p align="center">
  <img src="logo.png" alt="Grimoire" width="128" height="128">
</p>

<h1 align="center">Grimoire</h1>

<p align="center">
  An imperative package manager with a fixed content-addressed store, generations, and instant switching.
</p>

---

Grimoire is a self-contained Rust package manager that embeds Nushell to run package recipes
called **runes** — a [curated subset](docs/rune-authoring.md) of the language, so recipe
behavior stays stable across Nushell releases. It installs verified prebuilt packages when they exist, builds from source
when they do not, and manages package catalogs as ordinary git repositories called **tomes**.

## Highlights

- **Imperative runes.** Package recipes are Nushell `build` functions — closer to PKGBUILDs or
ebuilds than to a functional DSL.
- **Split packages.** One parent build can produce several packages: companion runes claim
their slice of the output by glob (`clang` is carved from the `llvm` monorepo build, with
the compiler-rt runtimes inside).
- **Decisions at plan time.** Conflicts, replacements, and the transitive build-dep closure
resolve before anything is fetched or built; every mutating command takes `--dry-run`.
- **Git-native catalogs.** Tomes are git repositories you can fork, diff, pin, and update.
- **Binary-first installs.** Grimoire prefers a signed/checksum-verified prebuilt package and
falls back to source builds when needed.
- **Native machinery.** Grimoire does not shell out to `git`, `tar`, `zstd`, `curl`, or `nu` for
its own work.
- **Managed build dependencies.** Source builds install declared build dependencies and cache
them store-only for later builds.
- **Reproducible state.** The lockfile records packages, versions, archive hashes, content
addresses, install reasons, holds, and tome commits; `grm generation restore` rebuilds the recorded set
on any install root, and `--locked` operations refuse to resolve against a tome that moved
off its pinned commit.
- **Trustable binaries.** Tome indexes can be minisign-signed and TOFU-pinned, so an index
signature authenticates every archive hash it publishes.
- **Generations and switching.** Every install/remove/upgrade creates a new generation;
`grm generation switch` moves between them (back or forward) instantly without rebuilding.
- **Distro-citizen tooling.** Install-reason tracking — removing a package takes its
now-unneeded dependencies with it in the same transaction — file-ownership queries
(`grm pkg files`, `grm pkg owns`, `grm pkg provides`), preferred providers for contested commands
(`grm pkg prefer awk gawk`), post-install notes, and tome news.

## Positioning

| Axis | Grimoire | Like |
|---|---|---|
| Store / install / upgrade | immutable fixed-path content-addressed store, generations, switching | **Nix / Guix** |
| Recipe authoring | imperative Nushell `build` function | **Portage / Pacman** (PKGBUILD/ebuild) |
| Build-time customization | `build_flags` | **Portage USE flags** |
| Catalogs / overlays | tomes + addenda | **AUR / overlays** |
| Contested commands | `grm pkg prefer` | **update-alternatives / eselect** |
| Build / trust | managed clang/LLVM toolchain, signed binhost | **Pacman / Gentoo binhost** |

## Install

```sh
cargo install --git https://github.com/hermetomancy/grimoire
grm setup
grm setup --bootstrap
```

This installs the `grm` command. `grm setup` creates the fixed store (`/grm`), puts the
active profile's `bin` on your shell's PATH, and leaves package state alone. `grm setup
--bootstrap` then adds the core and world tomes and installs grimoire through itself — from then
on `grm upgrade grimoire` is self-update.

## Quick Start

```sh
# Add a package catalog if you did not run `grm setup --bootstrap`
grm tome add https://github.com/hermetomancy/tome-core --ref main

# Search, inspect, install
grm search hello
grm info hello
grm install hello --dry-run   # the full plan: steps, build deps, migrations
grm install hello
grm ls                        # the linked environment; --all adds cached build deps, --explicit only your roots

# Upgrade, hold, switch generations
grm upgrade
grm pkg hold hello
grm pkg unhold hello
grm generation switch

# Ask questions
grm pkg files hello                   # what did this package install?
grm pkg owns ~/.grimoire/profiles/current/bin/hello   # what installed this file?
grm pkg provides awk                  # who can provide this command?
grm pkg prefer awk gawk               # pick the provider when several can

# Clean up
grm remove hello                      # dependencies nothing needs anymore leave with it
grm clean                             # prune old generations, unreferenced store paths, caches
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
Publish by uploading `dist/` to a static host and pointing `tome.rn` at it. A tome may also ship
announcements as `news/*.md`; `grm tome update` shows new items once and `grm tome news` re-reads
them.

## Writing Runes

A rune exports package metadata and a `build` function written in native Nushell:

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
    # build-env pulls the managed toolchain (compiler, gmake, cmake, python, userland floor).
    build: { default: ["build-env"] }
    runtime: []
  }
  bins: { default: { widget: "bin/widget" } }
}

export def build [ctx] {
  cd ($ctx.sources.main.dir | path join "widget-1.2.3")
  ./configure --prefix=($ctx.prefix)
  make -j($ctx.nproc)
  make install DESTDIR=($ctx.package_dir)
}
```

The build stages into `ctx.package_dir`, configures against the final store prefix
(`ctx.prefix`), and runs in a sandboxed environment where only declared dependencies are
discoverable. See [docs/rune-authoring.md](docs/rune-authoring.md) for the full reference:
the `ctx` record, install conventions, platform conditionals, and post-install notes.

## Store Layout

Grimoire installs into `~/.grimoire` by default (override with `GRIMOIRE_ROOT`). The canonical
store is at `/grm/store/<hash>-<name>-<version>`. Active packages are surfaced through generation
profiles:

```
~/.grimoire/
├── store/<hash>-<name>-<version>/   # package contents
├── profiles/
│   ├── current -> gen-3             # symlink to active generation
│   └── gen-3/                       # symlinks into store
│       ├── bin/
│       └── share/man/
├── state/packages/<name>.nuon       # installed state
└── state/grimoire.lock.nuon         # lockfile
```

`grm setup` puts `~/.grimoire/profiles/current/bin` on your shell's PATH (zsh, bash, and
fish are recognised); add it manually for other shells. `grm setup --bootstrap` performs the
optional self-hosting bootstrap after that store/PATH setup is complete.

## Release Signing

Official release artifacts are signed with the project minisign key:

```
untrusted comment: minisign public key D4CCD5A2669CAC7C
RWR8rJxmotXM1NhQBsJZQfEeWtSP+3x67Nih78Tl7An5o7UQ8gWwmTt6
```

Verify a release file with `minisign -Vm <file> -P RWR8rJxmotXM1NhQBsJZQfEeWtSP+3x67Nih78Tl7An5o7UQ8gWwmTt6`.
A tome opts into signing by declaring `signers` in its `tome.rn` (see below); the keys are
pinned on first sync and every later sync must present the same set.

## Signing a Tome

Trust is **per artifact**, not per index: each published archive carries a detached
`archive.tar.zst.minisig`, and source runes are authenticated through a signed
`runes-manifest.nuon` (which pins every rune's content hash). The `index.nuon` itself is not
signed — its archive hashes are authenticated by each archive's own signature plus its checksum.

```sh
minisign -G                                    # generate a keypair
grm tome build --all --path ./mytome           # build archives into dist/
# sign each published artifact against your secret key:
minisign -S -m ./mytome/runes-manifest.nuon    # source distribution
for a in ./mytome/dist/*.tar.zst; do minisign -S -m "$a"; done   # binary distribution
```

Declare the public key at the manifest's **top level** (a `signer` nested under `packages` is
rejected as an unknown field, so a misplaced key fails loudly instead of shipping unsigned):

```nu
export const tome = {
  name: "mytome"
  signers: ["RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"]
  packages: {
    repo: "https://example.com/mytome"
    format: "http"
    index: "index.nuon"
  }
}
```

Grimoire pins the key set on first sync and refuses later syncs with a missing, invalid, or
rotated key.

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
arbitrary root execution. The binding security invariants are documented in
[AGENTS.md §10](AGENTS.md).

Report implementation bugs in verification, extraction, or privilege boundaries privately via
GitHub's vulnerability reporting tab.

## Further Reading

- [Rune authoring reference](docs/rune-authoring.md)
- [Agent guidelines](AGENTS.md)
- [Remaining work](TODO.md)

## License

[MIT](LICENSE)
