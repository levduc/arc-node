//! Persistent, incremental sharded-JMT for the payment lane — the shared engine both the
//! trie-db overlay (read-only root) and the provider commit hook (advance base) call.
//!
//! Correctness model (mirrors reth's MPT):
//!   - `readonly_root(changes)` computes the root of (committed base + this block's k changes)
//!     WITHOUT persisting. Pure => reth's ~11 speculative builds + validate + witness all read the
//!     same committed base and agree. O(k log n).
//!   - `commit(changes)` advances the persisted base by one block (the ONLY writer), called once
//!     per canonical block from `write_hashed_state`.
use crate::{combine_roots, encode_account, RedbNodeStore, SHARDS};
use alloy_primitives::B256;
use jmt::{JellyfishMerkleTree, KeyHash, OwnedValue, Version};
use once_cell::sync::OnceCell;
use std::sync::Mutex;

pub fn enabled() -> bool {
    std::env::var("ARC_PAYMENT_ROOT").map(|v| v == "jmt").unwrap_or(false)
}

struct State {
    shards: Vec<RedbNodeStore>,
    committed: Version, // highest persisted version = last canonical block applied
}
static JMT: OnceCell<Mutex<State>> = OnceCell::new();

fn state() -> &'static Mutex<State> {
    JMT.get_or_init(|| {
        let base = std::env::var("ARC_JMT_STORE_PATH").unwrap_or_else(|_| "jmt-store".into());
        std::fs::create_dir_all(&base).ok();
        let shards = (0..SHARDS)
            .map(|i| RedbNodeStore::open(std::path::Path::new(&base).join(format!("shard{i:02}.redb"))))
            .collect();
        Mutex::new(State { shards, committed: 0 })
    })
}

fn shard_of(kh: &KeyHash) -> usize { (kh.0[0] >> 4) as usize }

fn partition(changes: &[(B256, Option<OwnedValue>)]) -> Vec<Vec<(KeyHash, Option<OwnedValue>)>> {
    let mut per: Vec<Vec<_>> = (0..SHARDS).map(|_| Vec::new()).collect();
    for (addr, v) in changes {
        let kh = KeyHash(addr.0);
        per[shard_of(&kh)].push((kh, v.clone()));
    }
    per
}

/// Encode reth (nonce, balance_be) changes into JMT leaf values.
pub fn encode_changes<'a>(
    accounts: impl Iterator<Item = (B256, Option<(u64, [u8; 32])>)>,
) -> Vec<(B256, Option<OwnedValue>)> {
    accounts
        .map(|(a, v)| (a, v.map(|(n, b)| encode_account(n, B256::from(b)))))
        .collect()
}

/// Compute the lane root of committed-base + `changes`, WITHOUT persisting. O(k log n).
pub fn readonly_root(changes: &[(B256, Option<OwnedValue>)]) -> B256 {
    let st = state().lock().unwrap();
    let version = st.committed + 1;
    let per = partition(changes);
    let roots: Vec<[u8; 32]> = st
        .shards
        .iter()
        .zip(per)
        .map(|(store, w)| {
            let tree = JellyfishMerkleTree::<_, sha2::Sha256>::new(store);
            let (root, _batch) = tree.put_value_set(w, version).expect("jmt readonly");
            root.0 // discard the batch => read-only
        })
        .collect();
    combine_roots(&roots)
}

/// Advance the committed base by one canonical block, persisting `changes`. The sole writer.
pub fn commit(changes: &[(B256, Option<OwnedValue>)]) -> B256 {
    use jmt::storage::TreeWriter;
    let mut st = state().lock().unwrap();
    let version = st.committed + 1;
    let per = partition(changes);
    let roots: Vec<[u8; 32]> = st
        .shards
        .iter()
        .zip(per)
        .map(|(store, w)| {
            let tree = JellyfishMerkleTree::<_, sha2::Sha256>::new(store);
            let (root, batch) = tree.put_value_set(w, version).expect("jmt commit");
            store.write_node_batch(&batch.node_batch).expect("jmt persist");
            root.0
        })
        .collect();
    st.committed = version;
    combine_roots(&roots)
}
