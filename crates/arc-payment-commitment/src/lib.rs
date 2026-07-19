//! Arc payment-lane state commitment: a **sharded Jellyfish-Merkle-Tree** alternative to
//! reth's MPT, computed directly from reth's own [`HashedPostState`].
//!
//! Why this exists: the payment lane's `stateRoot` is Arc's — the CL only needs every payment
//! EL to compute the *same* header, not Ethereum-MPT semantics. So the lane may commit its
//! account state with a structure tuned for it: a 16-way forest of JMTs partitioned by the
//! first nibble of `keccak(address)`, with the lane root = `keccak(shard_root_0 ‖ … ‖ shard_root_15)`.
//! The 16 shards' roots are computed in parallel (rayon). This is deterministic (consensus-safe)
//! and, measured against reth's parallel sparse-trie on 10M accounts, lands in the same
//! millisecond class while adding native versioning and one-table KV storage.
//!
//! This crate is the drop-in the provider seam calls: feed it the block's `HashedPostState`
//! (what reth already builds via `HashedPostState::from_bundle_state`) and it returns the
//! lane root. A pluggable [`NodeStore`] backs the JMT nodes (in-memory here; an MDBX/redb
//! impl lives in the node). Storage tries are intentionally unsupported — a payment lane has
//! no contract storage; only account records are committed.

use alloy_primitives::B256;
use jmt::storage::{Node, NodeBatch, NodeKey, TreeReader, TreeWriter};
use jmt::{JellyfishMerkleTree, KeyHash, OwnedValue, Version};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::RwLock;

pub const SHARDS: usize = 16;

/// Node storage backing one JMT shard. The node ships an MDBX/redb implementation;
/// the in-memory [`MemNodeStore`] here is used by tests and by the genesis builder.
pub trait NodeStore: TreeReader + TreeWriter + Send + Sync {}
impl<T: TreeReader + TreeWriter + Send + Sync> NodeStore for T {}

/// Encode a reth account into the lane's compact record: nonce(8 LE) ‖ balance(32 BE).
/// (code_hash/storage_root omitted — a payment account has neither.)
pub fn encode_account(nonce: u64, balance: B256) -> OwnedValue {
    let mut v = Vec::with_capacity(40);
    v.extend_from_slice(&nonce.to_le_bytes());
    v.extend_from_slice(balance.as_slice());
    v
}

/// Map a hashed address (`keccak(address)`, already what `HashedPostState` keys by) to
/// (shard index, JMT key). The JMT key is the full 32-byte hashed address.
#[inline]
fn locate(hashed_address: B256) -> (usize, KeyHash) {
    ((hashed_address[0] >> 4) as usize, KeyHash(hashed_address.0))
}

/// A 16-shard JMT commitment over payment-account state.
pub struct ShardedJmt<S: NodeStore> {
    shards: Vec<S>,
}

impl<S: NodeStore> ShardedJmt<S> {
    pub fn new(shards: Vec<S>) -> Self {
        assert_eq!(shards.len(), SHARDS, "need exactly {SHARDS} shards");
        Self { shards }
    }

    /// Apply a write-set at `version` and return the lane root.
    /// `writes` are (hashed_address, Some(account_record) | None-for-deletion).
    pub fn commit(
        &self,
        writes: impl IntoIterator<Item = (B256, Option<OwnedValue>)>,
        version: Version,
    ) -> anyhow::Result<B256> {
        let mut per_shard: Vec<Vec<(KeyHash, Option<OwnedValue>)>> =
            (0..SHARDS).map(|_| Vec::new()).collect();
        for (addr, val) in writes {
            let (s, k) = locate(addr);
            per_shard[s].push((k, val));
        }
        let roots: Vec<[u8; 32]> = self
            .shards
            .par_iter()
            .zip(per_shard.into_par_iter())
            .map(|(store, batch)| {
                let tree = JellyfishMerkleTree::<S, sha2::Sha256>::new(store);
                let (root, tub) = tree.put_value_set(batch, version)?;
                store.write_node_batch(&tub.node_batch)?;
                Ok::<_, anyhow::Error>(root.0)
            })
            .collect::<Result<_, _>>()?;
        Ok(combine_roots(&roots))
    }
}

/// lane root = keccak256(shard_root_0 ‖ … ‖ shard_root_15).
pub fn combine_roots(shard_roots: &[[u8; 32]]) -> B256 {
    use tiny_keccak::{Hasher, Keccak};
    let mut h = Keccak::v256();
    for r in shard_roots {
        h.update(r);
    }
    let mut out = [0u8; 32];
    h.finalize(&mut out);
    B256::from(out)
}

// ---- in-memory node store (tests + genesis) ----

#[derive(Default)]
pub struct MemNodeStore {
    nodes: RwLock<HashMap<NodeKey, Node>>,
    values: RwLock<std::collections::BTreeMap<(Version, KeyHash), Option<OwnedValue>>>,
}

impl TreeReader for MemNodeStore {
    fn get_node_option(&self, key: &NodeKey) -> anyhow::Result<Option<Node>> {
        Ok(self.nodes.read().unwrap().get(key).cloned())
    }
    fn get_value_option(&self, v: Version, k: KeyHash) -> anyhow::Result<Option<OwnedValue>> {
        Ok(self
            .values
            .read()
            .unwrap()
            .range(..=(v, k))
            .rev()
            .find(|((_, kk), _)| *kk == k)
            .and_then(|(_, val)| val.clone()))
    }
    fn get_rightmost_leaf(&self) -> anyhow::Result<Option<(NodeKey, jmt::storage::LeafNode)>> {
        Ok(None)
    }
}
impl TreeWriter for MemNodeStore {
    fn write_node_batch(&self, batch: &NodeBatch) -> anyhow::Result<()> {
        let mut n = self.nodes.write().unwrap();
        for (k, v) in batch.nodes() {
            n.insert(k.clone(), v.clone());
        }
        let mut vals = self.values.write().unwrap();
        for ((ver, kh), v) in batch.values() {
            vals.insert((*ver, *kh), v.clone());
        }
        Ok(())
    }
}

