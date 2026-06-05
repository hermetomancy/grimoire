# Design goal: content-addressed store + dual-role distro

This is a **target architecture**, not the current implementation. It records the model Grimoire
is aiming at so the work between here and there is pointed in one direction. Current behavior lives
in [`../layout.md`](../layout.md) and [`../threat-model.md`](../threat-model.md); this document is
the destination.

## The goal in one sentence

Grimoire should be a capable **secondary** package manager (user-local, alongside the native PM on
macOS or another Linux) **and** the **primary** package manager of its own Linux distribution — an
*approachable* take on a NixOS-style rollback-able, "immutable" system, without the functional
language. Here "immutable" refers to the **store** and to **package generations/rollback**; system
configuration (`/etc`) is handled conventionally, as on a traditional distro.

## The central decision: a single fixed, content-addressed store

The whole architecture follows from one commitment, taken from Nix/Guix:

> Packages live in an immutable, content-addressed store at a **single fixed, username-free
> absolute path** — `/grm/store/<hash>-<name>-<version>` — that is identical on every machine and
> every OS where Grimoire runs.

`<hash>` is derived from the build's inputs (source checksums, the rune bytes, the resolved
dependency closure, target triple, and the build-flag set), so the same inputs resolve to the same
store path everywhere. That single constant path is what makes everything else work:

- **Cross-user / cross-machine portability.** Because the path never contains a username and is the
  same on every host, a prebuilt binary's baked absolute paths (RPATH, macOS `install_name`,
  pkg-config `prefix`) are valid on every machine. This is the property a per-user `~/...` root can
  never have, and it is *the* enabler for a shared binary cache.
- **No relocation, ever.** With a fixed store, binaries bake absolute store paths and need neither
  runtime relocation (`$ORIGIN`/`@rpath`) nor install-time path rewriting. Runtime relocatability
  is explicitly a **non-goal** (see below).
- **Side-by-side versions.** Two packages needing different versions of a library each point at
  their own store path — the structural cure for dependency hell.

**Accepted cost:** a fixed store path requires a **one-time privileged creation of `/grm`** on
foreign hosts (exactly why `/nix` exists). Nix deliberately rejected relocatable per-user stores
because they break the binary cache; Grimoire takes the same position. Day-to-day use after that is
unprivileged, via profiles. The store path may be overridable for special cases, but overriding it
forfeits the shared binary cache, so in practice `/grm` is canonical and treated as sacred.

## Profiles, generations, and rollback

The store is immutable; the *system* is assembled from it by **profiles** — real directory trees
that a user's (or the system's) `PATH` points at. Every change produces a new **generation**:

- **Thin profiles.** Because Grimoire binaries bake absolute store paths (RPATH, `install_name`,
  pkg-config `prefix`), the profile does not need to globally expose libraries, headers, or
  pkg-config files. A generation only surfaces executables and human-facing artifacts: `bin/`,
  `share/man/`, shell completions, and desktop files. Everything else stays in the store and is
  found via the baked absolute paths.

- **Real files, not symlinks.** Each generation is a real directory tree whose files are **hard
  links** (or APFS `clonefile` / Linux reflink on CoW filesystems) into the store. Software sees
  normal paths, `argv[0]` and `/proc/self/exe` resolve to the profile path, and there is no
  symlink-traversal overhead or `realpath` confusion. Because store paths are immutable, hard links
  are completely safe.

- **Atomic switch.** The active generation is selected by a single symlink:
  `profiles/current -> gen-N`. Activating an install/upgrade/remove repoints that symlink; the
  running system is never mutated in place. It either fully switches or doesn't.

- **Rollback is byte-exact and reliable.** `grm rollback` repoints `current` to the previous
  generation, whose files are the literal bytes that ran before. Rollback depends only on
  **retaining the old generation directories**, never on rebuilding anything.

- **Boot integration.** On a Grimoire distro the bootloader lists generations, so a broken kernel
  or init still boots the previous generation.

- **GC roots.** Generations themselves are the GC roots: any store path referenced by any retained
  generation is protected. `grm gc` walks the generation trees, collects referenced store basenames,
  and deletes unreferenced store paths.

Generations + profiles + GC deliver ~90% of the felt NixOS benefit and are the highest priority of
this model. They require no functional language and no change to the build model.

## Build & trust model: imperative recipes, signed binhost

Where the store mechanics are Nix-like, the build and trust model are deliberately Arch/Gentoo-like
— this is the "approachable" half of the bet:

- **Imperative recipes.** A rune is an imperative Nushell `build` function (`./configure && make`),
  like a PKGBUILD/ebuild — not a purely functional expression language. `build_flags` play the role
  of Portage USE flags; tomes + addenda play the role of overlays / the AUR.
- **Host-toolchain builds.** Builds run against a host compiler boundary. The store path is derived
  from the build's inputs (source checksums, rune bytes, dependency closure, target, build flags).
- **Signed binhost.** Binaries are trusted via signatures from the builder/distro, backed by the
  existing minisign tome/index signing — the Arch/Gentoo signed-binhost trust model.

### Build contract change this implies

