# Core Toolchain: Clang/LLVM + musl libc Design

**Date:** 2026-06-06
**Status:** Approved
**Approach:** A (Self-hosted bootstrap)

## Context

Grimoire currently builds core packages with the host GCC and links against the host glibc. This creates two problems:

1. **Foreign binary incompatibility**: A core package built on Debian 12 may fail on Arch Linux if glibc versions differ.
2. **Toolchain inconsistency**: macOS and FreeBSD default to Clang; Linux defaults to GCC. Core packages behave differently across platforms.

This design replaces the host GCC/glibc foundation with a self-built Clang/LLVM + musl libc toolchain. Core packages build against musl on Linux and produce fully static binaries that run on any Linux distro. macOS and FreeBSD continue to use their native libc (musl is Linux-only) but now share the same Clang compiler.

The FHS compat layer (TODO #1) is still required for user packages from third-party tomes that may be dynamically linked against glibc. Its scope is reduced but not eliminated.

---

## Section 1: Philosophy

Core is the **bare minimum** required to bootstrap a development environment. It contains only runes that are needed to build other core runes. Everything else lives in user tomes.

Core is **almost entirely non-GNU**. The sole exception is GNU Make, which is unavoidable for practical C/C++ package building (see §8).

---

## Section 2: Target Platform Matrix

### Current targets

```
linux-x86_64-gnu
linux-aarch64-gnu
macos-x86_64-darwin
macos-aarch64-darwin
freebsd-x86_64-unknown
freebsd-aarch64-unknown
```

### New targets

Add musl variants for Linux:

```
linux-x86_64-musl
linux-aarch64-musl
```

Keep gnu variants for backward compatibility and user packages:

```
linux-x86_64-gnu
linux-aarch64-gnu
```

### Target selection rules

- **Core tome** (`tome-core`): builds for `-musl` on Linux, `-darwin` on macOS, `-unknown` on FreeBSD.
- **User tomes**: default to `-gnu` on Linux unless the tome explicitly opts into `-musl`.
- The active target is determined by `host_triple()` at runtime.

---

## Section 3: Core Rune Inventory

### Linux-only (2 runes)

| Rune | Purpose |
|------|---------|
| `linux-headers` | Kernel syscall headers required by musl |
| `musl` | libc (static) for Linux-musl targets |

### All platforms (6 runes)

| # | Rune | Purpose | Needed By |
|---|------|---------|-----------|
| 1 | `compiler-rt` | Clang builtins and runtime support | `clang` |
| 2 | `llvm` | LLVM libs, `lld`, `llvm-ar`, `llvm-strip`, `llvm-nm`, `llvm-ranlib` | `clang` |
| 3 | `clang` | C/C++ compiler | Everything else |
| 4 | `make` | GNU Make — **the sole GNU exception** | `busybox`, `fhs-compat`, and any Makefile-based package |
| 5 | `busybox` | POSIX userland (200+ utilities via symlinks) | Replaces host `/bin` entirely |
| 6 | `fhs-compat` | Baseline FHS tree for dynamically-linked foreign binaries | User packages with `deps.features` |

**Linux total: 8 runes. FreeBSD/macOS total: 6 runes.**

### What's deliberately excluded from core

| Excluded | Why | Where it lives |
|----------|-----|----------------|
| `cmake` | Not needed by any core rune. LLVM/Clang bootstrap uses **host** CMake. User tome. |
| `samurai` / `ninja` | Not needed by any core rune. Core packages build with Make. User tome. |
| `perl` | Only needed for autotools-based packages. User tome. |
| `python` | Only needed for some build scripts. User tome. |
| `m4` | Only needed to *generate* autotools configure scripts. User tome. |
| `zlib` | Common but not needed by core. User tome. |
| `libressl` / `openssl` | Common but not needed by core. User tome. |
| `ncurses` | Only needed by TUI programs. User tome. |
| `bash`, `vim`, `git`, `curl`, `ssh` | User-facing tools, not bootstrap infrastructure. User tome. |

---

## Section 4: Platform-Conditional Build Dependencies

### Syntax

Platform-conditional build dependencies use bracket suffixes on dep names:

```nuon
deps: {
  build: [
    'llvm'
    'make'
    'linux-headers[linux]'
    'musl[linux]'
  ]
  runtime: []
}
```

**Rules:**
- Strings without brackets are universal deps (all platforms).
- Strings with `[pattern]` are platform-conditional. The pattern is matched against the current target triple using simple glob rules:
  - `*` matches any sequence of characters.
  - If the pattern contains `-` or `*`, match against the **full target triple**.
  - If the pattern is a simple word (no `-`, no `*`), also match against the **OS component** of the triple.
- The resolver takes the universal set plus matching platform-specific deps.
- Multiple platforms per dep: `libfoo[linux,macos]` (future extension).
- No brackets = universal. This is backward compatible with all existing runes.

**Examples:**

| Dep | Target `linux-x86_64-musl` | Target `linux-x86_64-gnu` | Target `macos-aarch64-darwin` |
|-----|---------------------------|--------------------------|------------------------------|
| `'linux-headers[linux]'` | ✅ Match (OS = linux) | ✅ Match (OS = linux) | ❌ No match |
| `'musl[linux-*-musl]'` | ✅ Match (full triple) | ❌ No match | ❌ No match |
| `'libSystem[macos]'` | ❌ No match | ❌ No match | ✅ Match (OS = macos) |
| `'libSystem[macos-aarch64-*]'` | ❌ No match | ❌ No match | ✅ Match (full triple) |
| `'llvm'` | ✅ Match (universal) | ✅ Match (universal) | ✅ Match (universal) |

### Example: musl rune deps

```nuon
export const package = {
  name: 'musl'
  version: '1.2.5'
  deps: {
    build: [
      'linux-headers[linux]'
    ]
    runtime: []
  }
}
```

On Linux, `musl` has `linux-headers` as a build dep. On FreeBSD/macOS, `linux-headers` is silently omitted.

### Example: compiler-rt with precise musl targeting

```nuon
export const package = {
  name: 'compiler-rt'
  version: '19.1.0'
  deps: {
    build: [
      'llvm'
      'musl[linux-*-musl]'
    ]
    runtime: []
  }
}
```

`llvm` is a universal build dep. `musl` is only included when building for a Linux-musl target (e.g., `linux-x86_64-musl`), not for Linux-gnu or macOS.

### Example: clang rune deps

```nuon
export const package = {
  name: 'clang'
  version: '19.1.0'
  deps: {
    build: [
      'compiler-rt'
      'llvm'
    ]
    runtime: []
  }
}
```

No platform brackets needed — `compiler-rt` and `llvm` are universal.

---

## Section 5: Toolchain Bootstrap

### Bootstrap order

The toolchain must be built before any core package. The bootstrap sequence is:

1. **musl libc** — built with host GCC + host Make. Small (~1MB source), fast build (~2 minutes).
2. **LLVM + compiler-rt** — built with host GCC + host CMake. Large (~800MB source), slow build (~30-60 minutes).
3. **Clang** — built with host GCC + host CMake against the self-built LLVM.
4. **Core packages** — built with self-built Clang + musl (Linux) or host libc (FreeBSD/macOS).

### Bootstrap packages

| Package | Purpose | Built With | Links Against |
|---------|---------|-----------|---------------|
| `linux-headers` | Kernel syscall headers | Host GCC | N/A |
| `musl` | libc headers and static library | Host GCC | N/A |
| `compiler-rt` | Clang builtins and runtime | Host GCC + host CMake | Host libc |
| `llvm` | LLVM libraries, `lld`, `llvm-ar`, etc. | Host GCC + host CMake | Host libc |
| `clang` | C/C++ compiler | Host GCC + host CMake + built LLVM | Host libc |
| `make` | GNU Make | Self-built Clang + musl | musl (Linux) / host libc (FreeBSD/macOS) |
| `busybox` | POSIX userland | Self-built Clang + musl | musl (Linux) / host libc (FreeBSD/macOS) |
| `fhs-compat` | FHS tree | Self-built toolchain | N/A |

**Note:** LLVM and Clang are build tools. They link against the host libc (glibc on Linux) — this is acceptable because they are not runtime dependencies of core packages. Only the final core packages link against musl.

### Bootstrap dependency chain

```
host-gcc + host-make + host-cmake
  → linux-headers[linux]
  → musl[linux]
  → compiler-rt + llvm + clang   (built together from LLVM monorepo)
  → make                          (using self-built clang + musl)
  → busybox                       (using self-built clang + musl + make)
  → fhs-compat                    (using self-built toolchain)
```

After `busybox`, host dependence is gone. Every subsequent build uses Grimoire's own POSIX tools.

### Bootstrap caching

The LLVM build is slow. Grimoire should cache the bootstrapped toolchain:

- The toolchain's `store_hash` is computed from its input hash (musl source + LLVM source + host build env).
- If a substitute exists for the toolchain packages, fetch and install it instead of building from source.
- This means the LLVM build is paid once per host configuration, not once per Grimoire installation.

---

## Section 6: Core Package Builds

### Compiler flags for musl targets

Core packages on Linux-musl targets use:

```bash
CC=clang
CXX=clang++
CFLAGS="-target x86_64-linux-musl -static"
LDFLAGS="-target x86_64-linux-musl -static -fuse-ld=lld"
```

The `clang` rune provides a `grm-clang` wrapper script that injects these flags automatically, so individual runes don't need to hardcode them.

### Build environment identity

`toolchain::build_env_id()` must distinguish musl from gnu environments:

- For `-musl` targets: hash `clang --version`, `lld --version`, and `musl libc` version string.
- For `-gnu` targets: continue using the existing logic (host `cc --version`, `ld --version`).

This ensures musl and gnu builds produce different store hashes even for the same source.

### macOS and FreeBSD

On macOS and FreeBSD, core packages build with the self-built Clang but link against the **host libc** (not musl):

```bash
CC=clang
CXX=clang++
CFLAGS="-target x86_64-apple-darwin"
LDFLAGS="-target x86_64-apple-darwin -fuse-ld=lld"
```

macOS binaries remain dynamically linked against `libSystem.dylib`. FreeBSD binaries remain dynamically linked against `libc.so.7`.

The benefit is toolchain consistency (same Clang version, same flags, same diagnostics) even though the libc differs.

---

## Section 7: FHS Compat Layer (Reduced Scope)

### What still needs FHS

The FHS compat layer (TODO #1) is still required for:

1. **User packages from third-party tomes** that are dynamically linked against glibc.
2. **Prebuilt binaries** downloaded from the internet (e.g., official Go binaries, Node.js binaries).
3. **Core packages on Linux-gnu targets** if a user explicitly opts into glibc builds.

### What no longer needs FHS

Core packages on Linux-musl targets produce fully static binaries. They:
- Have no dynamic linker dependency (`ld-linux.so`)
- Have no glibc version dependency
- Run on any Linux kernel >= 2.6.39 (musl's minimum)

This eliminates the foreign binary problem for core packages entirely.

### FHS implementation changes

The FHS compat layer only needs to handle packages that declare `deps.features` (dynamically-linked foreign binaries). The baseline FHS tree package (`tome-core/runes/fhs-compat.rn`) is simplified:

- No longer needs to provide glibc (core packages don't use it)
- Only needs to provide core libraries for user packages: `libz.so`, `libssl.so`, `libcrypto.so`, etc.
- The `grm fhs-run` command and `unshare` + bind mount mechanism remain unchanged.

---

## Section 8: Why GNU Make Is The Sole Exception

A fully non-GNU core is possible in theory but impractical for a package manager that builds arbitrary software:

- ~60-70% of C/C++ projects use **autotools**, which generates Makefiles with GNU Make extensions (`$(foreach)`, `$(call)`, `$(eval)`, `%` pattern rules).
- BSD make (`bmake`) has incompatible syntax.
- Even Alpine Linux — the most successful non-GNU, musl-based distro — includes GNU Make in its build environment.

**Can core build without GNU Make?**

| Package | Build System | Needs GNU Make? |
|---------|-------------|-----------------|
| `musl` | Custom Makefile | Likely yes |
| `compiler-rt` | CMake + Ninja | No |
| `llvm` | CMake + Ninja | No |
| `clang` | CMake + Ninja | No |
| `busybox` | Kconfig + Makefile | **Yes** |

Even for core, some packages need it. GNU Make stays as the single exception.

---

## Section 9: Signing and Security

The existing signing model applies unchanged:

- Toolchain packages (`musl`, `llvm`, `clang`, `make`, `busybox`) are part of `tome-core` and are covered by the tome's signed rune manifest.
- The bootstrapped toolchain binaries are store-hash-addressed and verified by archive signatures.
- Build environment identity includes the toolchain version, so toolchain upgrades produce new store hashes and trigger rebuilds.

No changes to `capture_signer`, `verify_runes_manifest`, or the TOFU model.

---

## Section 10: Files to Modify

| File | Change |
|------|--------|
| `src/model.rs` | Add `linux-x86_64-musl` and `linux-aarch64-musl` to supported targets. Update `Deps` to support platform-conditional bracket syntax. Update `parse_deps`. |
| `src/solve.rs` | Update dependency resolver to filter platform-conditional deps by target triple. |
| `src/build.rs` | Inject toolchain flags (`CC`, `CXX`, `CFLAGS`, `LDFLAGS`) based on target triple. |
| `src/toolchain.rs` | Update `build_env_id()` to distinguish musl vs gnu; detect Clang/LLVM versions. |
| `src/profile.rs` | No changes (static binaries hard-link normally). |
| `tome-core/runes/linux-headers.rn` | New: Linux kernel headers package. |
| `tome-core/runes/musl.rn` | New: musl libc build rune. |
| `tome-core/runes/compiler-rt.rn` | New: compiler-rt build rune (or bundled with llvm). |
| `tome-core/runes/llvm.rn` | New: LLVM build rune. |
| `tome-core/runes/clang.rn` | New: Clang build rune. |
| `tome-core/runes/make.rn` | New: GNU Make build rune. |
| `tome-core/runes/busybox.rn` | New: Busybox build rune. |
| `tome-core/runes/fhs-compat.rn` | Simplified: remove glibc, keep other libs. |
| `tome-core/tome.rn` | Add new runes to `packages` index. Update `deps` to use bracket syntax where needed. |

---

## Testing Requirements

- Bootstrap test: `grm tome build --all` on a fresh Linux host builds toolchain → all core packages.
- Static binary test: `ldd` on a core binary returns "not a dynamic executable" on Linux.
- Cross-target test: `grm build <package> --target linux-x86_64-musl` produces a musl static binary.
- macOS/FreeBSD test: Core packages still build and link dynamically against host libc.
- Platform dep test: `linux-headers[linux]` is resolved on Linux, omitted on macOS/FreeBSD.
- FHS regression: A user package with `deps.features` still runs correctly in the FHS tree.

## Open Questions

None. Design is complete.
