//! Parallel storage implementation.

#![allow(missing_docs, clippy::missing_panics_doc)]

use std::{
    any::Any,
    collections::{HashMap, VecDeque},
    mem,
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use super::{patch::PartialPatchSet, Database, NodeKeys, PatchSet};
use crate::{
    errors::DeserializeError,
    types::{Manifest, Node, NodeKey, ProfiledTreeOperation, Root},
    PruneDatabase, PrunePatchSet,
};

#[derive(Debug, Clone)]
struct PersistenceCommand {
    manifest: Manifest,
    patch: Arc<PartialPatchSet>,
    stale_keys: Vec<NodeKey>,
}

/// Database implementation that persists changes in a background thread. Not yet applied changes
/// are queued up and are used in `Database` getters. A queue can sometimes be stale (i.e., changes
/// at its head may have been applied), but this is fine as long as changes are applied atomically and sequentially.
///
/// # Assumptions
///
/// - This is the only mutable database instance.
/// - All database updates update the same tree version (e.g., the tree is being recovered).
/// - The application supports latest changes being dropped.
#[derive(Debug)]
pub(crate) struct ParallelDatabase<DB> {
    inner: DB,
    updated_version: u64,
    command_sender: mpsc::SyncSender<PersistenceCommand>,
    persistence_handle: Option<thread::JoinHandle<()>>,
    commands: VecDeque<PersistenceCommand>,
}

impl<DB: Database + Clone + 'static> ParallelDatabase<DB> {
    fn new(inner: DB, updated_version: u64, buffer_capacity: usize) -> Self {
        let (command_sender, command_receiver) = mpsc::sync_channel(buffer_capacity);
        let persistence_database = inner.clone();
        let persistence_handle = thread::spawn(move || {
            Self::run_persistence(persistence_database, updated_version, command_receiver);
        });
        Self {
            inner,
            updated_version,
            command_sender,
            persistence_handle: Some(persistence_handle),
            commands: VecDeque::with_capacity(buffer_capacity),
        }
    }

    fn run_persistence(
        mut database: DB,
        updated_version: u64,
        command_receiver: mpsc::Receiver<PersistenceCommand>,
    ) {
        let mut persisted_count = 0;
        while let Ok(command) = command_receiver.recv() {
            tracing::debug!("Persisting patch #{persisted_count}");
            // Reconstitute a `PatchSet` and apply it to the underlying database.
            let patch = PatchSet {
                manifest: command.manifest,
                patches_by_version: HashMap::from([(updated_version, command.patch.cloned())]),
                updated_version: Some(updated_version),
                stale_keys_by_version: HashMap::from([(updated_version, command.stale_keys)]),
            };
            database.apply_patch(patch);
            tracing::debug!("Persisted patch #{persisted_count}");
            persisted_count += 1;
        }
        drop(command_receiver);
    }
}

impl<DB: Database> ParallelDatabase<DB> {
    fn wait_sync(&mut self) {
        while !self.commands.is_empty() {
            self.commands
                .retain(|command| Arc::strong_count(&command.patch) > 1);
            thread::sleep(Duration::from_millis(50)); // TODO: more intelligent approach
        }

        // Check that the persistence thread hasn't panicked
        let persistence_handle = self
            .persistence_handle
            .as_ref()
            .expect("Persistence thread previously panicked");
        if persistence_handle.is_finished() {
            mem::take(&mut self.persistence_handle)
                .unwrap()
                .join()
                .expect("Persistence thread panicked");
            unreachable!("Persistence thread never exits when `ParallelDatabase` is alive");
        }
    }

    fn join(mut self) -> DB {
        let join_handle = mem::take(&mut self.persistence_handle)
            .expect("Persistence thread previously panicked");
        drop(self.command_sender);
        drop(self.commands);
        join_handle.join().expect("Persistence thread panicked");
        self.inner
    }
}

impl<DB: Database> Database for ParallelDatabase<DB> {
    fn try_manifest(&self) -> Result<Option<Manifest>, DeserializeError> {
        let latest_command = self.commands.iter().next_back();
        if let Some(command) = latest_command {
            Ok(Some(command.manifest.clone()))
        } else {
            self.inner.try_manifest()
        }
    }

