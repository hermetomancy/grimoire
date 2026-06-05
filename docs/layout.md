# Operating layout

Everything Grimoire writes lives under a single user-local install root. This document
describes what's in it, what's safe to delete, and how to move it.

## Install root

By default the install root is `~/.grimoire` on every platform (like `~/.cargo` or `~/.rustup`).

Override it by setting `GRIMOIRE_ROOT` to an absolute path. The override is honored by every
command and is the recommended way to run multiple isolated installs (for example, a test
profile alongside your daily one). Grimoire never writes outside this root and never asks
for elevation.

> **The install root must not contain spaces.** Source builds break otherwise: autotools records
> the absolute paths of build tools (`MKDIR_P`, `INSTALL`, …) and Makefiles use them unquoted, so a
> path like `~/Library/Application Support/…` splits at the space. This is the main reason the root
> is a plain `~/.grimoire` rather than the platform data directory, and why a source build fails
> early if `GRIMOIRE_ROOT` points somewhere with a space.

## Directory tree

```
<install root>/
├── store/<hash>-<name>-<version>/  # installed package contents (bin/, lib/, share/, …)
├── profiles/
│   ├── current -> gen-3           # symlink to the active generation
│   └── gen-3/                     # generation: real directory tree of hard links
│       ├── bin/
│       └── share/man/
├── state/
│   ├── packages/<name>.nuon       # installed-state records (one per package)
│   └── generations.nuon           # generation registry (index + history)
├── tomes/                         # cached tome git repositories
├── addendums/                     # cached addendum repositories
├── cache/
│   ├── sources/                   # verified source artifacts, keyed by sha256
│   ├── archives/                  # verified binary package archives
│   └── builds/                    # source-build output, before promotion
├── transactions/                  # staging directories for in-flight installs
└── grimoire.lock.nuon             # reproducible-install lockfile
```

## What each path is for

- **`store/<hash>-<name>-<version>/`** — the installed package itself, in the content-addressed
  store. `<hash>` is derived from the build's inputs (sources, rune, target, build flags, and
  resolved dependency closure), so identical inputs resolve to the same directory. The basename is
  recorded in the archive's metadata and is the install's identity; `state/packages/<name>.nuon`
  records its absolute path. Promoted into place by an atomic rename from `transactions/`. Removing
  a store directory by hand will leave the state file and profile references dangling — use
  `grm remove` instead.
  (The canonical store is the fixed path `/grm/store`; under `GRIMOIRE_ROOT` it lives at
  `<root>/store` for tests and isolated installs, at the cost of cross-machine binary-cache
  portability.)
- **`profiles/current -> gen-N/`** — the active generation: a real directory tree of hard links
  (or reflinks on CoW filesystems) into `store/`. Put `profiles/current/bin` on your `PATH`.
  Every install, remove, or upgrade creates a new generation and atomically repoints `current`.
  `grm rollback` switches back to the previous generation instantly.
- **`profiles/gen-N/`** — an immutable generation directory. It contains only executables and
  human-facing artifacts (`bin/`, `share/man/`, completions, desktop files) because Grimoire
  binaries bake absolute store paths for libraries and headers. Old generations are retained as
  rollback targets until reclaimed by `grm gc`.
- **`state/packages/<name>.nuon`** — the recorded state for an installed package: version,
  target, source/archive hashes, runtime deps, store path, and the tome it came from. `grm doctor`
  and the lockfile are derived from these files.
- **`state/generations.nuon`** — the generation registry: a list of all retained generations with
  their package sets and creation timestamps. Used by `grm rollback`, `grm gc`, and boot
  integration.
- **`tomes/`** — clones of the git repositories you added with `grm tome add`. Re-cloned on
  demand; safe to delete, at the cost of a re-sync on the next install.
- **`addendums/`** — clones (or local-path copies) of repositories you added with
  `grm addendum add`. Same recovery story as `tomes/`.
- **`cache/sources/`** and **`cache/archives/`** — verified inputs keyed by sha256. Safe to
  delete; they will be re-fetched and re-verified on the next install. Reclaims the most
  space.
- **`cache/builds/`** — archives produced by source builds before they were installed.
  Useful when debugging a build; otherwise safe to delete.
- **`transactions/`** — staging dirs for installs in progress. If an install crashes
  mid-flight you may find stale entries here; they are safe to delete when no `grm` process
  is running. (Until a concurrency lock lands, do not run multiple mutating commands
  against the same root.)
- **`grimoire.lock.nuon`** — the reproducible-install lockfile, regenerated after every
  install or remove. `grm install --locked` reads it back and refuses anything not recorded.

## Common operations

- **Reclaim disk space without losing installs:** run `grm clean`. It empties `cache/sources/`,
  `cache/archives/`, `cache/builds/`, and any leftover `transactions/` staging directories,
  reports bytes freed, and leaves installed packages, shims, state, tomes, addenda, and the
  lockfile untouched. Everything cleaned is reproducible from the original sources on the
  next install.
- **Move an install to another machine or directory:** copy the entire install root and set
  `GRIMOIRE_ROOT` to the new path. Shims contain absolute paths, so they will need to be
  regenerated (`grm install --locked` against the copied `grimoire.lock.nuon` is the
  supported path).
- **Start over:** delete the install root. There is no global state outside it.
- **Check that everything is consistent:** `grm doctor` validates configured tome caches,
  installed-state integrity (package dirs + shims), and lockfile presence.
