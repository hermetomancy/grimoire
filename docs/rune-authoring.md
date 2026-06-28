# Rune Authoring Reference

A rune is a Nushell module exporting `package` (inert metadata) and `build` (the build
function). The binding rules live in [AGENTS.md §7](../AGENTS.md); this is the full reference.

This document is the contract: it is kept in lockstep with the parser
(`src/model/package.rs`), the build context (`src/nu/runtime/mod.rs`), and the rune command
set (`src/nu/commands/`). If you change any of them, update this file in the same commit
(AGENTS.md §15.4).

## Structure

```rn
export const package = {
  name: "example"
  version: "1.0.0"
  summary: "One-line description"
  meta: {
    homepage: "https://example.com/"
    license: "MIT"
  }
  targets: ["linux-x86_64-musl" "macos-aarch64-darwin"]

  sources: {
    main: {
      url: "https://example.com/example-1.0.0.tar.gz"
      sha256: "sha256:..."
    }
  }

  deps: {
    build: { default: ["cmake"] }
    runtime: ["libc"]
  }

  bins: {
    default: {
      example: "bin/example"
      ex: "bin/example"  # capability alias
    }
  }

  notes: ["Run `example --init` once before first use."]
}

export def build [ctx] {
  let source_dir = ($ctx.sources.main.dir | path join "example-1.0.0")
  cd $source_dir
  ./configure --prefix=($ctx.prefix)
  make -j($ctx.nproc)
  make install DESTDIR=($ctx.package_dir)
}
```

## The `package` record

