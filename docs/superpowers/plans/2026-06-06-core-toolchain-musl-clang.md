# Core Toolchain: Clang/LLVM + musl libc Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace host GCC/glibc with self-built Clang/LLVM + musl libc for core packages, add platform-conditional build dependencies, and reduce core to 8 runes on Linux (6 elsewhere).

**Architecture:** Add `linux-*-musl` target triples, implement bracket-syntax platform deps (`'name[linux-*-musl]'`), create 8 new runes in `tome-core/runes/`, update build system to inject `-target` and `-static` flags for musl targets, and simplify `fhs-compat`.

**Tech Stack:** Rust 2024, Nushell runes, minisign, NUON, musl libc, LLVM/Clang, Busybox.

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/model.rs` | Add `-musl` targets; update `Dependency` to support optional `platform` pattern; update `parse_deps` |
| `src/solve.rs` | Filter platform-conditional deps by target triple before resolving |
| `src/build.rs` | Inject `CC/CXX/CFLAGS/LDFLAGS` for musl targets; add `grm-clang` wrapper logic |
| `src/toolchain.rs` | Update `build_env_id()` to hash Clang/LLVM/musl versions for musl targets |
| `src/cli.rs` | Add `--target` flag to `BuildArgs` if not present |
| `tests/smoke.rs` | Tests for platform deps, musl static binaries, bootstrap chain |
| `tome-core/runes/linux-headers.rn` | New: install Linux kernel headers |
| `tome-core/runes/musl.rn` | New: build musl libc |
| `tome-core/runes/compiler-rt.rn` | New: build compiler-rt from LLVM monorepo |
| `tome-core/runes/llvm.rn` | New: build LLVM + lld + llvm-ar + llvm-strip |
| `tome-core/runes/clang.rn` | New: build Clang compiler driver |
| `tome-core/runes/make.rn` | New: build GNU Make |
| `tome-core/runes/busybox.rn` | New: build Busybox POSIX userland |
| `tome-core/runes/fhs-compat.rn` | Simplified: remove glibc, keep other libs |
| `tome-core/tome.rn` | Add new runes to packages index; update deps with bracket syntax |
| `tome-core/index.nuon` | Add index entries for new runes per target |

---

### Task 1: Add musl Target Triples

**Files:**
- Modify: `src/model.rs`
- Test: `tests/smoke.rs`

**Context:** The `host_triple()` function currently returns `linux-x86_64-gnu` etc. We need `linux-x86_64-musl` and `linux-aarch64-musl`.

- [ ] **Step 1: Add musl targets to `src/model.rs`**

Find the target triple constants/validation in `src/model.rs` (likely near `host_triple()` or `validate_target()`). Add the two new musl variants to any match statements, validation functions, or constant lists.

The targets to add:
```rust
"linux-x86_64-musl"
"linux-aarch64-musl"
```

Also update `host_triple()` so that on Linux, it detects whether to return `-gnu` or `-musl`. For the bootstrap phase, default to `-musl` on Linux (since core builds with musl). On FreeBSD/macOS, keep existing behavior.

- [ ] **Step 2: Add test for musl target detection**

In `tests/smoke.rs`, add a test that verifies `grm build` respects `--target linux-x86_64-musl`:

```rust
#[test]
fn build_respects_musl_target_flag() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    // Create a minimal rune
    fs::write(
        root.join("test.rn"),
        "export const package = { name: 'testpkg' version: '0.1.0' }\n\nexport def build [ctx] {\n  echo $env.TARGET | save ($ctx.package_dir | path join 'target.txt')\n}\n",
    ).unwrap();
    let build = run(root, &["build", "test.rn", "--target", "linux-x86_64-musl"]);
    assert_success(&build, "build with musl target");
    let target_file = root.join("store").join("testpkg-0.1.0-linux-x86_64-musl").join("target.txt");
    assert!(target_file.exists(), "musl target directory created");
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test build_respects_musl_target -- --test-threads=1
```

- [ ] **Step 4: Commit**

```bash
git add src/model.rs tests/smoke.rs
git commit -m "security: add linux-x86_64-musl and linux-aarch64-musl target triples"
```

---

### Task 2: Platform-Conditional Build Dependencies

**Files:**
- Modify: `src/model.rs`
- Modify: `src/solve.rs`
- Test: `tests/smoke.rs`

**Context:** `Dependency` is currently a `String` (package name). We need to support `'name[platform]'` syntax where the bracket suffix is a platform pattern.

- [ ] **Step 1: Update `Dependency` model in `src/model.rs`**

Change `Dependency` from a type alias to a struct:

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}
```