    fn try_root(&self, version: u64) -> Result<Option<Root>, DeserializeError> {
        if version != self.updated_version {
            return self.inner.try_root(version);
        }
        let root = self
            .commands
            .iter()
            .rev()
            .find_map(|command| command.patch.root.clone());
        if let Some(root) = root {
            Ok(Some(root))
        } else {
            self.inner.try_root(version)
        }
    }

    fn try_tree_node(
        &self,
        key: &NodeKey,
        is_leaf: bool,
    ) -> Result<Option<Node>, DeserializeError> {
        if key.version != self.updated_version {
            return self.inner.try_tree_node(key, is_leaf);
        }
        let node = self
            .commands
            .iter()
            .rev()
            .find_map(|command| command.patch.nodes.get(key));
        if let Some(node) = node {
            debug_assert_eq!(matches!(node, Node::Leaf(_)), is_leaf);
            Ok(Some(node.clone()))
        } else {
            self.inner.try_tree_node(key, is_leaf)
        }
    }

    fn tree_nodes(&self, keys: &NodeKeys) -> Vec<Option<Node>> {
        let mut nodes = vec![None; keys.len()];
        for command in self.commands.iter().rev() {
            for (key_idx, (key, is_leaf)) in keys.iter().enumerate() {
                if nodes[key_idx].is_some() {
                    continue;
                }
                if let Some(node) = command.patch.nodes.get(key) {
                    debug_assert_eq!(matches!(node, Node::Leaf(_)), *is_leaf);
                    nodes[key_idx] = Some(node.clone());
                }
            }
        }

        // Load missing nodes from the underlying database
        let (key_indexes, missing_keys): (Vec<_>, Vec<_>) = keys
            .iter()
            .copied()
            .enumerate()
            .filter(|(i, _)| nodes[*i].is_none())
            .unzip();
        let inner_nodes = self.inner.tree_nodes(&missing_keys);
        for (key_idx, node) in key_indexes.into_iter().zip(inner_nodes) {
            nodes[key_idx] = node;
        }
        nodes
    }

    fn start_profiling(&self, operation: ProfiledTreeOperation) -> Box<dyn Any> {
        self.inner.start_profiling(operation)
    }

    fn apply_patch(&mut self, mut patch: PatchSet) {
        let partial_patch = if let Some(updated_version) = patch.updated_version {
            assert_eq!(
                updated_version, self.updated_version,
                "Unsupported update: must update predefined version {}",
                self.updated_version
            );
            assert_eq!(
                patch.patches_by_version.len(),
                1,
                "Unsupported update: must *only* update version {updated_version}"
            );

            // Garbage-collect patches already applied by the persistence thread. This will remove all patches
            // if the persistence thread has panicked, but this is OK because we'll panic below anyway.
            self.commands
                .retain(|command| Arc::strong_count(&command.patch) > 1);
            tracing::debug!("Retained commands: {}", self.commands.len());

            patch
                .patches_by_version
                .remove(&updated_version)
                .expect("PatchSet invariant violated: missing patch for the updated version")
        } else {
            // We only support manifest updates.
            assert!(patch.patches_by_version.is_empty(), "{patch:?}");
            PartialPatchSet::empty()
        };

        let mut stale_keys_by_version = patch.stale_keys_by_version;
        assert!(
            stale_keys_by_version.is_empty()
                || (stale_keys_by_version.len() == 1
                    && stale_keys_by_version.contains_key(&self.updated_version))
        );
        let stale_keys = stale_keys_by_version
            .remove(&self.updated_version)
            .unwrap_or_default();

        let command = PersistenceCommand {
            manifest: patch.manifest,
            patch: Arc::new(partial_patch),
            stale_keys,
        };
        if self.command_sender.send(command.clone()).is_err() {
            mem::take(&mut self.persistence_handle)
                .expect("Persistence thread previously panicked")
                .join()
                .expect("Persistence thread panicked");
            unreachable!("Persistence thread never exits when `ParallelDatabase` is alive");
        }
        self.commands.push_back(command);
    }
}

