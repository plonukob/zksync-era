//! Crate allowing to calculate root hashes and Merkle proofs for small in-memory Merkle trees.

// Linter settings.
#![warn(missing_debug_implementations, missing_docs, bare_trait_objects)]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::must_use_candidate, clippy::similar_names)]

use std::{collections::VecDeque, iter};

use once_cell::sync::Lazy;

#[cfg(test)]
mod tests;

use zksync_basic_types::H256;
use zksync_crypto::hasher::{keccak::KeccakHasher, Hasher};

/// Maximum supported depth of the tree. 32 corresponds to `2^32` elements in the tree, which
/// we unlikely to ever hit.
const MAX_TREE_DEPTH: usize = 32;

/// In-memory Merkle tree of bounded depth (no more than 32).
///
/// The tree is left-leaning, meaning that during its initialization, the size of a tree
/// can be specified larger than the number of provided leaves. In this case, the remaining leaves
/// will be considered to equal `[0_u8; LEAF_SIZE]`.
///
/// The tree has dynamic size, meaning that it can grow by a factor of 2 when the number of leaves
/// exceeds the current tree size. It does not shrink.
///
/// The tree is optimized for the case when the queries are performed on the rightmost leaves
/// and the leftmost leaves are cached (trimmed). Caching enables the merkle roots and paths to be computed
/// in `O(max(n, depth))` time, where `n` is the number of uncached leaves (in contrast to the total number of
/// leaves). Cache itself only takes up `O(depth)` space. However, caching prevents the retrieval of paths to the
/// cached leaves.
#[derive(Debug, Clone)]
pub struct MiniMerkleTree<const LEAF_SIZE: usize, H = KeccakHasher> {
    hasher: H,
    hashes: VecDeque<H256>,
    binary_tree_size: usize,
    start_index: usize,
    cache: Vec<H256>,
}

impl<const LEAF_SIZE: usize> MiniMerkleTree<LEAF_SIZE>
where
    KeccakHasher: HashEmptySubtree<LEAF_SIZE>,
{
    /// Creates a new Merkle tree from the supplied leaves. If `min_tree_size` is supplied and is larger
    /// than the number of the supplied leaves, the leaves are padded to `min_tree_size` with `[0_u8; LEAF_SIZE]` entries.
    /// The hash function used in keccak-256.
    ///
    /// # Panics
    ///
    /// Panics in the same situations as [`Self::with_hasher()`].
    pub fn new(
        leaves: impl Iterator<Item = [u8; LEAF_SIZE]>,
        min_tree_size: Option<usize>,
    ) -> Self {
        Self::with_hasher(KeccakHasher, leaves, min_tree_size)
    }
}

