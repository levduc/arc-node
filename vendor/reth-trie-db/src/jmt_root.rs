//! Arc payment-lane JMT state root, injected into reth's overlay_root path.
//!
//! When `ARC_PAYMENT_ROOT=jmt` is set (payment EL only), reth's MPT state-root computation is
//! replaced by a persistent 16-shard Jellyfish-Merkle-Tree root over the block's changed
//! accounts (via `arc-payment-commitment`). Nodes persist in a redb store under
//! `ARC_JMT_STORE_PATH` (default `<cwd>/jmt-store`). The version advances per call.
//!
//! FIRST-CUT SCOPE: proves reth 2.3 produces blocks whose stateRoot is a JMT root. Determinism
//! across validators is guaranteed by arc-payment-commitment (same HashedPostState -> same root,
//! tested). Per-call idempotency on re-validation and stale-node GC are documented follow-ups.

use alloy_primitives::B256;
use arc_payment_commitment::{encode_account, ShardedJmt, SHARDS};
use jmt::storage::{LeafNode, Node, NodeBatch, NodeKey, TreeReader, TreeWriter};
use jmt::{KeyHash, OwnedValue, Version};
use once_cell::sync::OnceCell;
use redb::{Database, ReadableTable, TableDefinition};
use reth_trie_common::HashedPostStateSorted;
use std::sync::Mutex;

const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jmt_nodes");
const VALUES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts_versioned");

/// redb-backed JMT node store for one shard.
pub struct RedbShard {
    db: Database,
}
impl RedbShard {
    fn open(path: std::path::PathBuf) -> Self {
        let db = Database::create(path).expect("open jmt shard");
        let w = db.begin_write().unwrap();
        w.open_table(NODES).unwrap();
        w.open_table(VALUES).unwrap();
        w.commit().unwrap();
        Self { db }
    }
}
impl TreeReader for RedbShard {
    fn get_node_option(&self, key: &NodeKey) -> anyhow::Result<Option<Node>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(NODES)?;
        Ok(t.get(borsh::to_vec(key)?.as_slice())?
            .map(|v| borsh::from_slice::<Node>(v.value()).unwrap()))
    }
    fn get_value_option(&self, ver: Version, kh: KeyHash) -> anyhow::Result<Option<OwnedValue>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(VALUES)?;
        let mut lo = kh.0.to_vec(); lo.extend_from_slice(&0u64.to_be_bytes());
        let mut hi = kh.0.to_vec(); hi.extend_from_slice(&ver.to_be_bytes());
        Ok(t.range(lo.as_slice()..=hi.as_slice())?.next_back().transpose()?
            .and_then(|(_, v)| borsh::from_slice::<Option<OwnedValue>>(v.value()).unwrap()))
    }
    fn get_rightmost_leaf(&self) -> anyhow::Result<Option<(NodeKey, LeafNode)>> { Ok(None) }
}
impl TreeWriter for RedbShard {
    fn write_node_batch(&self, batch: &NodeBatch) -> anyhow::Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut nt = w.open_table(NODES)?;
            for (k, v) in batch.nodes() {
                nt.insert(borsh::to_vec(k)?.as_slice(), borsh::to_vec(v)?.as_slice())?;
            }
            let mut vt = w.open_table(VALUES)?;
            for ((ver, kh), val) in batch.values() {
                let mut key = kh.0.to_vec(); key.extend_from_slice(&ver.to_be_bytes());
                vt.insert(key.as_slice(), borsh::to_vec(val)?.as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }
}

static JMT: OnceCell<Mutex<ShardedJmt<RedbShard>>> = OnceCell::new();

fn jmt() -> &'static Mutex<ShardedJmt<RedbShard>> {
    JMT.get_or_init(|| {
        let base = std::env::var("ARC_JMT_STORE_PATH").unwrap_or_else(|_| "jmt-store".into());
        std::fs::create_dir_all(&base).ok();
        let shards = (0..SHARDS)
            .map(|i| RedbShard::open(std::path::Path::new(&base).join(format!("shard{i:02}.redb"))))
            .collect();
        Mutex::new(ShardedJmt::new(shards))
    })
}

/// True when the payment lane should commit with a JMT root instead of the MPT.
pub fn jmt_enabled() -> bool {
    std::env::var("ARC_PAYMENT_ROOT").map(|v| v == "jmt").unwrap_or(false)
}

/// Compute the JMT lane root over the block's changed accounts, versioned by the block being
/// built (`block_number`, = parent+1). This is IDEMPOTENT: reth calls overlay_root ~11x per block
/// while speculatively refining the payload, but every call for the same block uses the same JMT
/// version, so re-computes and the final build vs the validation agree. The JMT root itself is
/// version-independent (a function of the leaf set), so persisting speculative candidates at the
/// same version is safe last-writer-wins — the validated block's writeset persists last, giving
/// block N+1 the correct cumulative base at version N.
pub fn jmt_overlay_root(post_state: &HashedPostStateSorted, block_number: u64) -> B256 {
    let mut tree = jmt().lock().unwrap();
    let writes: Vec<_> = post_state.accounts().iter().map(|(hashed_addr, maybe_acct)| {
        let rec = maybe_acct
            .as_ref()
            .map(|a| encode_account(a.nonce, B256::from(a.balance.to_be_bytes())));
        (*hashed_addr, rec)
    }).collect();
    tree.commit(writes, block_number).expect("jmt commit")
}