impl<DB: PruneDatabase> PruneDatabase for ParallelDatabase<DB> {
    fn min_stale_key_version(&self) -> Option<u64> {
        if self
            .commands
            .iter()
            .any(|command| command.stale_keys.is_empty())
        {
            return Some(self.updated_version);
        }
        self.inner.min_stale_key_version()
    }

    fn stale_keys(&self, version: u64) -> Vec<NodeKey> {
        if version != self.updated_version {
            return self.inner.stale_keys(version);
        }
        self.commands
            .iter()
            .flat_map(|command| command.stale_keys.clone())
            .chain(self.inner.stale_keys(version))
            .collect()
    }

    fn prune(&mut self, patch: PrunePatchSet) {
        // Require the underlying database to be fully synced.
        self.wait_sync();
        self.inner.prune(patch);
    }
}

#[derive(Debug)]
pub(crate) enum MaybeParallel<DB> {
    Just(DB),
    Parallel(ParallelDatabase<DB>),
}

impl<DB: PruneDatabase> MaybeParallel<DB> {
    pub fn wait_sync(&mut self) {
        if let Self::Parallel(db) = self {
            db.wait_sync();
        }
    }

    pub fn join(self) -> DB {
        match self {
            Self::Just(db) => db,
            Self::Parallel(db) => db.join(),
        }
    }
}

impl<DB: 'static + Clone + PruneDatabase> MaybeParallel<DB> {
    pub fn parallelize(&mut self, updated_version: u64, buffer_capacity: usize) {
        if let Self::Just(db) = self {
            *self = Self::Parallel(ParallelDatabase::new(
                db.clone(),
                updated_version,
                buffer_capacity,
            ));
        }
    }
}

impl<DB: Database> Database for MaybeParallel<DB> {
    fn try_manifest(&self) -> Result<Option<Manifest>, DeserializeError> {
        match self {
            Self::Just(db) => db.try_manifest(),
            Self::Parallel(db) => db.try_manifest(),
        }
    }

    fn try_root(&self, version: u64) -> Result<Option<Root>, DeserializeError> {
        match self {
            Self::Just(db) => db.try_root(version),
            Self::Parallel(db) => db.try_root(version),
        }
    }

    fn try_tree_node(
        &self,
        key: &NodeKey,
        is_leaf: bool,
    ) -> Result<Option<Node>, DeserializeError> {
        match self {
            Self::Just(db) => db.try_tree_node(key, is_leaf),
            Self::Parallel(db) => db.try_tree_node(key, is_leaf),
        }
    }

    fn tree_nodes(&self, keys: &NodeKeys) -> Vec<Option<Node>> {
        match self {
            Self::Just(db) => db.tree_nodes(keys),
            Self::Parallel(db) => db.tree_nodes(keys),
        }
    }

    fn start_profiling(&self, operation: ProfiledTreeOperation) -> Box<dyn Any> {
        match self {
            Self::Just(db) => db.start_profiling(operation),
            Self::Parallel(db) => db.start_profiling(operation),
        }
    }

    fn apply_patch(&mut self, patch: PatchSet) {
        match self {
            Self::Just(db) => db.apply_patch(patch),
            Self::Parallel(db) => db.apply_patch(patch),
        }
    }
}

impl<DB: PruneDatabase> PruneDatabase for MaybeParallel<DB> {
    fn min_stale_key_version(&self) -> Option<u64> {
        match self {
            Self::Just(db) => db.min_stale_key_version(),
            Self::Parallel(db) => db.min_stale_key_version(),
        }
    }

    fn stale_keys(&self, version: u64) -> Vec<NodeKey> {
        match self {
            Self::Just(db) => db.stale_keys(version),
            Self::Parallel(db) => db.stale_keys(version),
        }
    }

    fn prune(&mut self, patch: PrunePatchSet) {
        match self {
            Self::Just(db) => db.prune(patch),
            Self::Parallel(db) => db.prune(patch),
        }
    }
}
