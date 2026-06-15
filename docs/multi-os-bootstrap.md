# Multi-OS Bootstrap Reference

How Grimoire bootstraps its managed toolchain and core tome across operating systems, and what
actually differs between them. Written after taking `linux-aarch64-musl` and `macos-aarch64-darwin`
green; the FreeBSD column is **anticipated** (not yet built) and is the working hypothesis for that
port.

## Thesis: the OS is the axis, not the CPU

Porting across CPU architectures on one OS is nearly free — the runes already parameterize the arch
through the target triple (`RUST_TRIPLES`, `LLVM_TARGETS_TO_BUILD="X86;AArch64"`, the
`<os>-<arch>-<abi>` → clang-triple mapping in `clang_musl_triple`). Two arches on one OS share ~90%
of the work.

Porting across operating systems is where the work is. The same aarch64 object behaves completely
differently on Linux-musl vs macOS because the **libc, object format, base toolchain, and loader**
differ. Arch only changes the *names* of the failures (`R_AARCH64_*` ↔ `R_X86_64_*`,
`__letf2`/`__getf2` for aarch64 128-bit `long double` ↔ x86's 80-bit helpers, outline-atomics), not
their *shape*.

## The four axes of discrepancy

### 1. libc provisioning + CRT/runtime

Who supplies libc, its startup objects (`crt1.o`/`crti.o`/`crtn.o`/`Scrt1.o`), `libc.a`/`libc.so`,
and the compiler runtime (soft-float helpers, unwinder)?

- **Linux/musl** — Grimoire builds musl itself (`musl.rn`) and packages the full chain. Nothing is
  assumed present on the host. The compiler runtime is LLVM's `compiler-rt` + `libunwind`, *not*
  gcc's `libgcc`/`libgcc_s` (which the musl world does not have). Two consequences that bit us:
  - musl's `libc.so` link needs the long-double soft-float builtins (`__letf2`/`__getf2`/`__addtf3`)
    pinned via `LIBCC=$(cc --rtlib=compiler-rt -print-libgcc-file-name)` — musl's own probe looks for
    `libgcc` and comes up empty.
  - rust's `unwind` crate links `-lgcc_s` by default on musl → `[target.<musl>].llvm-libunwind =
    "in-tree"` makes rust build LLVM's libunwind from source instead.
- **macOS** — the platform SDK supplies libc, the CRT, and the C++ stdlib. Grimoire builds none of
  it; it injects `SDKROOT` (see `build_env_for_target`) and leans on the SDK.
- **FreeBSD (anticipated)** — base system supplies libc + CRT in `/usr/lib`. Closer to the macOS
  "lean on the platform" model than to the musl "build it yourself" model: likely **no floor build**,
  just point at base.

### 2. Object format / ABI

ELF vs Mach-O. This is where the brutal PIC/TLS work lived.

- **Linux (ELF)** — rust's dynamic `rustc` links the managed LLVM + libc++ static archives into a
  **shared** `librustc_driver.so`. Non-PIC code cannot go in a `.so`:
  - data → `R_AARCH64_ABS64`, and module-local `thread_local`s → local-exec TLS `R_AARCH64_TLSLE`,
    both rejected with `-shared`.
  - **`LLVM_ENABLE_PIC=ON` is not enough** — it only enables PIC *capability* (lets shared libs
    build); it does **not** add `-fPIC` to static-library objects. `CMAKE_POSITION_INDEPENDENT_CODE=ON`
    does, which lowers TLS to `.so`-safe `R_AARCH64_TLSDESC` and data to GOT-relative. Set it in both
    `llvm.rn` and `libcxx.rn` (Linux branch). With PIC on, also disable every shared-lib/plugin target
    that would otherwise build and fail in the static-musl world (`BUILD_SHARED_LIBS`,
    `LLVM_BUILD_LLVM_DYLIB`, `LLVM_LINK_LLVM_DYLIB`, `LLVM_BUILD_LLVM_C_DYLIB`, `CLANG_LINK_CLANG_DYLIB`,
    `LLVM_ENABLE_PLUGINS`).
- **macOS (Mach-O)** — PIC by default, different TLS; **none** of the above applies. This is why the
  llvm.rn/libcxx.rn PIC/TLS changes are guarded to the non-Darwin branch.
- **FreeBSD (anticipated)** — **ELF, like Linux**. The entire PIC/TLS/`.so` story should *transfer*.
  This is the strongest reason FreeBSD is a cheap third corner: it inherits the hardest-won work.

### 3. Base toolchain availability

Do you build the compiler + C++ stdlib, or does the OS hand them to you?

- **Linux/musl** — build the whole floor: `musl`, `linux-headers`, `libcxx` (the only musl C++
  stdlib), `llvm`, `clang`, `toolchain-wrappers`. There is no host C++ stdlib for musl, hence
  `libcxx.rn` exists purely to fill that gap.
- **macOS** — SDK ships clang + libc++. `libcxx.rn` explicitly excludes Darwin from its `targets`.
- **FreeBSD (anticipated)** — base ships clang + libc++ + lld. Likely lean on base (skip `libcxx.rn`,
  maybe skip building `llvm`/`clang` for the *host-tools* role) — though Grimoire may still want to
  build its own managed llvm/clang for hermeticity. A real decision point for the port.

### 4. Loader + dynamic-linking conventions

- **Linux/musl** — `/lib/ld-musl-<arch>.so.1`. Grimoire does **not** use `/lib`: `musl.rn` builds the
  shared libc + loader **into the store prefix** (`--syslibdir=$prefix/lib`), and consumers point
  `PT_INTERP` at the in-store path (`-Wl,--dynamic-linker=<musl-store>/lib/ld-musl-<arch>.so.1`). The
  dynamic `rustc`/`cargo` use this; they run on the build host with no `/lib` dependency. The static
  `grm` and the static-PIE build helpers carry **no** `PT_INTERP` and must *not* be given one (a
  stamped interpreter on a static-PIE makes the kernel hand it to the loader → SIGSEGV).
- **macOS** — `dyld`; handled by the SDK/linker, nothing special.
- **FreeBSD (anticipated)** — `/libexec/ld-elf.so.1`, present in base. No in-store-loader gymnastics
  needed (unlike musl), since base provides the loader.

## Where each discrepancy lives in the code

| Mechanism | File | What it does |
|---|---|---|
| Per-target build env | `src/build/mod.rs` `build_env_for_target` | Branches on `is_musl_target` (inject the musl floor) and `macos-` (inject `SDKROOT`). The single funnel for OS-specific env. |
| musl compiler retarget | `musl_target_env_vars` | `--target`/`--sysroot`/`-isystem`/`-B` + `--rtlib=compiler-rt --unwindlib=none -static` on `CFLAGS`/`CXXFLAGS`/`LDFLAGS`. |
| musl C++ floor | `inject_libcxx_flags` | `-stdlib=libc++ -nostdinc++ -isystem <libcxx>/…` for every musl C++ build except libcxx's own. |
| Discovery vars | `install::build_dep_env_vars` | `CPATH`/`LIBRARY_PATH`/`CMAKE_PREFIX_PATH`/`PKG_CONFIG_PATH` for declared deps (and the musl/linux-headers floor). |
| Toolchain boundary | `core_compiler_boundary_available` | Flips from host tools (bootstrap) to the managed clang once `toolchain-wrappers` is installed. |
| Per-OS rune branches | `llvm.rn`, `rust.rn`, `grimoire.rn` | `if ($darwin_arch \| is-empty)` (llvm), `if $is_musl` (rust/grimoire). `libcxx.rn` is Linux-only via `targets`. |

A recurring lesson: rust's bootstrap reads the **generic** `CFLAGS`/`CPATH` (not the per-triple
`CFLAGS_<triple>`) when it builds its internal LLVM and runs cc-rs for build-host helpers. On the
musl cross-build (`build=<gnu>`, `host=target=<musl>`) the floor's musl flags leaked into the gnu
build-host compiles; `rust.rn` therefore **re-scopes** the floor flags onto the musl target triple
(`CFLAGS_<musl-triple>` …) and **clears** the generic vars so the gnu helpers fall back to clean host
flags. A new-OS port that cross-compiles will hit the same generic-vs-per-triple split.

## Porting to a new OS — checklist

Worked example target: FreeBSD (anticipated answers in parentheses).

1. **Triples.** Add the `<os>-<arch>-<abi>` → rust/clang triple rows to `RUST_TRIPLES`
   (`rust.rn`, `rust-stage0.rn`), `clang_musl_triple`/host-triple maps, and any `targets:` lists.
   (`x86_64-unknown-freebsd` is already half-wired.)
2. **Stage0.** Confirm an upstream rust stage0 exists and runs natively on the host
   (`rust-stage0.rn`). (FreeBSD: yes, `x86_64-unknown-freebsd`.)
3. **libc/CRT.** Decide: build a floor (musl model) or lean on base (macOS model). Wire
   `build_env_for_target` accordingly. (FreeBSD: lean on base `/usr/lib`; probably no floor.)
4. **C++ stdlib.** Floor `libcxx` or platform-provided? (FreeBSD: base libc++ → likely skip
   `libcxx.rn`.)
5. **Object format.** ELF → the PIC/TLS `.so` work applies (reuse the Linux branch's
   `CMAKE_POSITION_INDEPENDENT_CODE` + dylib/plugin-off + the wrapper). Mach-O → skip it. (FreeBSD:
   ELF → reuse.)
6. **Loader.** In-store (musl) or system (`/libexec/ld-elf.so.1`)? Set/omit
   `-Wl,--dynamic-linker` in the linker wrapper accordingly. (FreeBSD: system loader; likely no
   wrapper `--dynamic-linker` at all, and the dynamic `rustc` just uses base's loader.)
7. **crt-static default.** Check `rustc --print cfg --target <triple>` for
   `target_feature="crt-static"`. musl defaults static (so `grm` is static with no extra flags);
   most others default dynamic. This decides whether `grm` ships static or dynamic and whether you
   need a loader for it. (FreeBSD: dynamic by default → `grm` would be a normal dynamic ELF using
   base's loader, which is fine.)
8. **Build host tooling.** Anything the build shells out to that the managed/floor env shadows
   (`/usr/bin/cc` for kbuild-style host tools, host `cc` for cross build-host helpers). Mostly a
   Linux-cross concern; a host==target build avoids it.

## What the CPU arch *does* change

Not nothing — just analogous, not structural:
- Relocation/TLS names (`R_AARCH64_*` ↔ `R_X86_64_*`); the *class* of fix (PIC, TLS model) is the
  same.
- compiler-rt soft-float helpers for the arch's `long double` (aarch64 128-bit `__letf2`/`__getf2`
  vs x86 80-bit).
- Arch-specific codegen quirks (aarch64 outline-atomics, `config.guess` fossils).
- `LLVM_TARGETS_TO_BUILD` / `LLVM_TARGET_ARCH` strings.

All of these are already parameterized by the triple, which is why adding `x86_64` alongside
`aarch64` on an existing OS is a matter of filling in table rows, not new logic.
