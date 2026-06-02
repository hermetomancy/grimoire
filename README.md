<p align="center">
  <img src="logo.svg" alt="Grimoire" width="128" height="128">
</p>

<h1 align="center">Grimoire</h1>

<p align="center">
  A fast, git-native, cross-platform package manager with reproducible installs.
</p>

---

Grimoire installs and manages software on Linux, macOS, and Windows from a single
self-contained binary. It installs prebuilt packages instantly when they exist and
builds from source when they don't — without asking you to install a compiler,
a shell, or any other tooling first.

Packages live in ordinary git repositories, so the catalog you install from can be
forked, pinned, reviewed, and rolled back like any other code.

## Highlights

- **One binary, every desktop.** Runs natively on Linux, macOS, and Windows. No WSL,
  no external tools to install before you can install software.
- **Prebuilt or from source.** Grimoire grabs a verified prebuilt package when one
  matches your system, and falls back to a source build only when it has to.
- **No admin rights needed.** Everything installs into a user-local directory, and
  executables are exposed through shims you can drop on your `PATH`.
- **Reproducible by default.** A lockfile records exactly what was installed — versions,
  checksums, sources, and dependencies — so an install can be reproduced or audited.
- **Safe by construction.** Every download is checksum-verified before it is trusted,
  and archives are validated before extraction.
- **Git-native catalogs.** Package sets are git repositories you can fork, pin to a
  commit, diff, and roll back.
- **Data-only overlays.** Addenda patch package metadata without executing hooks, so local
  source/checksum/dependency overrides stay reviewable.

## Install

```sh
cargo install grimoire
```

This installs the `grm` command.

## Quick start

```sh
# Add a catalog of packages (a "tome")
grm tome add https://github.com/example/core-runes.git --ref main

# Find and inspect packages
grm search hello
grm info hello

# Install, upgrade, and remove
grm install hello
grm upgrade hello
grm remove hello

# Build from source explicitly
grm install hello --from-source

# Reproduce exactly what the lockfile records
grm install hello --locked

# See what's installed and check your setup
grm list
grm doctor
```

Most commands have short aliases, so the above can be terser — `grm in hello`,
`grm rm hello`, `grm up`, `grm s hello`, `grm ls`. Run `grm --help` to see them all.

Progress messages go to stderr; results go to stdout. Add `--quiet` (`-q`) to silence
progress while keeping the result.

## Authoring a tome

Create your own catalog and start packaging software:

```sh
grm tome init mytome --path ./mytome   # scaffold a tome
grm tome rune widget --path ./mytome   # add a package definition (rune)
grm tome add ./mytome                  # register it locally to test
grm install widget --from-source
grm tome build widget --path ./mytome  # build a prebuilt archive into dist/
grm tome build --all --path ./mytome   # build every rune in the tome
```

`grm tome init` writes a `tome.rn` manifest alongside `runes/`, `sources/`, a git-ignored
`dist/`, and a `.gitignore`. `grm tome rune` drops a templated rune you fill in with the
package's sources, dependencies, and build steps.

`grm tome build` compiles a rune into a verified archive and records it in `dist/index.nuon`.
The `dist/` directory is the publishing unit: it holds the index plus every prebuilt archive
and is kept out of git. To publish, upload the whole `dist/` directory to a static webserver
and point the manifest's `packages.repo` at that base URL (`format: "http"`); installers then
fetch the index and checksum-verified archives over HTTP. For local testing, set `repo` to a
filesystem path with `format: "local"`. Either way, the git repository carries only your runes
and `tome.rn` — never the built binaries.

### Source build context

During a source build, Grimoire fetches every declared source, verifies its `sha256`, prepares a
temporary build workspace, installs build dependencies, then calls the rune's `build` function with
a `ctx` record. The build function should assemble the package under `ctx.package_dir`; Grimoire
packs that directory into the final `.tar.zst`.

The build context fields are:

- `ctx.sources.<name>.path`: the verified source artifact, keyed by the name used in
  `package.sources`. This is the cached file Grimoire checked against the declared checksum.
