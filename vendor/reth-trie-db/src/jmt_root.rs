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
use redb::{Database, ReadableTable, TableDefinition};
use reth_trie_common::HashedPostStateSorted;

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

/// True when the payment lane should commit with a JMT root instead of the MPT.
pub fn jmt_enabled() -> bool {
    std::env::var("ARC_PAYMENT_ROOT").map(|v| v == "jmt").unwrap_or(false)
}

/// Stateless JMT lane root: build a FRESH 16-shard JMT over the FULL account set and return
/// its root. Because it is a pure function of the account set handed in (which the caller derives
/// from `tx` + this block's overlay), it is identical across reth's build / validate / witness
/// contexts — fixing the context-dependent-base bug of the earlier incremental store.
/// Cost is O(state) per call (naive); the incremental sharded store (parity speed) needs a
/// commit-lifecycle and is the documented follow-up.
pub fn jmt_stateless_root(writes: Vec<(B256, Option<OwnedValue>)>) -> B256 {
    let n = writes.len();
    let tree = arc_payment_commitment::in_memory();
    let root = tree.commit(writes, 0).expect("jmt stateless commit");
    if std::env::var("ARC_JMT_TRACE").is_ok() {
        eprintln!("JMT accounts={} root={:#x}", n, root);
    }
    root
}

/// Encode a hashed-account record (nonce, balance) for the JMT leaf.
pub fn encode_acct(nonce: u64, balance_be: [u8; 32]) -> OwnedValue {
    encode_account(nonce, B256::from(balance_be))
}

/// True when ANY alternative payment-lane commitment replaces the MPT (jmt | dense).
pub fn alt_enabled() -> bool {
    #[cfg(feature = "salt-commitment")]
    if arc_payment_commitment::salt_commitment::enabled() {
        return true;
    }
    jmt_enabled() || arc_payment_commitment::dense::enabled()
}