/// Convenience: build a fresh in-memory 16-shard commitment.
pub fn in_memory() -> ShardedJmt<MemNodeStore> {
    ShardedJmt::new((0..SHARDS).map(|_| MemNodeStore::default()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie::HashedPostState;

    /// Consumes reth's own `HashedPostState` (the exact type the provider seam hands us) and
    /// commits its account set — proves this crate links + speaks reth's commitment types.
    fn root_from_reth_state(jmt: &ShardedJmt<MemNodeStore>, hps: &HashedPostState, v: Version) -> B256 {
        let writes = hps.accounts.iter().map(|(hashed_addr, maybe_acct)| {
            let rec = maybe_acct
                .as_ref()
                .map(|a| encode_account(a.nonce, B256::from(a.balance.to_be_bytes())));
            (*hashed_addr, rec)
        });
        jmt.commit(writes, v).unwrap()
    }

    #[test]
    fn determinism_across_instances() {
        use reth_primitives_traits::Account;
        let mut hps = HashedPostState::default();
        for i in 0..500u64 {
            let mut a = [0u8; 32];
            a[24..].copy_from_slice(&i.to_be_bytes());
            hps.accounts.insert(
                B256::from(a),
                Some(Account { nonce: i, balance: alloy_primitives::U256::from(1_000u64 + i), ..Default::default() }),
            );
        }
        let a = in_memory();
        let b = in_memory();
        let ra = root_from_reth_state(&a, &hps, 0);
        let rb = root_from_reth_state(&b, &hps, 0);
        assert_eq!(ra, rb, "two instances must agree — the consensus property");
        assert_ne!(ra, B256::ZERO);
    }

    #[test]
    fn root_changes_with_state() {
        use reth_primitives_traits::Account;
        let jmt = in_memory();
        let mut hps = HashedPostState::default();
        hps.accounts.insert(B256::repeat_byte(1),
            Some(Account { nonce: 0, balance: alloy_primitives::U256::from(100u64), ..Default::default() }));
        let r0 = root_from_reth_state(&jmt, &hps, 0);
        hps.accounts.insert(B256::repeat_byte(1),
            Some(Account { nonce: 1, balance: alloy_primitives::U256::from(90u64), ..Default::default() }));
        let r1 = root_from_reth_state(&jmt, &hps, 1);
        assert_ne!(r0, r1);
    }
}

pub mod persistent;
pub mod dense;

/// redb-backed JMT node store (one shard). Used by the persistent incremental JMT.
pub struct RedbNodeStore {
    db: redb::Database,
}
impl RedbNodeStore {
    const NODES: redb::TableDefinition<'static, &'static [u8], &'static [u8]> =
        redb::TableDefinition::new("jmt_nodes");
    const VALUES: redb::TableDefinition<'static, &'static [u8], &'static [u8]> =
        redb::TableDefinition::new("accounts_versioned");
    pub fn open(path: std::path::PathBuf) -> Self {
        let db = redb::Database::create(path).expect("open jmt shard");
        let w = db.begin_write().unwrap();
        w.open_table(Self::NODES).unwrap();
        w.open_table(Self::VALUES).unwrap();
        w.commit().unwrap();
        Self { db }
    }
}
impl jmt::storage::TreeReader for RedbNodeStore {
    fn get_node_option(&self, key: &jmt::storage::NodeKey) -> anyhow::Result<Option<jmt::storage::Node>> {
        use redb::ReadableTable;
        let r = self.db.begin_read()?;
        let t = r.open_table(Self::NODES)?;
        Ok(t.get(borsh::to_vec(key)?.as_slice())?
            .map(|v| borsh::from_slice::<jmt::storage::Node>(v.value()).unwrap()))
    }
    fn get_value_option(&self, ver: jmt::Version, kh: jmt::KeyHash)
        -> anyhow::Result<Option<jmt::OwnedValue>> {
        use redb::ReadableTable;
        let r = self.db.begin_read()?;
        let t = r.open_table(Self::VALUES)?;
        let mut lo = kh.0.to_vec(); lo.extend_from_slice(&0u64.to_be_bytes());
        let mut hi = kh.0.to_vec(); hi.extend_from_slice(&ver.to_be_bytes());
        Ok(t.range(lo.as_slice()..=hi.as_slice())?.next_back().transpose()?
            .and_then(|(_, v)| borsh::from_slice::<Option<jmt::OwnedValue>>(v.value()).unwrap()))
    }
    fn get_rightmost_leaf(&self) -> anyhow::Result<Option<(jmt::storage::NodeKey, jmt::storage::LeafNode)>> {
        Ok(None)
    }
}
impl jmt::storage::TreeWriter for RedbNodeStore {
    fn write_node_batch(&self, batch: &jmt::storage::NodeBatch) -> anyhow::Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut nt = w.open_table(Self::NODES)?;
            for (k, v) in batch.nodes() {
                nt.insert(borsh::to_vec(k)?.as_slice(), borsh::to_vec(v)?.as_slice())?;
            }
            let mut vt = w.open_table(Self::VALUES)?;
            for ((ver, kh), val) in batch.values() {
                let mut key = kh.0.to_vec(); key.extend_from_slice(&ver.to_be_bytes());
                vt.insert(key.as_slice(), borsh::to_vec(val)?.as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }
}
