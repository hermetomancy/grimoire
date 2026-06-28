//! Single in-memory authority for installed package state.

use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    model::{PackageState, parse_version_relaxed},
    nu::nuon_io,
    util::{output::status, paths},
};

/// The installed package world: loaded once per command, mutated in memory, and committed to disk
/// at a single transaction boundary. Replaces the scattered `installed_states()` / `linked_set()`
/// disk scans.
pub struct InstalledWorld {
    root: PathBuf,
    states: BTreeMap<String, PackageState>,
    dirty: HashSet<String>,
    deleted: HashSet<String>,
    linked_cache: Option<HashSet<String>>,
}

impl Default for InstalledWorld {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            states: BTreeMap::new(),
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            linked_cache: None,
        }
    }
}

impl InstalledWorld {
    /// Load from `{root}/state/packages/*.nuon`. Returns an empty world if the directory does not
    /// exist. Replaces the old `installed_states()`.
    /// Load from the current installation root (`paths::install_root()`).
    pub fn load_default() -> Result<Self> {
        Self::load(paths::install_root()?)
    }

    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let state_dir = root.join("state").join("packages");
        let mut states = BTreeMap::new();
        if state_dir.exists() {
            for entry in fs::read_dir(&state_dir)
                .with_context(|| format!("read state dir {}", state_dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
                    continue;
                }
                let state = PackageState::from_value(nuon_io::read_nuon(&path)?)
                    .with_context(|| format!("read package state {}", path.display()))?;
                states.insert(state.name.clone(), state);
            }
        }
        Ok(Self {
            root,
            states,
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            linked_cache: None,
        })
    }

    pub fn get(&self, name: &str) -> Option<&PackageState> {
        self.states.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PackageState> {
        self.states.values()
    }

    pub(crate) fn states(&self) -> &BTreeMap<String, PackageState> {
        &self.states
    }

    pub fn contains(&self, name: &str) -> bool {
        self.states.contains_key(name)
    }

    pub fn insert(&mut self, state: PackageState) {
        let name = state.name.clone();
        self.states.insert(name.clone(), state);
        self.dirty.insert(name.clone());
        self.deleted.remove(&name);
        self.linked_cache = None;
    }

    pub fn remove(&mut self, name: &str) -> Option<PackageState> {
        if let Some(state) = self.states.remove(name) {
            self.dirty.remove(name);
            self.deleted.insert(name.to_owned());
            self.linked_cache = None;
            Some(state)
        } else {
            None
        }
    }

    pub fn update(&mut self, name: &str, f: impl FnOnce(&mut PackageState)) {
        if let Some(state) = self.states.get_mut(name) {
            f(state);
            self.dirty.insert(name.to_owned());
            self.linked_cache = None;
        }
    }

    /// Resolve a dependency string to a state by exact name, bin, or capability.
    pub fn resolve_dep(&self, dep: &str) -> Option<&PackageState> {
        if let Some(state) = self.states.get(dep) {
            return Some(state);
        }
        self.states
            .values()
            .find(|state| state.bins.contains_key(dep) || state.provides.iter().any(|p| p == dep))
    }

    /// Compute the linked set: requested/held roots plus transitive runtime deps.
    /// Lazy and cached; invalidates on mutation. Test-only: production callers use
    /// [`linked_immut`].
    #[cfg(test)]
    pub fn linked(&mut self) -> &HashSet<String> {
        if self.linked_cache.is_none() {
            self.linked_cache = Some(self.compute_linked());
        }
        self.linked_cache.as_ref().unwrap()
    }

    /// Non-caching variant for immutable contexts.
    pub fn linked_immut(&self) -> HashSet<String> {
        self.compute_linked()
    }

    fn compute_linked(&self) -> HashSet<String> {
        let mut linked: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<&PackageState> = self
            .states
            .values()
            .filter(|state| state.requested || state.held)
            .collect();
        while let Some(state) = queue.pop_front() {
            if !linked.insert(state.name.clone()) {
                continue;
            }
            for dep in &state.runtime_deps {
                if let Some(dep_state) = self.resolve_dep(dep)
                    && !linked.contains(&dep_state.name)
                {
                    queue.push_back(dep_state);
                }
            }
        }
        linked
    }

    /// The packages whose bins are symlinked onto the active PATH. Seeded from every **requested**
    /// package, then walked through runtime deps — so a held dependency's bins link when something
    /// the user explicitly asked for needs it at runtime (e.g. `rust` reaching the `cc` wrapper).
    /// Build-only packages are PATH **barriers**: a build-only dependency (the managed toolchain —
    /// `clang`, `llvm`, pulled in via the wrappers) never links and is not traversed, so its bins
    /// stay in the store, reachable by store path, but off PATH. The deliberate exception is an
    /// explicit request: `grm install clang` puts clang's own bins on PATH (the user asked for it by
    /// name) without dragging its build-only closure along — a build-only package links only as a
    /// requested seed, never as a dependency. Contrast [`linked_immut`], the GC-root set, which
    /// keeps the whole runtime closure regardless.
    pub fn bin_linked_immut(&self) -> HashSet<String> {
        let mut linked: HashSet<String> = HashSet::new();
        // Every requested package seeds, build-only included: an explicit `grm install clang` should
        // put clang on PATH even though clang is build-only by default.
        let mut queue: VecDeque<&PackageState> = self
            .states
            .values()
            .filter(|state| state.requested)
            .collect();
        while let Some(state) = queue.pop_front() {
            if !linked.insert(state.name.clone()) {
                continue;
            }
            // A build-only package is a PATH barrier: its runtime closure is build machinery, so
            // never pursue its deps. Its own bins stay linked — it only reaches here as a requested
            // seed (a build-only dependency is skipped at the enqueue below, never linked).
            if state.build_only {
                continue;
            }
            for dep in &state.runtime_deps {
                if let Some(dep_state) = self.resolve_dep(dep)
                    && !dep_state.build_only
                    && !linked.contains(&dep_state.name)
                {
                    queue.push_back(dep_state);
                }
            }
        }
        linked
    }

    pub fn to_states(&self) -> Vec<PackageState> {
        self.states.values().cloned().collect()
    }

    /// Installed package names mapped to their concrete versions, for the solver. Replaces the old
    /// global `installed_versions()`.
    pub fn installed_versions(&self) -> std::collections::BTreeMap<String, semver::Version> {
        self.states
            .values()
            .filter_map(|state| {
                parse_version_relaxed(&state.version)
                    .ok()
                    .map(|version| (state.name.clone(), version))
            })
            .collect()
    }

    /// Like [`installed_versions`], but omitting packages whose installed bits have drifted from
    /// their current rune. Replaces the old global `installed_versions_current()`.
    pub fn installed_versions_current(
        &self,
    ) -> Result<std::collections::BTreeMap<String, semver::Version>> {
        let stale: std::collections::HashSet<String> = crate::store::closure::stale_installed(self)
            .into_iter()
            .map(|stale| stale.name)
            .collect();
        let mut versions = std::collections::BTreeMap::new();
        for state in self.states.values() {
            if stale.contains(&state.name) {
                status(&format!(
                    "{} {} drifted from its rune; not reusable",
                    state.name, state.version
                ));
                continue;
            }
            if let Ok(version) = parse_version_relaxed(&state.version) {
                versions.insert(state.name.clone(), version);
            }
        }
        Ok(versions)
    }

    /// Recorded store hashes keyed by package name. Replaces `closure::installed_resolved()` and
    /// similar ad-hoc scans.
    pub fn store_hashes(&self) -> std::collections::BTreeMap<String, String> {
        self.states
            .values()
            .map(|state| (state.name.clone(), state.store_hash.clone()))
            .collect()
    }

    /// Commit dirty and deleted states into the provided transaction. Only rewrites files that
    /// changed; removes files for deleted packages. Clears `dirty` and `deleted` on success.
    pub fn commit(&mut self, tx: &mut super::Transaction) -> Result<()> {
        let state_dir = self.root.join("state").join("packages");
        fs::create_dir_all(&state_dir)?;

        for name in &self.deleted {
            let path = state_dir.join(format!("{name}.nuon"));
            if path.exists() {
                let previous = Some(fs::read(&path)?);
                let path_for_closure = path.clone();
                tx.on_rollback(move || match &previous {
                    Some(bytes) => {
                        let _ = fs::write(&path_for_closure, bytes);
                    }
                    None => {
                        let _ = fs::remove_file(&path_for_closure);
                    }
                });
                fs::remove_file(&path)?;
            }
        }

        for name in &self.dirty {
            if let Some(state) = self.states.get(name) {
                let path = state_dir.join(format!("{name}.nuon"));
                let previous = if path.exists() {
                    Some(fs::read(&path)?)
                } else {
                    None
                };
                let path_for_closure = path.clone();
                tx.on_rollback(move || match &previous {
                    Some(bytes) => {
                        let _ = fs::write(&path_for_closure, bytes);
                    }
                    None => {
                        let _ = fs::remove_file(&path_for_closure);
                    }
                });
                nuon_io::write_nuon(&path, &state.to_value())?;
            }
        }

        self.dirty.clear();
        self.deleted.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn state(name: &str, runtime_deps: &[&str], requested: bool, held: bool) -> PackageState {
        PackageState {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            target: None,
            archive_hash: "0".repeat(64),
            store_hash: "deadbeef".to_owned(),
            store_path: format!("/grm/store/deadbeef-{name}-1.0.0"),
            bins: BTreeMap::new(),
            runtime_deps: runtime_deps.iter().map(|s| s.to_string()).collect(),
            build_deps: Vec::new(),
            source_hashes: BTreeMap::new(),
            held,
            requested,
            provides: Vec::new(),
            libs: Vec::new(),
            notes: Vec::new(),
            upstream_version: None,
            conflicts: Vec::new(),
            replaces: Vec::new(),
            build_env: None,
            build_only: false,
        }
    }

    #[test]
    fn linked_set_includes_requested_roots_and_runtime_deps() {
        let mut world = InstalledWorld {
            root: PathBuf::from("/tmp"),
            states: BTreeMap::from([
                ("app".to_owned(), state("app", &["lib"], true, false)),
                ("lib".to_owned(), state("lib", &[], false, false)),
            ]),
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            linked_cache: None,
        };
        let linked = world.linked().clone();
        assert!(linked.contains("app"));
        assert!(linked.contains("lib"));
    }

    #[test]
    fn linked_set_excludes_store_only_packages() {
        let mut world = InstalledWorld {
            root: PathBuf::from("/tmp"),
            states: BTreeMap::from([
                ("app".to_owned(), state("app", &[], true, false)),
                ("cache".to_owned(), state("cache", &[], false, false)),
            ]),
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            linked_cache: None,
        };
        let linked = world.linked().clone();
        assert!(linked.contains("app"));
        assert!(!linked.contains("cache"));
    }

    #[test]
    fn bin_linked_links_build_only_only_when_requested_by_name() {
        // `clang`: build-only, but explicitly requested -> its own bins link (user asked by name).
        let mut clang = state("clang", &["llvm"], true, false);
        clang.build_only = true;
        // `llvm`: build-only, pulled in only as a dependency (held) -> stays off PATH.
        let mut llvm = state("llvm", &[], false, true);
        llvm.build_only = true;
        // `build-env`: build-only meta seed -> links nothing of its held closure (`toolchain`).
        let mut build_env = state("build-env", &["toolchain"], true, false);
        build_env.build_only = true;
        // `rust` (requested, non-build-only) reaches the held non-build-only `wrappers` (links, e.g.
        // the `cc` wrapper) but the build-only `llvm` behind them is a barrier and stays off PATH.
        let wrappers = state("wrappers", &["llvm"], false, true);
        let rust = state("rust", &["wrappers"], true, false);
        let world = InstalledWorld {
            root: PathBuf::from("/tmp"),
            states: BTreeMap::from([
                ("app".to_owned(), state("app", &["lib"], true, false)),
                ("lib".to_owned(), state("lib", &[], false, true)),
                ("rust".to_owned(), rust),
                ("wrappers".to_owned(), wrappers),
                ("clang".to_owned(), clang),
                ("llvm".to_owned(), llvm),
                ("build-env".to_owned(), build_env),
                ("toolchain".to_owned(), state("toolchain", &[], false, true)),
            ]),
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            linked_cache: None,
        };
        let on_path = world.bin_linked_immut();
        // Requested packages and their reachable non-build-only runtime deps link.
        assert!(on_path.contains("app") && on_path.contains("lib"));
        assert!(on_path.contains("rust") && on_path.contains("wrappers"));
        // A build-only package links its own bins when explicitly requested (`grm install clang`)...
        assert!(on_path.contains("clang"));
        // ...but never as a dependency: `llvm` (behind the non-build-only `wrappers` and the
        // build-only `clang`) and `toolchain` (behind build-env) stay off PATH.
        assert!(!on_path.contains("llvm"));
        assert!(!on_path.contains("toolchain"));
        // The GC-root set still pins the whole closure regardless.
        let linked = world.linked_immut();
        assert!(linked.contains("llvm") && linked.contains("toolchain"));
    }

    #[test]
    fn mutation_invalidates_linked_cache() {
        let mut world = InstalledWorld {
            root: PathBuf::from("/tmp"),
            states: BTreeMap::from([
                ("app".to_owned(), state("app", &[], true, false)),
                ("lib".to_owned(), state("lib", &[], false, false)),
            ]),
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            linked_cache: None,
        };
        assert!(!world.linked().contains("lib"));
        world.update("app", |s| s.runtime_deps.push("lib".to_owned()));
        assert!(world.linked().contains("lib"));
    }
}