Every field Grimoire parses. Unknown top-level keys are errors: put informational conventions
such as homepage/license under `meta`, which is inert data and carries no behavior yet.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Package identifier: letters, digits, `_.+-`, starting with a letter or digit. No paths, no spaces. |
| `version` | string | yes | Semver, leniently parsed (`1.2` normalizes to `1.2.0`). Non-semver upstream versions must be normalized here; keep the real version string in the source `url`. |
| `summary` | string | no | One-line description shown by `grm search`/`info`. |
| `meta` | record | no | Inert informational metadata ignored by Grimoire for now. Use for conventions like `homepage` and `license` instead of adding unknown top-level fields. |
| `targets` | list\<string\> | no | Recognized exact target triples (`linux-{x86_64,aarch64}-{musl,gnu}`, `macos-{x86_64,aarch64}-darwin`, `freebsd-{x86_64,aarch64}-unknown`). Non-empty + current triple absent ⇒ the rune is skipped by `grm tome build --all` and refuses to build. Empty/absent ⇒ builds everywhere. Source-build environments are wired today for `linux-*-musl` and `macos-*-darwin`; `linux-*-gnu` and FreeBSD are metadata/index targets until their managed floors land. |
| `fixed_output` | bool | no (default `false`) | Declares that the output is fully determined by declared sources — the build only fetches/repackages, never compiles. Requires at least one source and no build deps. Switches the package to output addressing: its store hash covers name + version + (platform-filtered) sources + target only, excluding the build environment and dependency closures. Use for repackaged prebuilts (see `rust-stage0.rn`); never for anything that invokes a compiler. |
| `sources` | record | no | Verified inputs; see [Sources](#sources). `sources: {}` is valid for generated-output packages. |
| `deps` | record | no | Build and runtime dependencies; see [Dependencies](#dependencies). |
| `bins` | record | no | Target-keyed executable declarations; see [Bins and capabilities](#bins-and-capabilities). |
| `build_flags` | record\<string, string\> | no | Free-form feature toggles surfaced to the build as `ctx.build_flags`. Addenda may patch them; they are part of a compiled package's store hash. |
| `provides` | list\<string\> | no | Capability names this package supplies beyond its bins. Used by the solver pre-build and preserved at pack time alongside discovered executable names and declared bin aliases. |
| `libs` | list\<string\> | no | Library base names (`foo` for `libfoo.so`). Like `provides`, merged with post-build discovery at pack time, so declared non-file capabilities survive while discovered library files are added. |
| `notes` | list\<string\> | no | User-facing post-install notes, printed once after install commits and replayed by `grm info`. |
| `upstream_version` | string | no | The real upstream version string when `version` had to be normalized to semver (see [Version policy](#version-policy)). Display only — shown by `grm info`, never ordered. |
| `conflicts` | list\<string\> | no | Installed packages this one cannot coexist with (bare names). Only *linked* coexistence conflicts, symmetrically — store-only packages on either side are cache, not environment, so a package may conflict with its own store-only build dep (`rust` vs `rust-stage0`). Checked when a linked install lands *and* when a store-only package is promoted into the linked set; the conflicting package must be removed first (or be replaced in the same command). |
| `replaces` | list\<string\> | no | Package names this one supersedes. Installing this package removes them in the same transaction and migrates their requested/held intent. A bare `grm upgrade` treats an available replacer as the upgrade for the replaced name — this is how renames work. |
| `split_from` | string | no | Declares this rune a **split member** of the named parent rune in the same `runes/` directory; see [Split packages](#split-packages). A split member must not declare `sources`, build deps, `build_flags`, `targets`, `fixed_output`, or a `build` function. |
| `files` | list\<string\> | with `split_from` | Glob patterns claiming this member's files from the parent build's output, relative to the payload root. `*`/`?` stay within one path component; `**` crosses directories. Required (non-empty) exactly when `split_from` is set. |
| `format` | int | (archives only) | Archive metadata format version. Written by Grimoire into packed archives; not authored in runes. |
| `target` | string | (archives only) | The concrete triple an archive was built for. Written by `grm tome build` into archive metadata; not authored in runes. |
| `store_path` | string | (archives only) | The content-addressed store basename embedded in archives. Not authored in runes. |

## Version policy

`version` must be semver-orderable: every comparison in the solver, the lockfile, and
`grm upgrade` goes through strict semver ordering, and there is no secondary ordering
scheme. When the upstream scheme is not semver, normalize it and record the real string in
`upstream_version` (and use the real one in the source `url`):

| Upstream scheme | Example | Normalize as |
|---|---|---|
| letter suffix releases | `1.1.1w` | `1.1.1+w` is **wrong** (build metadata never orders); count the letter: `1.1.1` → 23rd letter patch ⇒ `1.1.123` or adopt the next upstream scheme at the bump |
| date releases | `2025a`, `2024.07.02` | `2025.1.0`, `2024.7.2` |
| `p`-suffix patches | `9.9p1` | `9.9.1` |

The mapping is a per-package convention — pick one when the rune is first written and keep
it monotonic; ordering correctness is the rune author's responsibility. Pre-release pins of
unreleased software use semver prerelease (`0.1.0-dev.20260612`), which orders below the
release and chronologically among themselves.

## Sources

```rn
sources: {
  main: { url: "https://...", sha256: "sha256:..." }
  fix:  { url: "../sources/portability.patch", sha256: "sha256:..." }
  bin:  { url: "https://.../tool-aarch64.tar.xz", sha256: "sha256:...", platform: "macos-aarch64-darwin" }
}
```

| Field | Required | Meaning |
|---|---|---|
| `url` | yes | `https://` (or `http://`), or a path relative to the rune's directory (vendored files under the tome's `sources/`). |
| `sha256` | yes | 64 hex digits, optional `sha256:` prefix. Every source is verified before the build can read it; a mismatch is fatal. |
| `platform` | no | Target glob (dependency-bracket syntax: `macos-*`, `linux-x86_64-musl`). Unlike `targets`, this is a selector and may be an OS shorthand or glob. Non-matching sources are neither fetched nor folded into the store hash — how a fixed-output package pins a different artifact per platform (`rust.rn` is the canonical example). |
| `host_libc` | no | Build-host libc selector (`glibc` or `musl`) for bootstrap sources that must execute on the build host. Non-matching sources are neither fetched nor folded into the store hash. Omit for ordinary target-selected sources. |

Archive sources (`.tar.zst`/`.tar.gz`/`.tar.xz`) are extracted automatically (path-validated
first, same rules as package archives) and exposed as `ctx.sources.<name>.dir`. A URL whose
final path segment has *no* extension — codeload.github.com commit tarballs
(`.../tar.gz/<sha>`) — is sniffed by magic bytes instead; any URL with an extension keeps
its extension-derived meaning, so a `.patch.gz` never extracts as a tarball. Non-archive
sources (patches, single files) are exposed as `ctx.sources.<name>.path` only.

## Dependencies

```rn
deps: {
  build: {
    default: ["cmake" "gmake"]
    linux: ["linux-headers"]
    "linux-x86_64-musl": ["some-exact-triple-tool"]
  }
  runtime: ["libc" { name: "openssl", version: ">=3" } "linux-headers[linux-*]"]
}
```

- **`deps.build`** — tools needed during the build. Target-keyed: `default` applies
  everywhere, an OS key (`linux`, `macos`, `freebsd`) merges over it, an exact supported
  triple merges over both. Typos and globs are rejected here; use bracket/platform selectors
  on individual dependencies when you need glob matching. Their
  `bin/` dirs join the managed build PATH and their prefixes are layered into discovery
  variables (`CMAKE_PREFIX_PATH`, `PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, `ACLOCAL_PATH`,
  `<DEP>_PREFIX`). Declare every non-POSIX tool the build invokes — and remember the
  self-hosted userland floor (uutils/dash/mawk/gsed/ggrep/gtar) ships no `make`. Out-of-core packages can
  declare `build-env` instead of enumerating the toolchain. The resolved build-dependency closure
  is part of a compiled package's store hash, so changing a build tool's recipe or version moves
  every package built with it to a new address.
- **`deps.runtime`** — packages required at execution time; resolved by the solver and
  installed into the active generation.
- **Entry forms** (anywhere a dependency appears): a bare name (`"libc"`, any version), a
  record (`{ name: "openssl", version: ">=3" }`, semver requirement), or a bracket string
  (`"linux-headers[linux-*]"`) gating the dep on a target glob.
- A dependency name may be a **capability** rather than a literal package (`"awk"` resolves
  to any provider; `grm pkg prefer` breaks ties). Use the capability when any implementation
  works; use the literal name when you need that one. Resolution order is deterministic —
  the `grm pkg prefer` choice, else an installed provider, else the first provider by name —
  because the chosen provider folds into the dependent's store hash: building against
  `gawk`'s awk and `mawk`'s awk are different content at different addresses, and a prebuilt
  only substitutes for users whose resolution matches the builder's.
- **`deps.features`** — *(future work, AGENTS.md §6)* execution-time FHS-compat capabilities.

## Bins and capabilities

`bins` is **target-keyed** like `deps.build`: `default` → OS key → exact supported triple,
each level merging over the previous. Typos and globs are rejected here. Each inner record
maps a command name to a path relative to `package_dir`:

```rn
bins: {
  default: { gawk: "bin/gawk", awk: "bin/gawk" }
  macos: { gawk: "bin/gawk-mac" }
}
```

- Any key that differs from the package `name` is a **capability**: `awk` resolves to this
  package in dependency resolution, and `grm pkg prefer awk <pkg>` chooses among multiple
  providers.
- **Discovery merges with declaration at pack time.** After a successful build, every
  executable staged under `bin/` is discovered; discovered names win on collision, but a
  declared *alias* — a second command name whose path is a real file, like
  `awk: "bin/gawk"` — survives into the packed metadata. `provides` becomes the union of
  declared non-bin capabilities and the merged executable name set; `libs` becomes the
  discovered libraries. Static declarations still matter *before* a build exists: they tell
  the solver (and `grm tome build --all`'s ordering) what the rune will provide. Keep them
  accurate.
- Only declare commands end users or other runes invoke; internal helper scripts need no
  entries.

### Naming rule: implementations own their name, the generic name is a capability

A standard utility with multiple real implementations (make, sed, awk, grep, tar, …) is
packaged under its **implementation name**, and ships **both** command names:

| Package | Bins | Wrong |
|---|---|---|
| `gmake` | `gmake`, `make` | a package named `make` — *which* make? |
| `gsed`  | `gsed`, `sed`   | `gnu-sed` — the package name should match the primary bin |
| `gawk`  | `gawk`, `awk`   | |
| `gtar`  | `gtar`, `tar`   | |

The generic name (`make`, `sed`, `awk`) is then a **capability** with potentially many
providers — never assume what the platform floor provides (GNU on glibc distros, busybox
on Alpine, the managed floor (uutils/dash/mawk/gsed/ggrep/gtar) once bootstrapped):

- **Consumers** depend on and invoke the *generic* name when any implementation serves,
  and the *implementation* name when specific semantics are required (`gsed` for GNU `T`
  branches, `gmake` for GNU pattern rules). The dependency and the invocation should
  agree.
- **Declared build deps outrank the core floor on PATH**, so declaring `gsed` makes plain
  `sed` mean GNU sed *for that build* — declaration is specificity.
- When a user explicitly installs an ambiguous capability (`grm install sed` with several
  providers and no preference), Grimoire asks which implementation they mean and records
  the answer as a `grm pkg prefer` choice. Rune-declared deps never prompt: they resolve
  deterministically (preference → installed provider → first by name).

Single-implementation tools (`cmake`, `python3`, `llvm`) keep their upstream names — there
is no "which cmake" question to encode.

The same no-assumptions rule applies to **libraries**: the only host floor a build may
lean on is libc and, on macOS, the platform SDK. Anything else a build links — zlib,
bzip2, ncurses, readline — must come from the store as a declared dep (or be explicitly
disabled at configure time), never picked up from whatever the host happens to ship. A
statically linked library is a build dep only; one whose store path survives into the
output (a shared library, or a compiled-in data path like ncurses' default terminfo
directory) must also be a runtime dep so GC keeps it alive.

## The `ctx` record

| Field | Type | Meaning |
|---|---|---|
| `ctx.package_dir` | string | Staging root for this build. Install files here; Grimoire packs this directory into the archive. |
| `ctx.prefix` | string | Final install prefix (e.g. `/grm/store/<hash>-example-1.0.0`). Bake this into configure-time metadata so the package knows where it will live. |
| `ctx.work_dir` | string | Scratch directory for build artifacts. Use for out-of-tree builds. `HOME`, temp, and XDG dirs all point inside it. |
| `ctx.target` | string | Target triple (e.g. `linux-x86_64-musl`). |
| `ctx.host_libc` | string | Build-host libc selector: `glibc`, `musl`, or `none`. This is distinct from the target ABI and exists for bootstrap runes that must choose a seed executable for the build host. |
| `ctx.nproc` | int | Host parallelism for `-j`/`--parallel` flags (falls back to 4 when undetectable). |
| `ctx.sources.<name>.dir` | string | Extracted directory for an archive source — use this for tarballs. `null` for non-archive sources (patches, single files). |
| `ctx.sources.<name>.path` | string | The raw verified file in the cache. Use for non-archive sources (e.g. `patch -p1 -i ($ctx.sources.fix.path)`). |
| `ctx.sources.<name>.url` / `.sha256` | string | The declared origin and pinned hash, for runes that need to reference them. |
| `ctx.build_flags` | record | Key-value flags from the rune metadata (possibly patched by addenda). Use for feature toggles. |
| `ctx.env.PATH` | string | The managed build PATH (AGENTS.md §5). |
| `ctx.env.GRIMOIRE_VERBOSITY` | string | `"quiet"`, `"normal"`, or `"verbose"`. |
| `ctx.env.<DEP>_PREFIX` | string | The store prefix of each declared build dep, uppercased (`LLVM_PREFIX`, `CLANG_PREFIX`). Also set in the process env, so `$env.LLVM_PREFIX` works in external command position. |
| `ctx.env.*` | string | The other managed discovery variables (`CMAKE_PREFIX_PATH`, `PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, `ACLOCAL_PATH`) plus target extras (e.g. `CC`/`CFLAGS` for musl static builds). |

## The build return value

`build` may return nothing, or a record merged back into the package metadata:

```rn
{ bins: { default: { example: "bin/example" } }, notes: ["compiled without TLS support"] }
```

| Field | Meaning |
|---|---|
| `bins` | Target-keyed bins, merged over the static declaration (then subject to discovery, above). |
| `notes` | Dynamic post-install notes, appended (deduplicated) to the static `notes`. |

## Installation convention

**Install into `ctx.package_dir`, not `ctx.prefix`.** `package_dir` is the staging area that
gets packed into the archive; `prefix` is the final location after extraction. For autotools:

```rn
./configure --prefix=($ctx.prefix)
make install DESTDIR=($ctx.package_dir)
```

For CMake:

```rn
cmake -S . -B build -DCMAKE_INSTALL_PREFIX=($ctx.prefix)
cmake --build build
cmake --install build --prefix ($ctx.package_dir)
```

## The command subset

Runes target a **defined subset** of Nushell, not whatever a full nu install ships. The core
language (`let`, `mut`, `if`, `for`, `match`, `def`, `do`, `try`, `error make`, `describe`,
operators, string interpolation, closures) comes from `nu-cmd-lang`; on top of it Grimoire
registers exactly these commands (`src/nu/commands/`):

| Family | Commands |
|---|---|
| system | external invocation (bare or `^cmd`), `complete` |
| filesystem | `mkdir`, `save` (verbatim; `--force`/`--append`), `open` (always raw), `rm` (`-r`/`-f`), `cp` (`-r`), `ls` (name/type/size), `cd` |
| path | `path join`, `path exists`, `path type`, `path basename`, `path dirname` |
| strings | `str starts-with`, `str ends-with`, `str contains`, `str trim`, `str replace` (`-r` regex, `-a` all) |
| filters | `get`, `merge`, `columns`, `lines`, `first`, `is-empty` |

Anything not listed does not exist in the rune engine — `from json`, `http get`, `where`,
`each`, and the rest of nushell's surface are deliberately absent. This keeps rune behavior
stable across Nushell upgrades and the dependency tree small. If a rune genuinely needs a
missing structured operation, add it to `src/nu/commands/` and to this table in the same commit
(AGENTS.md §15.4). Do not route around a missing rune command with an undeclared host utility.

The file-loading parse keywords (`use`, `source`, `source-env`, `overlay`, `module`,
`register`, `plugin`) are **rejected outright** when Grimoire reads a rune's metadata: they
load files *during parsing*, which would make reading the inert `package` record open arbitrary
host files (AGENTS.md §4.3). Rune metadata is data, not a program — keep `package` a literal
record.

## Build script patterns

**No `sh -c`.** Rune `build` functions are native Nushell. Use Nushell's own control flow,
variables, and external command invocation. Shelling out to `sh` forfeits error handling and
obscures the build logic; decompose complex steps instead of hiding them in a shell script.

**External commands must be owned by the build environment.** Invoking upstream build tools
(`make`, `cmake`, `cargo`, `cc`) is expected when those tools come from declared build deps or
the package's own source tree. Generic POSIX utilities may come from the managed floor (or, during
bootstrap/`--impure`, the documented ambient tail), but a rune must not use an arbitrary host
utility to stand in for missing Nushell functionality. Prefer the native rune commands above for
filesystem work (`cp`, `rm`, `path`, string transforms); when a generic external utility is
genuinely required, make sure the providing package/floor contract is explicit.

**Use explicit parentheses for variable interpolation in external commands.** Bare
record-field access like `$ctx.prefix` or `$env.VAR` in external command position can be
parsed incorrectly by Nushell, silently producing wrong paths or empty strings. Always wrap:
`($ctx.prefix)`, `($env.VAR)`, `($nproc)` — for `ctx` fields and local variables alike.

**Parallel builds:** pass `-j($ctx.nproc)` to `make` and the equivalent to other build
systems.

**Out-of-tree builds:** use `ctx.work_dir` for build artifacts when the build system supports
it (CMake, Meson). This keeps the source tree clean and avoids packing build artifacts.

## Platform conditionals

Use `ctx.target` for platform-specific logic; prefer prefix matching over exact triples:

```rn
let is_macos = ($ctx.target | str starts-with "macos-")
let is_linux = ($ctx.target | str starts-with "linux-")
let is_musl = ($ctx.target | str ends-with "-musl")
```

Keep platform logic in the rune, not in Rust — the Rust side only provides the target triple.

## No sources

A rune may declare `sources: {}` and generate all outputs in `build` (e.g.
`toolchain-wrappers`, which writes wrapper scripts). Valid for meta-packages and pure-script
tools.

## Split packages

One build can produce several packages: a parent rune builds normally, and **companion
runes** in the same `runes/` directory carve their slice out of its output. The canonical
example is `clang.rn` splitting from `llvm.rn` — one LLVM monorepo build, two packages.

```rn
# clang.rn — a complete package record, but no sources and no build function.
export const package = {
  name: "clang"
  version: "19.1.0"          # must equal the parent's version
  summary: "Clang C/C++ compiler"
  split_from: "llvm"
  files: ["bin/clang*" "lib/clang/**" "lib/libclang*" "include/clang/**"]
  deps: { runtime: ["llvm"] }   # references to fellow group members are ordinary deps
}
```

Semantics:

- **Membership is discovered**, not declared by the parent: any rune in the directory with
  `split_from: "llvm"` joins llvm's group. The parent rune does not mention its members.
- **One build.** Building or installing any member runs the parent's `build` once (with the
  parent's sources, build deps, and `build_flags`), then partitions the payload: each
  member takes the files its globs claim, the parent keeps the remainder. Sibling archives
  land in the build cache, so installing another member later reuses them.
- **Partition errors are hard errors**: a file claimed by two members, a member whose globs
  claim nothing at all (individual globs may match nothing), or a relative symlink whose
  target was claimed away from it. Directories emptied by claims are pruned from the parent.
- **Addressing is group-wide.** All members derive their store hashes from one group hash
  covering the parent's inputs, every member's rune bytes, and the union of all members'
  *external* runtime deps. Editing any member's rune (or globs) re-addresses the whole
  group — correct, since the parent's remainder changes too. Member archives embed every
  group rune under `.grimoire/group/` so their addresses are recomputable and auditable.
- **Per-member metadata**: each member keeps its own summary, runtime deps, `provides`,
  `notes`, and discovered bins/libs. Build deps, sources, `build_flags`, and `targets` live
  on the parent only; declaring them on a member is a parse error.
- **One prefix caveat**: the whole group configures against the *parent's* store prefix,
  while each member's files install into its own store path. Members must locate shared
  resources relative to their own binaries — absolute paths baked to the parent prefix
  point at the remainder only.
