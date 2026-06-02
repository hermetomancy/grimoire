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

## Install

```sh
cargo install grimoire
```

## Quick start

```sh
# Add a catalog of packages (a "tome")
grimoire tome add https://github.com/example/core-runes.git --ref main

# Find and inspect packages
grimoire search hello
grimoire info hello

# Install, upgrade, and remove
grimoire install hello
grimoire upgrade hello
grimoire remove hello

# Build from source explicitly
grimoire install hello --from-source

# See what's installed and check your setup
grimoire list
grimoire doctor
```

Progress messages go to stderr; results go to stdout. Add `--quiet` (`-q`) to silence
progress while keeping the result.

## Concepts

- **Runes** are package definitions. Each rune declares a package — its version, sources,
  dependencies, and the executables it provides — and, when needed, how to build it from
  source.
- **Tomes** are catalogs of runes: ordinary git repositories you add, update, and pin.
- **Packages** install as self-contained archives with embedded metadata and checksums.
  Source builds and prebuilt downloads both produce the same kind of archive, so installs
  behave identically either way.
- **Addendums** *(planned)* are customization layers that patch a tome's packages —
  sources, mirrors, build flags, metadata — without forking it, in the spirit of overlays.

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
dependency resolution, removal, upgrades, lockfiles, and health checks are working today.
Addendums are designed but not yet implemented.

## License

[MIT](LICENSE)
