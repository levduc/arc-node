//! JMT (Penumbra jmt crate = the Diem/Aptos Jellyfish Merkle Tree) as the payment-lane
//! state commitment: build N accounts, then measure per-block batched root updates.
//!
//! Comparison anchor (measured on the live payment lane, reth MPT, 10M accounts):
//!   state root ≈ 2.6–12 ms per block at 273–4,761 account updates/block.
//! Run:  cargo run --release --bin jmt_bench -- [n_accounts] [updates_per_block] [blocks]
//! Defaults: 10_000_000 4761 30.

use jmt::storage::{LeafNode, Node, NodeBatch, NodeKey, TreeReader, TreeWriter};
use jmt::{JellyfishMerkleTree, KeyHash, OwnedValue, Version};
use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;
use std::time::Instant;

/// In-memory node store (stands in for an MDBX/RocksDB table; HashMap keeps the
/// comparison honest vs our other RAM-resident experiments).
#[derive(Default)]
struct MemStore {
    nodes: RwLock<HashMap<NodeKey, Node>>,
    values: RwLock<BTreeMap<(Version, KeyHash), Option<OwnedValue>>>,
}

impl TreeReader for MemStore {
    fn get_node_option(&self, key: &NodeKey) -> anyhow::Result<Option<Node>> {
        Ok(self.nodes.read().unwrap().get(key).cloned())
    }
    fn get_value_option(&self, ver: Version, key: KeyHash) -> anyhow::Result<Option<OwnedValue>> {
        let vals = self.values.read().unwrap();
        Ok(vals
            .range(..=(ver, key))
            .rev()
            .find(|((_, k), _)| *k == key)
            .and_then(|(_, v)| v.clone()))
    }
    fn get_rightmost_leaf(&self) -> anyhow::Result<Option<(NodeKey, LeafNode)>> {
        Ok(None) // only needed for restore paths
    }
}

impl TreeWriter for MemStore {
    fn write_node_batch(&self, batch: &NodeBatch) -> anyhow::Result<()> {
        let mut nodes = self.nodes.write().unwrap();
        for (k, v) in batch.nodes() {
            nodes.insert(k.clone(), v.clone());
        }
        let mut vals = self.values.write().unwrap();
        for ((ver, kh), v) in batch.values() {
            vals.insert((*ver, *kh), v.clone());
        }
        Ok(())
    }
}

fn acct_key(i: u64) -> KeyHash {
    // address = 0x2000000000 + i, matching the preseed scheme
    let mut addr = [0u8; 20];
    addr[12..].copy_from_slice(&(0x20_0000_0000u64 + i).to_be_bytes());
    KeyHash::with::<sha2::Sha256>(addr)
}

fn acct_value(i: u64, balance: u128) -> OwnedValue {
    // account record ≈ nonce(8) + balance(16) + padding to ~72B (reth account-ish)
    let mut v = vec![0u8; 72];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v[8..24].copy_from_slice(&balance.to_le_bytes());
    v
}

fn main() -> anyhow::Result<()> {
    let args: Vec<u64> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let n: u64 = *args.first().unwrap_or(&10_000_000);
    let per_block: u64 = *args.get(1).unwrap_or(&4761);
    let blocks: u64 = *args.get(2).unwrap_or(&30);

    let store = MemStore::default();
    let tree = JellyfishMerkleTree::<_, sha2::Sha256>::new(&store);

    // ---- build: insert n accounts in genesis-sized batches ----
    println!("building {n} accounts (batched)...");
    let t0 = Instant::now();
    let mut version: Version = 0;
    let chunk = 100_000u64;
    let mut i = 0u64;
    while i < n {
        let hi = (i + chunk).min(n);
        let batch: Vec<(KeyHash, Option<OwnedValue>)> =
            (i..hi).map(|j| (acct_key(j), Some(acct_value(j, 1_000_000_000_000_000_000)))).collect();
        let (_root, tub) = tree.put_value_set(batch, version)?;
        store.write_node_batch(&tub.node_batch)?;
        version += 1;
        i = hi;
        if i % 1_000_000 == 0 {
            println!("  {i:>10} accounts, {:>6.1}s, nodes {}", t0.elapsed().as_secs_f64(),
                     store.nodes.read().unwrap().len());
        }
    }
    let build_s = t0.elapsed().as_secs_f64();
    let node_count = store.nodes.read().unwrap().len();
    println!("build done: {build_s:.1}s, {node_count} stored nodes");

    // ---- steady state: per-block batched updates (pool-walk pattern) ----
    println!("measuring {blocks} blocks x {per_block} account updates...");
    let mut times = Vec::new();
    let mut cursor = 0u64;
    for b in 0..blocks {
        let batch: Vec<(KeyHash, Option<OwnedValue>)> = (0..per_block)
            .map(|k| {
                let idx = (cursor + k) % n;
                (acct_key(idx), Some(acct_value(idx, 2_000_000_000_000_000_000 + b as u128)))
            })
            .collect();
        cursor = (cursor + per_block) % n;
        let t = Instant::now();
        let (_root, tub) = tree.put_value_set(batch, version)?;
        store.write_node_batch(&tub.node_batch)?;
        let dt = t.elapsed().as_secs_f64() * 1000.0;
        version += 1;
        times.push(dt);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg: f64 = times.iter().sum::<f64>() / times.len() as f64;
    println!(
        "JMT @{}M accounts, {} updates/block: avg {:.2} ms/block  p50 {:.2}  p95 {:.2}  ({:.1} µs/update)",
        n / 1_000_000, per_block, avg, times[times.len() / 2],
        times[(times.len() as f64 * 0.95) as usize], avg * 1000.0 / per_block as f64
    );
    println!("(reference: live payment lane reth-MPT @10M measured 2.6-12 ms/block at 273-4761 upd/blk)");
    Ok(())
}
