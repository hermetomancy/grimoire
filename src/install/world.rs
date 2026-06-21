//! Single in-memory authority for installed package state.

use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use crate::{model::PackageState, nu::nuon_io};

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

impl InstalledWorld {
    /// Load from `{root}/state/packages/*.nuon`. Returns an empty world if the directory does not
    /// exist. Replaces the old `installed_states()`.
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

    /// Load while tolerating corrupt individual state files. Returns the world built from valid
    /// files plus a list of per-file error messages. Used only by `grm doctor`.
    pub fn load_tolerant(root: impl AsRef<Path>) -> (Self, Vec<String>) {
        let root = root.as_ref().to_path_buf();
        let state_dir = root.join("state").join("packages");
        let mut states = BTreeMap::new();
        let mut errors = Vec::new();
        if state_dir.exists() {
            if let Ok(entries) = fs::read_dir(&state_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
                        continue;
                    }
                    match nuon_io::read_nuon(&path) {
                        Ok(value) => match PackageState::from_value(value) {
                            Ok(state) => {
                                states.insert(state.name.clone(), state);
                            }
                            Err(e) => errors.push(format!("{}: {}", path.display(), e)),
                        },
                        Err(e) => errors.push(format!("{}: {}", path.display(), e)),
                    }
                }
            }
        }
        (
            Self {
                root,
                states,
                dirty: HashSet::new(),
                deleted: HashSet::new(),
                linked_cache: None,
            },
            errors,
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get(&self, name: &str) -> Option<&PackageState> {
        self.states.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PackageState> {
        self.states.values()
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
        self.states.values().find(|state| {
            state.bins.contains_key(dep) || state.provides.iter().any(|p| p == dep)
        })
    }

    /// Compute the linked set: requested/held roots plus transitive runtime deps.
    /// Lazy and cached; invalidates on mutation.
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

    pub fn to_states(&self) -> Vec<PackageState> {
        self.states.values().cloned().collect()
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