`ctx.prefix` must become the package's **final store path** (known before the build), and builds use
`./configure --prefix=<store path>` + `make install DESTDIR=<staging>`. The current
`ctx.prefix == staging tempdir` is already latently wrong — binaries bake a temp path that no longer
exists after the package is promoted.

## Positioning

Grimoire sits in a genuinely unoccupied cell, recombining proven ideas:

| Axis | Grimoire | Like |
|---|---|---|
| Store / install / upgrade | immutable fixed-path content-addressed store, generations, rollback, dual-role | **Nix / Guix** |
| Recipe authoring | imperative Nushell `build` fn | **Portage / Pacman** (PKGBUILD/ebuild) |
| Build-time customization | `build_flags` | **Portage USE flags** |
| Catalogs / overlays | tomes + addenda | **AUR / overlays** |
| Build / trust | host-toolchain builds, signed binhost | **Pacman / Gentoo binhost** |

Nearest existing neighbors, each missing at least one defining axis:

- **Spack** — closest on *mechanics* (hashed multi-version store, imperative Python recipes,
  host-compiler builds, signed binary mirrors, environments ≈ profiles) but not a distro and no
  OS-level rollback.
- **GoboLinux** — closest in *spirit* (approachable per-package-versioned-store distro with shell
  recipes and `/usr` compatibility symlinks) but no content-addressing, no atomic generations, no
  binary-cache story, primary-only.
- **Nix / Guix** — the only ones with the full store + generations + dual-role, but via a
  functional-DSL path this model deliberately avoids. **Guix** (GNU's from-source-bootstrapped
  distro) is the closest overall precedent to Grimoire's trajectory.

## End-user experience on a Grimoire distro

- **Install/upgrade** are atomic and produce a new generation; multiple versions coexist; no
  partial-upgrade breakage.
- **Mistakes are reversible**: `grm rollback`, or pick a previous generation from the boot menu.
- **Continuity**: because `/grm` is the same constant everywhere, the same tool, commands, and
  binary cache serve both the distro and a secondary install on a macOS laptop.
- **Disk**: multiple versions + retained generations cost space; `grm gc` reclaims it.
- **Trust**: installs verify minisign signatures (TOFU on first tome add). Trust is in the signed
  builder, not an independent rebuild.

To *use* it feels like Arch/Homebrew; to *survive mistakes* it feels like NixOS — without the Nix
learning cliff.

## FHS compatibility: "real-file programs, sandbox libraries"

A store-based, non-FHS system breaks software that assumes fixed paths. The policy:

- **Executables (`/bin`, `/usr/bin`): populate with a generation-managed hard-link view** into the
  active profile. Because the profile contains real files (hard links into the store), shebangs,
  absolute-path `exec`s, and self-locating binaries all work normally — a deliberately *more*
  FHS-friendly stance than NixOS's symlink farm. The inherent cost is that a flat `/usr/bin` is a
  single namespace, so colliding `bin/foo` providers need a winner chosen at view-generation time
  (profile priority).
- **Libraries (`/lib`, `/usr/lib`): do NOT globally symlink.** A global lib symlink farm can hold
  only one version per soname, which collapses the store's multi-version guarantee and reintroduces
  the exact conflicts the store exists to prevent — and it would only ever serve *foreign* binaries,
  since Grimoire's own binaries resolve libraries via baked store RPATHs.
- **Foreign prebuilt binaries** get a bounded FHS/glibc **compat layer** instead (an `nix-ld` /
  `buildFHSEnv` / `steam-run` analogue): symlink the loader `/lib64/ld-linux-*.so` to a chosen
  compat glibc, plus a configurable default library path for their `DT_NEEDED` libs — a contained
  single-version world kept separate from the store.

This FHS-compat story (especially the loader/foreign-binary half) is **make-or-break for end-user
approachability** and belongs on the critical path alongside generations + GC roots.

## Non-goals and deferred decisions

- **Runtime relocatability (`$ORIGIN`/`@rpath`)** — explicitly out; the fixed store replaces it.
- **Install-time relocation (placeholder-prefix rewriting, Homebrew/conda-style)** — only relevant
  if the fixed-store commitment is ever abandoned; not pursued.
- **Windows as a distro target** — out of scope; the store/profile model targets Linux (primary)
  and macOS/Linux (secondary).

System configuration (`/etc`) is handled conventionally, like a traditional distro; "immutable"
applies to the store and to package generations/rollback, not to system config.

## Delta from today (migration sketch)

| Today | Target |
|---|---|
| `packages/<name>/<version>` under a user root | `/grm/store/<hash>-<name>-<version>` (fixed, content-addressed) |
| `ctx.prefix == staging tempdir` | `ctx.prefix == <store path>`; `--prefix=<store> DESTDIR=<staging>` |
| `<root>/bin` shims | per-user/system **profiles**: generation hard-link trees, `current` symlink, PATH + GC |
| `dist/index.nuon` keyed by name/version/target | substitution cache keyed by **store hash** (path = identity) |
| version resolution picks a version | resolution computes the **input hash / closure** |

Already-shipped prerequisites: native `.tar.gz`/`.tar.xz`/`.tar.zst` source extraction, and
symlink **preservation** in package archives with target validation (so versioned shared-library
symlink chains survive packaging).