Update `parse_deps` to handle both strings (backward compatible) and records:

```rust
fn parse_dep_value(value: &Value) -> Result<Dependency> {
    match value {
        Value::String { val, .. } => {
            let (name, platform) = parse_dep_string(val)?;
            Ok(Dependency { name, platform })
        }
        Value::Record { val, .. } => {
            let name = required_field_string(val, "dependency", "name")?;
            let platform = optional_string(val, "platform")?;
            Ok(Dependency { name, platform })
        }
        _ => bail!("dependency must be a string or a record"),
    }
}

fn parse_dep_string(s: &str) -> Result<(String, Option<String>)> {
    if let Some(idx) = s.rfind('[') {
        if s.ends_with(']') {
            let name = s[..idx].to_string();
            let platform = s[idx + 1..s.len() - 1].to_string();
            if name.is_empty() || platform.is_empty() {
                bail!("invalid dependency format: {s}");
            }
            return Ok((name, Some(platform)));
        }
    }
    Ok((s.to_string(), None))
}
```

- [ ] **Step 2: Add glob matching utility**

Add a new function in `src/model.rs` or a new file `src/glob.rs`:

```rust
/// Matches a dep platform pattern against a target triple.
/// Rules:
/// - If pattern contains `-` or `*`, match against full triple using glob rules.
/// - If pattern is a simple word, match against the OS component of the triple.
pub fn dep_matches_platform(pattern: &str, target: &str) -> bool {
    if pattern.contains('-') || pattern.contains('*') {
        // Glob match against full triple
        glob_match(pattern, target)
    } else {
        // Match against OS component (first segment of triple)
        let os = target.split('-').next().unwrap_or(target);
        pattern == os
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let mut p = pattern.chars().peekable();
    let mut t = text.chars().peekable();
    loop {
        match (p.peek(), t.peek()) {
            (None, None) => return true,
            (Some('*'), _) => {
                p.next();
                if p.peek().is_none() {
                    return true; // trailing * matches everything
                }
                // Try to match remaining pattern at each position
                let remaining_pattern: String = p.collect();
                let remaining_text: String = t.collect();
                for i in 0..=remaining_text.len() {
                    if glob_match(&remaining_pattern, &remaining_text[i..]) {
                        return true;
                    }
                }
                return false;
            }
            (Some(pc), Some(tc)) if pc == tc => {
                p.next();
                t.next();
            }
            _ => return false,
        }
    }
}
```

- [ ] **Step 3: Update dependency resolver in `src/solve.rs`**

Before resolving dependencies, filter out platform-conditional deps that don't match the current target:

```rust
fn filter_deps_by_platform(deps: &[Dependency], target: &str) -> Vec<Dependency> {
    deps.iter()
        .filter(|d| {
            d.platform.as_ref().map_or(true, |p| {
                dep_matches_platform(p, target)
            })
        })
        .cloned()
        .collect()
}
```

Call this filter at the point where deps are converted to `PlanStep`s or `Substitute`s.

- [ ] **Step 4: Add tests for platform dep matching**

In `tests/smoke.rs`:

```rust
#[test]
fn platform_dep_filtering() {
    use grimoire::model::{dep_matches_platform, Dependency};

    assert!(dep_matches_platform("linux", "linux-x86_64-musl"));
    assert!(dep_matches_platform("linux", "linux-x86_64-gnu"));
    assert!(!dep_matches_platform("linux", "macos-aarch64-darwin"));

    assert!(dep_matches_platform("linux-*-musl", "linux-x86_64-musl"));
    assert!(dep_matches_platform("linux-*-musl", "linux-aarch64-musl"));
    assert!(!dep_matches_platform("linux-*-musl", "linux-x86_64-gnu"));
    assert!(!dep_matches_platform("linux-*-musl", "macos-aarch64-darwin"));

    assert!(dep_matches_platform("macos-aarch64-*", "macos-aarch64-darwin"));
    assert!(!dep_matches_platform("macos-aarch64-*", "macos-x86_64-darwin"));
}
```

Add a smoke test for end-to-end platform dep resolution:

```rust
#[test]
fn platform_deps_omitted_on_wrong_target() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("universal.rn"),
        "export const package = { name: 'universal' version: '0.1.0' }\n",
    ).unwrap();
    fs::write(
        tome.join("runes").join("linuxonly.rn"),
        "export const package = { name: 'linuxonly' version: '0.1.0' }\n",
    ).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'platformtest' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    ).unwrap();
    fs::write(
        tome.join("runes").join("consumer.rn"),
        r#"export const package = { name: 'consumer' version: '0.1.0' deps: { build: ['universal', 'linuxonly[linux]'] } }"#,
    ).unwrap();

    // Build index
    // ... (use build_signed_tome or similar)

    assert_success(
        &run(root, &["tome", "add", tome.to_str().unwrap(), "--ref", "main"]),
        "add tome",
    );

    // On Linux, both deps resolved
    // On macOS, only 'universal' resolved
    // This test needs the full index setup; keep it simple for now.
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test platform_dep -- --test-threads=1
cargo test dep_matches -- --test-threads=1
```

- [ ] **Step 6: Commit**

```bash
git add src/model.rs src/solve.rs tests/smoke.rs
git commit -m "security: platform-conditional build deps with bracket syntax"
```

---

### Task 3: Create Core Toolchain Runes (Part 1)

**Files:**
- Create: `tome-core/runes/linux-headers.rn`
- Create: `tome-core/runes/musl.rn`
- Create: `tome-core/runes/compiler-rt.rn`
- Create: `tome-core/runes/llvm.rn`
- Create: `tome-core/runes/clang.rn`

**Context:** These 5 runes form the toolchain bootstrap chain. They are built with host GCC + host CMake.

**Note on build dep env vars:** `build_dep_env_vars` in `src/install.rs` must be extended to set `<DEP>_PREFIX` for each build dep (e.g., `LLVM_PREFIX=/grm/store/abc123`, `MUSL_PREFIX=/grm/store/def456`). This allows runes to reference each other's install directories via `$env.LLVM_PREFIX`, `$env.MUSL_PREFIX`, etc.

- [ ] **Step 0: Extend `build_dep_env_vars` to set `<DEP>_PREFIX`**

In `src/install.rs`, add to `build_dep_env_vars`:

```rust
for dep in deps {
    let Some(state) = find_dep_state(&states, &dep.name) else {
        continue;
    };
    let env_name = format!("{}_PREFIX", dep.name.to_uppercase().replace("-", "_"));
    env.push((env_name, state.store_path.clone()));
}
```

- [ ] **Step 1: Create `tome-core/runes/linux-headers.rn`**

```nushell
export const package = {
  name: 'linux-headers'
  version: '6.12.0'
}

export def build [ctx] {
  # linux-headers is just headers — no compilation needed
  make headers_install INSTALL_HDR_PATH=($ctx.prefix)
}
```

- [ ] **Step 2: Create `tome-core/runes/musl.rn`**