- `ctx.sources.<name>.dir`: for `.tar.zst` and `.tzst` sources, the directory where Grimoire
  natively extracted the archive after validating every member path and rejecting links. For other
  source types this is `null`.
- `ctx.env.PATH`: the build PATH visible to Nushell and external commands run by the package's
  build script. Grimoire prepends each installed build dependency's `bin/` directory, then appends
  the host PATH.
- `ctx.package_dir`: the staging root that becomes the installed package. Put files here exactly as
  they should appear after install, for example `bin/tool`, `lib/...`, or `share/...`.
- `ctx.work_dir`: a scratch directory for the build. It contains Grimoire-prepared sources and may
  be used for temporary build output.
- `ctx.prefix`: the install prefix to pass to conventional build systems. It currently points at
  the same path as `ctx.package_dir`, so configure-style builds can use `./configure
  --prefix=$ctx.prefix` followed by `make install PREFIX=$ctx.package_dir`.
- `ctx.build_flags`: inert string key/value data from the rune's `build_flags` metadata after
  addenda have been applied. Runes may read it to choose configure options or feature flags.

For a typical archived source package, a rune build might look like:

```nu
export def build [ctx] {
  let src = ($ctx.sources.main.dir | path join "widget-1.2.3")
  cd $src
  ./configure $"--prefix=($ctx.prefix)"
  make
  make install $"PREFIX=($ctx.package_dir)"
}
```

## Concepts

- **Runes** are package definitions. Each rune declares a package — its version, sources,
  dependencies, and the executables it provides — and, when needed, how to build it from
  source. Dependencies can pin a version requirement (`{ name: "lib", version: ">=1.2" }`),
  and Grimoire resolves a set of versions that satisfies every constraint in the graph.
- **Tomes** are catalogs of runes: ordinary git repositories you add, update, and pin.
- **Packages** install as self-contained archives with embedded metadata and checksums.
  Source builds and prebuilt downloads both produce the same kind of archive, so installs
  behave identically either way.
- **Addenda** are customization layers that patch a tome's package metadata without forking it,
  in the spirit of overlays. They are inert NUON data: no hooks or scripts run from an addendum.

## Addenda

An addendum repository has an `addendum.nuon` file at its root:

```nuon
{
  name: mypatches
  patches: [
    {
      tome: example
      package: hello
      version: "0.2.0"
      summary: "hello with a local source override"
      sources: {
        main: {
          url: "hello-0.2.0.tar.zst"
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

`tome` is optional; when omitted, the patch applies to any rune with the matching package name.
Scalar fields (`version`, `summary`, `target`) replace the rune's metadata, `sources`, `bins`, and
`build_flags` merge by key, and `deps` replaces the rune's dependency policy. Addenda are managed with
`grm addendum add <git-url-or-local-path>`, `grm addendum list`, and `grm addendum remove <name>`.

## How it compares

Grimoire borrows the fast archive-first install model of system package managers, the
source-definition and layering ideas of functional package managers, and the git-backed
catalog model of per-user installers — while staying OS-independent and conventional.

| Tool | Main focus | Catalog model | Grimoire difference |
| --- | --- | --- | --- |
| **Pacman** | Fast binary installs | Distribution repositories | Same archive-first speed, but OS-independent definitions and source builds as a first-class fallback. |
| **Scoop** | Lightweight Windows app installs | Git buckets of manifests | Generalizes buckets into cross-platform tomes with prebuilt-package repos and layering. |
| **Chocolatey** | Windows software + automation | Central feed (+ private feeds) | No central feed assumed; git repositories are the native distribution unit. |
| **Homebrew** | Developer packages on macOS/Linux | Git taps of formulae | Keeps git-backed catalogs but runs natively on Windows too, with first-class customization layers. |
| **Nix** | Reproducible builds & environments | Declarative repos / channels | Source definitions, pins, and overlays without requiring a full functional store model. |

## Status

Grimoire is in early development. Installing (prebuilt and from source), building,
version-aware dependency resolution, publishing prebuilt archives over HTTP, removal,
upgrades, lockfiles with reproducible `--locked` installs, addenda, and health checks are working
today.

## License

[MIT](LICENSE)
