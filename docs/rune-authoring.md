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
  homepage: "https://example.com/"
  license: "MIT"
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

Every field Grimoire parses. Unknown keys are ignored, so informational conventions like
`homepage` and `license` are welcome but carry no behavior (yet).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Package identifier: letters, digits, `_.+-`, starting with a letter or digit. No paths, no spaces. |
| `version` | string | yes | Semver, leniently parsed (`1.2` normalizes to `1.2.0`). Non-semver upstream versions must be normalized here; keep the real version string in the source `url`. |
| `summary` | string | no | One-line description shown by `grm search`/`info`. |
| `targets` | list\<string\> | no | Supported target triples. Non-empty + current triple absent ⇒ the rune is skipped by `grm tome build --all` and refuses to build. Empty/absent ⇒ builds everywhere. |
| `fixed_output` | bool | no (default `false`) | Declares that the output is fully determined by the sources — the build only fetches/repackages, never compiles. Switches the package to output addressing: its store hash covers name + version + (platform-filtered) sources + target only, excluding the build environment and dependency closure. Use for repackaged prebuilts (see `rust.rn`); never for anything that invokes a compiler. Grimoire trusts the declaration — do not lie. |
| `sources` | record | no | Verified inputs; see [Sources](#sources). `sources: {}` is valid for generated-output packages. |
| `deps` | record | no | Build and runtime dependencies; see [Dependencies](#dependencies). |
| `bins` | record | no | Target-keyed executable declarations; see [Bins and capabilities](#bins-and-capabilities). |
| `build_flags` | record\<string, string\> | no | Free-form feature toggles surfaced to the build as `ctx.build_flags`. Addenda may patch them; they are part of a compiled package's store hash. |
| `provides` | list\<string\> | no | Capability names this package supplies beyond its bins. Used by the solver pre-build; **overwritten at pack time by discovered bin names** (see below), so declare it for capabilities the solver must know before any build exists. |
| `libs` | list\<string\> | no | Library base names (`foo` for `libfoo.so`). Like `provides`, replaced by discovery at pack time. |
| `notes` | list\<string\> | no | User-facing post-install notes, printed once after install commits and replayed by `grm info`. |
| `target` | string | (archives only) | The concrete triple an archive was built for. Written by `grm tome build` into archive metadata; not authored in runes. |
| `store_path` | string | (archives only) | The content-addressed store basename embedded in archives. Not authored in runes. |

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
| `platform` | no | Target glob (dependency-bracket syntax: `macos-*`, `linux-x86_64-musl`). Non-matching sources are neither fetched nor folded into the store hash — how a fixed-output package pins a different artifact per platform (`rust.rn` is the canonical example). |

Archive sources (`.tar.zst`/`.tar.gz`/`.tar.xz`) are extracted automatically (path-validated
first, same rules as package archives) and exposed as `ctx.sources.<name>.dir`. Non-archive
sources (patches, single files) are exposed as `ctx.sources.<name>.path` only.

## Dependencies

```rn
deps: {
  build: {
    default: ["cmake" "make"]
    linux: ["linux-headers"]
    "linux-x86_64-musl": ["some-exact-triple-tool"]
  }
  runtime: ["libc" { name: "openssl", version: ">=3" } "linux-headers[linux-*]"]
}
```

- **`deps.build`** — tools needed during the build. Target-keyed: `default` applies
  everywhere, an OS key (`linux`) merges over it, an exact triple merges over both. Their
  `bin/` dirs join the managed build PATH and their prefixes are layered into discovery
  variables (`CMAKE_PREFIX_PATH`, `PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, `ACLOCAL_PATH`,
  `<DEP>_PREFIX`). Declare every non-POSIX tool the build invokes — and remember the
  self-hosted ambient userland is toybox, which ships no `make`.
- **`deps.runtime`** — packages required at execution time; resolved by the solver and
  installed into the active generation.
- **Entry forms** (anywhere a dependency appears): a bare name (`"libc"`, any version), a
  record (`{ name: "openssl", version: ">=3" }`, semver requirement), or a bracket string
  (`"linux-headers[linux-*]"`) gating the dep on a target glob.
- A dependency name may be a **capability** rather than a literal package (`"awk"` resolves
  to any provider; `grm prefer` breaks ties). Use the capability when any implementation
  works; use the literal name when you need that one.
- **`deps.features`** — *(future work, AGENTS.md §6)* execution-time FHS-compat capabilities.

## Bins and capabilities

`bins` is **target-keyed** like `deps.build`: `default` → OS key → exact triple, each level
merging over the previous. Each inner record maps a command name to a path relative to
`package_dir`:

```rn
bins: {
  default: { gawk: "bin/gawk", awk: "bin/gawk" }
  macos: { gawk: "bin/gawk-mac" }
}
```

- Any key that differs from the package `name` is a **capability**: `awk` resolves to this
  package in dependency resolution, and `grm prefer awk <pkg>` chooses among multiple
  providers.
- **Discovery overrides declaration at pack time.** After a successful build, every
  executable staged under `bin/` is discovered; the discovered set replaces the `default`
  key in the packed archive's metadata, `provides` is reset to the discovered names, and
  `libs` to the discovered libraries. Static declarations therefore matter most *before* a
  build exists: they tell the solver (and `grm tome build --all`'s ordering) what the rune
  will provide. Keep them accurate.
- Only declare commands end users or other runes invoke; internal helper scripts need no
  entries.

## The `ctx` record

| Field | Type | Meaning |
|---|---|---|
| `ctx.package_dir` | string | Staging root for this build. Install files here; Grimoire packs this directory into the archive. |
| `ctx.prefix` | string | Final install prefix (e.g. `/grm/store/<hash>-example-1.0.0`). Bake this into configure-time metadata so the package knows where it will live. |
| `ctx.store_path` | string | Alias for `ctx.prefix`. |
| `ctx.work_dir` | string | Scratch directory for build artifacts. Use for out-of-tree builds. `HOME`, temp, and XDG dirs all point inside it. |
| `ctx.target` | string | Target triple (e.g. `linux-x86_64-musl`). |
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
| strings | `str starts-with`, `str ends-with`, `str trim`, `str replace` (`-r` regex, `-a` all) |
| filters | `get`, `merge`, `columns`, `lines`, `first`, `is-empty` |

Anything not listed does not exist in the rune engine — `from json`, `http get`, `where`,
`each`, and the rest of nushell's surface are deliberately absent. This keeps rune behavior
stable across Nushell upgrades and the dependency tree small. If a rune genuinely needs a
missing command, it is added to `src/nu/commands/` and to this table in the same commit
(AGENTS.md §15.4) — or the build step uses an external tool instead.

## Build script patterns

**No `sh -c`.** Rune `build` functions are native Nushell. Use Nushell's own control flow,
variables, and external command invocation. Shelling out to `sh` forfeits error handling and
obscures the build logic; decompose complex steps instead of hiding them in a shell script.

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