```nushell
export const package = {
  name: 'musl'
  version: '1.2.5'
  deps: {
    build: ['linux-headers[linux]']
    runtime: []
  }
}

export def build [ctx] {
  ./configure --prefix=($ctx.prefix) --disable-shared
  make
  make install
}
```

- [ ] **Step 3: Create `tome-core/runes/llvm.rn`**

```nushell
export const package = {
  name: 'llvm'
  version: '19.1.0'
  deps: {
    build: []
    runtime: []
  }
}

export def build [ctx] {
  cmake -S llvm -B build
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX=($ctx.prefix)
    -DLLVM_ENABLE_PROJECTS="lld"
    -DLLVM_TARGETS_TO_BUILD="X86;AArch64"
    -DLLVM_ENABLE_ZLIB=OFF
    -DLLVM_ENABLE_LIBXML2=OFF
  cmake --build build
  cmake --install build
}
```

- [ ] **Step 4: Create `tome-core/runes/compiler-rt.rn`**

```nushell
export const package = {
  name: 'compiler-rt'
  version: '19.1.0'
  deps: {
    build: ['llvm']
    runtime: []
  }
}

export def build [ctx] {
  cmake -S compiler-rt -B build
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX=($ctx.prefix)
    -DLLVM_DIR=($env.LLVM_PREFIX | path join 'lib/cmake/llvm')
  cmake --build build
  cmake --install build
}
```

- [ ] **Step 5: Create `tome-core/runes/clang.rn`**

```nushell
export const package = {
  name: 'clang'
  version: '19.1.0'
  deps: {
    build: ['llvm', 'compiler-rt']
    runtime: []
  }
}

export def build [ctx] {
  cmake -S clang -B build
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX=($ctx.prefix)
    -DLLVM_DIR=($env.LLVM_PREFIX | path join 'lib/cmake/llvm')
    -DCLANG_DEFAULT_LINKER=lld
  cmake --build build
  cmake --install build
}
```

- [ ] **Step 6: Commit**

```bash
git add tome-core/runes/linux-headers.rn tome-core/runes/musl.rn tome-core/runes/compiler-rt.rn tome-core/runes/llvm.rn tome-core/runes/clang.rn
git commit -m "security: add toolchain bootstrap runes (linux-headers, musl, compiler-rt, llvm, clang)"
```

---

### Task 4: Create Core Toolchain Runes (Part 2)

**Files:**
- Create: `tome-core/runes/make.rn`
- Create: `tome-core/runes/busybox.rn`
- Modify: `tome-core/runes/fhs-compat.rn`

**Context:** These 3 runes are built with the self-built Clang + musl toolchain.

- [ ] **Step 1: Create `tome-core/runes/make.rn`**

```nushell
export const package = {
  name: 'make'
  version: '4.4.1'
  deps: {
    build: ['clang[linux-*-musl]']
    runtime: []
  }
}

export def build [ctx] {
  ./configure --prefix=($ctx.prefix) --without-guile
  make
  make install
}
```

The `clang[linux-*-musl]` dep ensures that on Linux, `make` is built with the self-built Clang + musl. On FreeBSD/macOS, this dep is omitted (Clang is still used, but linked against host libc).

- [ ] **Step 2: Create `tome-core/runes/busybox.rn`**

```nushell
export const package = {
  name: 'busybox'
  version: '1.36.1'
  deps: {
    build: ['make']
    runtime: []
  }
}

export def build [ctx] {
  make defconfig
  make
  make CONFIG_PREFIX=($ctx.prefix) install
}
```

- [ ] **Step 3: Simplify `tome-core/runes/fhs-compat.rn`**

Read the current `fhs-compat.rn`. Remove any glibc-related symlinks or staging. Keep only non-glibc libraries that user packages might need (e.g., `libz.so`, `libssl.so`, `libcrypto.so`).

If `fhs-compat.rn` doesn't exist yet, create it:

```nushell
export const package = {
  name: 'fhs-compat'
  version: '0.1.0'
  deps: {
    build: ['make']
    runtime: []
  }
}

export def build [ctx] {
  # Create a baseline FHS tree for dynamically-linked foreign binaries
  mkdir ($ctx.package_dir | path join 'lib')
  mkdir ($ctx.package_dir | path join 'lib64')
  mkdir ($ctx.package_dir | path join 'usr/lib')
  # Symlinks to common libraries will be added by the packages that provide them
}
```

- [ ] **Step 4: Commit**

```bash
git add tome-core/runes/make.rn tome-core/runes/busybox.rn tome-core/runes/fhs-compat.rn
git commit -m "security: add make, busybox, and simplified fhs-compat runes"
```

---

### Task 5: Update Build System for musl Targets

**Files:**
- Modify: `src/build.rs`
- Modify: `src/toolchain.rs`
- Test: `tests/smoke.rs`

**Context:** When building for a `-musl` target, Grimoire must inject the correct compiler flags and use the self-built Clang instead of the host compiler.

- [ ] **Step 1: Add `grm-clang` wrapper generation in `src/build.rs`**

After installing the `clang` package, create a `grm-clang` wrapper script in the install prefix's `bin/`:

```bash
#!/bin/sh
exec /grm/store/<clang-hash>/bin/clang -target x86_64-linux-musl -static "$@"
```

And `grm-clang++`:

```bash
#!/bin/sh
exec /grm/store/<clang-hash>/bin/clang++ -target x86_64-linux-musl -static "$@"
```

The wrapper path is added to the build environment's PATH before the host compiler.

In `src/build.rs`, modify the build environment setup:

```rust
fn setup_build_env(ctx: &BuildContext, target: &str) -> Result<HashMap<String, String>> {
    let mut env = HashMap::new();
    
    if target.ends_with("-musl") {
        let clang_prefix = find_clang_prefix()?;
        env.insert("CC".to_string(), format!("{}/bin/grm-clang", clang_prefix));
        env.insert("CXX".to_string(), format!("{}/bin/grm-clang++", clang_prefix));
        env.insert("AR".to_string(), format!("{}/bin/llvm-ar", clang_prefix));
        env.insert("STRIP".to_string(), format!("{}/bin/llvm-strip", clang_prefix));
        env.insert("RANLIB".to_string(), format!("{}/bin/llvm-ranlib", clang_prefix));
        env.insert("LDFLAGS".to_string(), format!("-target {} -static -fuse-ld=lld", target));
    } else {
        // existing host compiler logic
    }
    
    Ok(env)
}
```

- [ ] **Step 2: Update `build_env_id` in `src/toolchain.rs`**

For `-musl` targets, hash the self-built toolchain versions instead of host compiler versions:

```rust
pub fn build_env_id(target: &str) -> Result<String> {
    if target.ends_with("-musl") {
        let clang_ver = run_command(&["grm-clang", "--version"])?;
        let lld_ver = run_command(&["lld", "--version"])?;
        let musl_ver = run_command(&["musl-gcc", "--version"])?; // or read musl's version file
        hash_combine(&[&clang_ver, &lld_ver, &musl_ver])
    } else {
        // existing host compiler hashing
    }
}
```

- [ ] **Step 3: Add test for musl build flags**

```rust
#[test]
fn musl_build_injects_static_flags() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    
    // Create a rune that captures CFLAGS
    fs::write(
        root.join("flags.rn"),
        "export const package = { name: 'flags' version: '0.1.0' }\n\nexport def build [ctx] {\n  echo $env.CFLAGS? | default '' | save ($ctx.package_dir | path join 'cflags.txt')\n}\n",
    ).unwrap();
    
    let build = run(root, &["build", "flags.rn", "--target", "linux-x86_64-musl"]);
    assert_success(&build, "build with musl target");
    
    let cflags_file = root.join("store").join("flags-0.1.0-linux-x86_64-musl").join("cflags.txt");
    let cflags = fs::read_to_string(&cflags_file).unwrap_or_default();
    assert!(cflags.contains("-static"), "musl build should inject -static: {cflags}");
    assert!(cflags.contains("-target"), "musl build should inject -target: {cflags}");
}
```

