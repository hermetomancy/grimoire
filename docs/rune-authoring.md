# Rune Authoring Reference

A rune is a Nushell module exporting `package` (inert metadata) and `build` (the build
function). The binding rules live in [AGENTS.md §7](../AGENTS.md); this is the full reference.

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
    example: "bin/example"
    ex: "bin/example"  # capability alias
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

## The `ctx` record

| Field | Type | Meaning |
|---|---|---|
| `ctx.package_dir` | string | Staging root for this build. Install files here; Grimoire packs this directory into the archive. |
| `ctx.prefix` | string | Final install prefix (e.g. `/grm/store/<hash>/example-1.0.0`). Bake this into configure-time metadata so the package knows where it will live. |
| `ctx.store_path` | string | Alias for `ctx.prefix`. |
| `ctx.work_dir` | string | Scratch directory for build artifacts. Use for out-of-tree builds. |
| `ctx.target` | string | Target triple (e.g. `linux-x86_64-musl`). |
| `ctx.nproc` | int | Host parallelism for `-j`/`--parallel` flags (falls back to 4 when undetectable). |
| `ctx.sources.<name>.dir` | string | Extracted directory for an archive source — use this for tarballs. `null` for non-archive sources (patches, single files). |
| `ctx.sources.<name>.path` | string | The raw verified file in the cache. Use for non-archive sources (e.g. `patch -p1 -i ($ctx.sources.fix.path)`). |
| `ctx.sources.<name>.url` / `.sha256` | string | The declared origin and pinned hash, for runes that need to reference them. |
| `ctx.build_flags` | record | Key-value flags from the rune metadata. Use for feature toggles. |
| `ctx.env.PATH` | string | The managed build PATH (AGENTS.md §5). |
| `ctx.env.GRIMOIRE_VERBOSITY` | string | `"quiet"`, `"normal"`, or `"verbose"`. |
| `ctx.env.*` | string | Extra env vars from `BuildEnv` (e.g. `CC`, `CFLAGS` for musl static builds). |

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

## Build script patterns

**No `sh -c`.** Rune `build` functions are native Nushell. Use Nushell's own control flow,
variables, and external command invocation. Shelling out to `sh` forfeits error handling and
obscures the build logic; decompose complex steps instead of hiding them in a shell script.

**Use explicit parentheses for variable interpolation in external commands.** Bare
record-field access like `$ctx.prefix` or `$env.VAR` in external command position can be
parsed incorrectly by Nushell, silently producing wrong paths or empty strings. Always wrap:
`($ctx.prefix)`, `($env.VAR)`, `($nproc)` — for `ctx` fields and local variables alike.

**Parallel builds:** pass `-j($ctx.nproc)` to `make` and the equivalent to other build
systems. If `nproc` is absent, omit the flag and let the build system default.

**Out-of-tree builds:** use `ctx.work_dir` for build artifacts when the build system supports
it (CMake, Meson). This keeps the source tree clean and avoids packing build artifacts.

## Platform-conditional sources

A source may carry a `platform` glob (same syntax as dependency brackets); it is fetched and
hashed only for matching targets. This is how a fixed-output package pins different prebuilt
artifacts per platform — see `tome-core/runes/rust.rn` for the canonical example:

```rn
sources: {
  "macos-aarch64-darwin": {
    url: "https://example.com/tool-aarch64-apple-darwin.tar.xz"
    sha256: "sha256:..."
    platform: "macos-aarch64-darwin"
  }
}
```

Each target's store hash covers exactly its own filtered source set, so updating one
platform's artifact does not perturb the others' content addresses.

## Platform conditionals

Use `ctx.target` for platform-specific logic; prefer prefix matching over exact triples:

```rn
let is_macos = ($ctx.target | str starts-with "macos-")
let is_linux = ($ctx.target | str starts-with "linux-")
let is_musl = ($ctx.target | str ends-with "-musl")
```

Keep platform logic in the rune, not in Rust — the Rust side only provides the target triple.

A *build dependency* may carry a platform filter in brackets: `'linux-headers[linux-*]'`
includes the dep only when the target triple matches the glob (full triple or prefix).

## `bins` conventions

- The package `name` is always implicitly a bin (e.g. `example` → `bin/example`).
- Additional entries are **capabilities**: `ex: "bin/example"` means `ex` resolves to this
  package (see AGENTS.md §6 for resolution and `grm prefer` tie-breaking).
- Paths are relative to `package_dir` (e.g. `bin/foo`, `sbin/bar`, `libexec/baz`).
- Only declare commands that end users or other runes invoke; internal helper scripts do not
  need entries.

## `targets` filtering

A rune whose `targets` list is non-empty and does not include the current triple is skipped by
`grm tome build --all`. Use this for platform-specific packages (`linux-headers`, `musl`). An
empty or absent `targets` builds on every platform.

## Post-install notes

`notes` is a list of user-facing strings printed once after the install commits and replayed
by `grm info` ("add yourself to the docker group", "run `x --init` once"). Declare them
statically in `package`, or return them dynamically from `build` alongside discovered bins:

```rn
{ bins: { default: { example: "bin/example" } }, notes: ["compiled without TLS support"] }
```

## No sources

A rune may declare `sources: {}` and generate all outputs in `build` (e.g.
`toolchain-wrappers`, which writes wrapper scripts). Valid for meta-packages and pure-script
tools.