impl<const LEAF_SIZE: usize, H> MiniMerkleTree<LEAF_SIZE, H>
where
    H: HashEmptySubtree<LEAF_SIZE>,
{
    /// Creates a new Merkle tree from the supplied leaves. If `min_tree_size` is supplied and is larger than the
    /// number of the supplied leaves, the leaves are padded to `min_tree_size` with `[0_u8; LEAF_SIZE]` entries.
    ///
    /// # Panics
    ///
    /// Panics if any of the following conditions applies:
    ///
    /// - `min_tree_size` (if supplied) is not a power of 2.
    /// - The number of leaves is greater than `2^32`.
    pub fn with_hasher(
        hasher: H,
        leaves: impl Iterator<Item = [u8; LEAF_SIZE]>,
        min_tree_size: Option<usize>,
    ) -> Self {
        let hashes: VecDeque<_> = leaves.map(|bytes| hasher.hash_bytes(&bytes)).collect();
        let mut binary_tree_size = hashes.len().next_power_of_two();
        if let Some(min_tree_size) = min_tree_size {
            assert!(
                min_tree_size.is_power_of_two(),
                "tree size must be a power of 2"
            );
            binary_tree_size = min_tree_size.max(binary_tree_size);
        }
        assert!(
            tree_depth_by_size(binary_tree_size) <= MAX_TREE_DEPTH,
            "Tree contains more than {} items; this is not supported",
            1 << MAX_TREE_DEPTH
        );

        Self {
            hasher,
            hashes,
            binary_tree_size,
            start_index: 0,
            cache: vec![],
        }
    }

    /// Returns `true` if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.start_index == 0 && self.hashes.is_empty()
    }

    /// Returns the root hash of this tree.
    pub fn merkle_root(&self) -> H256 {
        if self.hashes.is_empty() {
            let depth = tree_depth_by_size(self.binary_tree_size);
            if self.start_index == 0 {
                self.hasher.empty_subtree_hash(depth)
            } else {
                self.cache[depth]
            }
        } else {
            self.compute_merkle_root_and_path(0, None, None)
        }
    }

    /// Returns the root hash and the Merkle proof for a leaf with the specified 0-based `index`.
    /// `index` is relative to the leftmost uncached leaf.
    pub fn merkle_root_and_path(&self, index: usize) -> (H256, Vec<H256>) {
        let mut end_path = vec![];
        let root_hash = self.compute_merkle_root_and_path(index, None, Some(&mut end_path));
        (root_hash, end_path)
    }

    /// Returns the root hash and the Merkle proofs for a range of leafs.
    /// The range is 0..length, where `0` is the leftmost untrimmed leaf.
    pub fn merkle_root_and_paths_for_range(&self, length: usize) -> (H256, Vec<H256>, Vec<H256>) {
        let (mut left_path, mut right_path) = (vec![], vec![]);
        let root_hash = self.compute_merkle_root_and_path(
            length - 1,
            Some(&mut left_path),
            Some(&mut right_path),
        );
        (root_hash, left_path, right_path)
    }

    /// Adds a new leaf to the tree (replaces leftmost empty leaf).
    /// If the tree is full, its size is doubled.
    /// Note: empty leaves != zero leaves.
    pub fn push(&mut self, leaf: [u8; LEAF_SIZE]) {
        let leaf_hash = self.hasher.hash_bytes(&leaf);
        self.hashes.push_back(leaf_hash);
        if self.start_index + self.hashes.len() > self.binary_tree_size {
            self.binary_tree_size *= 2;
        }
    }

    /// Caches the rightmost `count` leaves.
    /// Does not affect the root hash, but makes it impossible to get the paths to the cached leaves.
    /// # Panics
    /// Panics if `count` is greater than the number of non-cached leaves in the tree.
    pub fn trim_start(&mut self, count: usize) {
        assert!(self.hashes.len() >= count, "not enough leaves to cache");
        let mut new_cache = vec![];
        // Cache is a subset of the path to the first untrimmed leaf.
        let root = self.compute_merkle_root_and_path(count, None, Some(&mut new_cache));
        self.hashes.drain(..count);
        self.start_index += count;
        // It is important to add the root in case we just trimmed all leaves *and*
        // the tree will grow on the next push.
        new_cache.push(root);
        self.cache = new_cache;
    }

    fn compute_merkle_root_and_path(
        &self,
        mut end_index: usize,
        mut start_path: Option<&mut Vec<H256>>,
        mut end_path: Option<&mut Vec<H256>>,
    ) -> H256 {
        let depth = tree_depth_by_size(self.binary_tree_size);
        if let Some(left_path) = start_path.as_deref_mut() {
            left_path.reserve(depth);
        }
        if let Some(right_path) = end_path.as_deref_mut() {
            right_path.reserve(depth);
        }

        let mut hashes = self.hashes.clone();
        let mut start_index = self.start_index;

        for level in 0..depth {
            let empty_hash_at_level = self.hasher.empty_subtree_hash(level);

            if start_index % 2 == 1 {
                hashes.push_front(self.cache[level]);
            }
            if hashes.len() % 2 == 1 {
                hashes.push_back(empty_hash_at_level);
            }

            let push_sibling_hash = |path: Option<&mut Vec<H256>>, index: usize| {
                // `index` is relative to `head_index`
                if let Some(path) = path {
                    let sibling = ((start_index + index) ^ 1) - start_index + start_index % 2;
                    let hash = hashes.get(sibling).copied().unwrap_or_default();
                    path.push(hash);
                }
            };

            push_sibling_hash(start_path.as_deref_mut(), 0);
            push_sibling_hash(end_path.as_deref_mut(), end_index);

            let level_len = hashes.len() / 2;
            for i in 0..level_len {
                hashes[i] = self.hasher.compress(&hashes[2 * i], &hashes[2 * i + 1]);
            }

            hashes.drain(level_len..);
            end_index = (end_index + start_index % 2) / 2;
            start_index /= 2;
        }

        hashes[0]
    }
}

fn tree_depth_by_size(tree_size: usize) -> usize {
    debug_assert!(tree_size.is_power_of_two());
    tree_size.trailing_zeros() as usize
}

/// Hashing of empty binary Merkle trees.
pub trait HashEmptySubtree<const LEAF_SIZE: usize>:
    'static + Send + Sync + Hasher<Hash = H256>
{
    /// Returns the hash of an empty subtree with the given depth. Implementations
    /// are encouraged to cache the returned values.
    fn empty_subtree_hash(&self, depth: usize) -> H256;
}

impl HashEmptySubtree<88> for KeccakHasher {
    fn empty_subtree_hash(&self, depth: usize) -> H256 {
        static EMPTY_TREE_HASHES: Lazy<Vec<H256>> = Lazy::new(compute_empty_tree_hashes::<88>);
        EMPTY_TREE_HASHES[depth]
    }
}

fn compute_empty_tree_hashes<const LEAF_SIZE: usize>() -> Vec<H256> {
    let empty_leaf_hash = KeccakHasher.hash_bytes(&[0_u8; LEAF_SIZE]);
    iter::successors(Some(empty_leaf_hash), |hash| {
        Some(KeccakHasher.compress(hash, hash))
    })
    .take(MAX_TREE_DEPTH + 1)
    .collect()
}
