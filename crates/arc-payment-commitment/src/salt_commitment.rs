//! SALT (MegaETH) as the payment-lane state commitment.
//!
//! SALT is a two-tier authenticated store: a static 4-level 256-ary trie over dynamic
//! open-addressed buckets that are *Strongly History Independent* (SHI), committed with
//! homomorphic IPA/Pedersen vector commitments on bandersnatch. Its pitch is the axis our own
//! experiments kept losing on — ~1 GB for 3B keys and no random disk I/O for root updates — i.e.
//! the STORE, not the tree arithmetic. (Our standalone probe measured ~7 us per changed account,
//! 89% of it elliptic-curve work, so it does not win on raw root arithmetic against a keccak tree.)
//!
//! Why SHI matters here: it makes the bucket layout a pure function of the CURRENT CONTENTS,
//! independent of insertion order. That is exactly the property an insertion-ordered dense index
//! lacks, and lacking it is what forced the dense lane to position leaves by keccak(addr) instead
//! of assigning compact indices. SALT gets compactness AND order-independence.
//!
//! Two-stage API, mapped onto reth's two seams:
//!   read-only root  = state.update_fin(kvs) -> StateRoot::update_fin(state_updates) -> root
//!                     (neither call mutates the store; we discard both update sets)
//!   commit          = the same, then store.update_state(..) + store.update_trie(..)
//! Only the second stage computes a commitment — timing the first alone yields a number that is
//! NOT a state root.
use alloy_primitives::B256;
use hashbrown::HashMap;
use once_cell::sync::OnceCell;
use salt::{EphemeralSaltState, MemStore, StateRoot};
use std::sync::Mutex;

pub fn enabled() -> bool {
    std::env::var("ARC_PAYMENT_ROOT").map(|v| v == "salt").unwrap_or(false)
}

/// The committed base. `MemStore` is SALT's in-memory store (its design point: durability would
/// come from snapshotting, not per-block writes — so unlike the dense lane this pays no per-block
/// persistence, and any comparison must say so).
static STORE: OnceCell<MemStore> = OnceCell::new();
/// Serializes commit against read-only roots so a speculative root never observes a half-applied
/// commit. (Reth calls the root ~11x per block speculatively, plus validation and witness.)
static LOCK: Mutex<()> = Mutex::new(());

fn store() -> &'static MemStore {
    STORE.get_or_init(MemStore::new)
}

/// Runs the one-time seed exactly once per process.
static SEEDED: std::sync::Once = std::sync::Once::new();

/// Seed the committed base from reth's durable plain state (`HashedAccounts`) before any root is
/// computed. **This is a correctness requirement, not an optimization.**
///
/// Without it the commitment contains only accounts that have flowed through `write_hashed_state`
/// since this process started. Genesis-funded accounts are written at `init` and never appear
/// there, so the root would authenticate only *touched* accounts — a validator could not detect
/// tampering with an untouched one. The chain still runs (build and validate agree, both being
/// equally blind), which is exactly what makes the bug easy to miss.
///
/// It also gives durability without per-block disk writes: `HashedAccounts` is the authoritative,
/// MDBX-durable plain state, so re-seeding from it on startup reconstructs the full commitment.
/// That is why SALT's nodes do NOT need to be written to MDBX per block — doing so would recreate
/// the store-bound cost that erased the JMT lane's structural win.
///
/// Determinism: the seed set is the entire table, and SALT's buckets are Strongly History
/// Independent, so the resulting base is a pure function of the account set — independent of
/// iteration order and identical on every validator.
///
/// Cost: O(state), once per process, on the first root computation. For a large restored state
/// this is a one-time startup stall; a real client would run it during node init rather than
/// lazily inside the first block validation.
pub fn ensure_seeded<F>(load: F)
where
    F: FnOnce() -> Vec<(B256, Option<Vec<u8>>)>,
{
    SEEDED.call_once(|| {
        let all = load();
        if all.is_empty() {
            return;
        }
        let n = all.len();
        // `commit` takes LOCK itself; ensure_seeded is always called OUTSIDE the lock (never from
        // within readonly_root/commit) so this cannot deadlock.
        let root = commit(&all);
        if std::env::var("ARC_JMT_TRACE").is_ok() {
            eprintln!("SALT seeded {n} accounts from HashedAccounts, base root={root:#x}");
        }
    });
}

/// Encode an account leaf as (nonce, balance) — byte-identical to what the JMT and dense lanes
/// commit, so per-key work stays comparable across the three experiments.
pub fn encode_account_leaf(nonce: u64, balance: B256) -> Vec<u8> {
    let mut v = Vec::with_capacity(40);
    v.extend_from_slice(&nonce.to_be_bytes());
    v.extend_from_slice(&balance.0);
    v
}

fn to_kvs(changes: &[(B256, Option<Vec<u8>>)]) -> HashMap<Vec<u8>, Option<Vec<u8>>> {
    changes.iter().map(|(k, v)| (k.0.to_vec(), v.clone())).collect()
}

/// Root of committed-base + `changes`, WITHOUT persisting. Pure, so every speculative build,
/// validation, and witness re-execution agree.
pub fn readonly_root(changes: &[(B256, Option<Vec<u8>>)]) -> B256 {
    let _g = LOCK.lock().unwrap();
    let s = store();
    let mut state = EphemeralSaltState::new(s);
    let state_updates = match state.update_fin(&to_kvs(changes)) {
        Ok(u) => u,
        Err(e) => panic!("salt state update failed: {e:?}"),
    };
    let mut root = StateRoot::new(s);
    match root.update_fin(&state_updates) {
        // Discard both update sets — read-only.
        Ok((root_hash, _trie_updates)) => B256::from(root_hash),
        Err(e) => panic!("salt root update failed: {e:?}"),
    }
}

/// Advance the committed base by one canonical block. The sole writer.
pub fn commit(changes: &[(B256, Option<Vec<u8>>)]) -> B256 {
    let _g = LOCK.lock().unwrap();
    let s = store();
    let mut state = EphemeralSaltState::new(s);
    let state_updates = match state.update_fin(&to_kvs(changes)) {
        Ok(u) => u,
        Err(e) => panic!("salt state update failed: {e:?}"),
    };
    let mut root = StateRoot::new(s);
    let (root_hash, trie_updates) = match root.update_fin(&state_updates) {
        Ok(r) => r,
        Err(e) => panic!("salt root update failed: {e:?}"),
    };
    s.update_state(state_updates);
    s.update_trie(trie_updates);
    B256::from(root_hash)
}