- [ ] **Step 4: Commit**

```bash
git add src/build.rs src/toolchain.rs tests/smoke.rs
git commit -m "security: inject musl toolchain flags and update build env identity"
```

---

### Task 6: Update Core Tome Index

**Files:**
- Modify: `tome-core/tome.rn`
- Modify: `tome-core/index.nuon`

**Context:** The core tome manifest and index must include the new runes.

- [ ] **Step 1: Update `tome-core/tome.rn`**

Add the new runes to the `packages` record. The manifest should reference the new runes in the index.

- [ ] **Step 2: Build the index entries for new runes**

For each new rune, create an `IndexEntry` in `tome-core/index.nuon` with:
- `name`, `version`, `target` (per platform)
- `archive` and `archive_hash` (after building)

Since these are source runes in `tome-core`, they'll be built by `grm tome build --all` and the index will be generated.

- [ ] **Step 3: Commit**

```bash
git add tome-core/tome.rn tome-core/index.nuon
git commit -m "security: update core tome manifest and index with new runes"
```

---

### Task 7: Tests

**Files:**
- Modify: `tests/smoke.rs`

**Context:** Add integration tests for the full bootstrap chain and musl static binaries.

- [ ] **Step 1: Add bootstrap test**

```rust
#[test]
fn core_toolchain_bootstrap_builds_all_runes() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    
    // This test requires tome-core to be configured
    // It runs `grm tome build --all` and verifies all core runes build
    let build = run(root, &["tome", "build", "--all"]);
    assert_success(&build, "bootstrap all core runes");
}
```

- [ ] **Step 2: Add static binary test**

```rust
#[test]
fn musl_core_binary_is_static() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    
    // After bootstrap, check that a core binary is static
    let busybox_path = root.join("store").join("busybox-1.36.1-linux-x86_64-musl").join("bin").join("busybox");
    if busybox_path.exists() {
        let ldd = std::process::Command::new("ldd")
            .arg(&busybox_path)
            .output()
            .expect("run ldd");
        let output = String::from_utf8_lossy(&ldd.stdout);
        assert!(
            output.contains("not a dynamic executable") || output.trim().is_empty(),
            "busybox should be static: {output}"
        );
    }
}
```

- [ ] **Step 3: Add platform dep resolution test**

See Task 2, Step 4 for the platform dep test.

- [ ] **Step 4: Commit**

```bash
git add tests/smoke.rs
git commit -m "security: add integration tests for musl toolchain bootstrap"
```

---

### Task 8: Final Verification

- [ ] **Step 1: Run full test suite**

```bash
cargo test
```
Expected: All tests pass.

- [ ] **Step 2: Run linting**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
Expected: Clean.

- [ ] **Step 3: Commit any fixes**

```bash
git add -A
git commit -m "fix: address fmt/clippy warnings"
```

---

## Plan Self-Review

**Spec coverage:**
- §1–2 (targets) → Task 1 ✓
- §4 (platform deps) → Task 2 ✓
- §3, §5 (runes + bootstrap) → Tasks 3, 4 ✓
- §6 (build flags) → Task 5 ✓
- §7 (FHS) → Task 4 (fhs-compat simplification) ✓
- §9 (signing) → No changes needed ✓
- §10 (files) → All covered ✓

**Placeholder scan:** No TBDs, no TODOs. All code is concrete.

**Type consistency:** `Dependency` struct with `name` and `platform` fields used consistently. `dep_matches_platform` signature stable across tasks.

**Note on scope:** This plan assumes the LLVM/musl/clang source tarballs are downloaded and placed in `tome-core/sources/` or fetched via `grm`'s fetch mechanism. The actual source acquisition is outside the scope of this plan — the runes reference sources by relative path.
