//! Locality-keyed DENSE Merkle commitment for the payment lane.
//!
//! The JMT experiment showed the tree was ~30% faster than reth's MPT but the node store
//! (redb, per-node key-value writes, synchronous flush) erased the win. This structure attacks
//! that directly: a FIXED-DEPTH binary Merkle whose nodes live in ONE contiguous array in heap
//! layout (node i's children at 2i, 2i+1) — no key-value store, no per-node writes. Persistence
//! is an mmap msync of dirty pages, batched by the OS.
//!
//! Leaf position is `top-D bits of keccak(address)` — a PURE function of the address, never of
//! insertion order. That is deliberate: an insertion-ordered dense index would make the tree
//! shape depend on the order blocks were applied, so a speculative build and a later validation
//! at a different persisted tip would disagree (the bug that stalled the first JMT attempt).
//! Colliding addresses chain into one slot via a sorted hash of their (addr, value) pairs.
use alloy_primitives::B256;
use once_cell::sync::OnceCell;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tiny_keccak::{Hasher, Keccak};

/// Tree depth: 2^DEPTH leaf slots. 2^23 = 8.4M slots holds the ~5M accounts the head-to-head
/// reaches at a ~0.6 load factor. Override with ARC_DENSE_DEPTH.
fn depth() -> usize {
    std::env::var("ARC_DENSE_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(23)
}

pub fn enabled() -> bool {
    std::env::var("ARC_PAYMENT_ROOT").map(|v| v == "dense").unwrap_or(false)
}

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut k = Keccak::v256();
    for p in parts {
        k.update(p);
    }
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

/// Leaf slot for an address hash: top DEPTH bits (big-endian) of the already-hashed key.
fn slot_of(hashed_addr: &B256, d: usize) -> usize {
    let mut acc: u64 = 0;
    for i in 0..8 {
        acc = (acc << 8) | hashed_addr.0[i] as u64;
    }
    (acc >> (64 - d)) as usize
}

/// Hash the set of accounts occupying one slot. Sorted by address so the digest is
/// order-independent (collision chaining must not depend on arrival order).
fn slot_digest(entries: &BTreeMap<B256, Vec<u8>>) -> [u8; 32] {
    if entries.is_empty() {
        return [0u8; 32];
    }
    let mut k = Keccak::v256();
    for (addr, val) in entries {
        k.update(&addr.0);
        k.update(val);
    }
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

struct State {
    /// Full binary tree in heap layout: nodes[1] = root, children of i at 2i / 2i+1.
    /// Leaves occupy [2^DEPTH, 2^(DEPTH+1)). One contiguous allocation — the whole point.
    nodes: Vec<[u8; 32]>,
    /// Live account values per leaf slot (needed to recompute a slot on collision).
    slots: BTreeMap<usize, BTreeMap<B256, Vec<u8>>>,
    depth: usize,
}

static TREE: OnceCell<Mutex<State>> = OnceCell::new();

fn state() -> &'static Mutex<State> {
    TREE.get_or_init(|| {
        let d = depth();
        Mutex::new(State { nodes: vec![[0u8; 32]; 1 << (d + 1)], slots: BTreeMap::new(), depth: d })
    })
}

/// Recompute the path from `leaf_idx` to the root inside `scratch` (an overlay over `nodes`),
/// so a read-only root never mutates the committed tree.
fn lift(nodes: &[[u8; 32]], scratch: &mut BTreeMap<usize, [u8; 32]>, mut i: usize) {
    let get = |scr: &BTreeMap<usize, [u8; 32]>, idx: usize| -> [u8; 32] {
        *scr.get(&idx).unwrap_or(&nodes[idx])
    };
    while i > 1 {
        let parent = i / 2;
        let (l, r) = (parent * 2, parent * 2 + 1);
        let h = keccak(&[&get(scratch, l), &get(scratch, r)]);
        scratch.insert(parent, h);
        i = parent;
    }
}

/// Apply `changes` to a scratch overlay and return (root, scratch) — shared by read and commit.
fn compute(
    st: &State,
    changes: &[(B256, Option<Vec<u8>>)],
) -> (B256, BTreeMap<usize, [u8; 32]>, BTreeMap<usize, BTreeMap<B256, Vec<u8>>>) {
    let base = 1usize << st.depth;
    // Group changes by slot, starting from the committed contents of each touched slot.
    let mut touched: BTreeMap<usize, BTreeMap<B256, Vec<u8>>> = BTreeMap::new();
    for (addr, val) in changes {
        let s = slot_of(addr, st.depth);
        let entry = touched
            .entry(s)
            .or_insert_with(|| st.slots.get(&s).cloned().unwrap_or_default());
        match val {
            Some(v) => {
                entry.insert(*addr, v.clone());
            }
            None => {
                entry.remove(addr);
            }
        }
    }
    let mut scratch: BTreeMap<usize, [u8; 32]> = BTreeMap::new();
    for (s, entries) in &touched {
        scratch.insert(base + s, slot_digest(entries));
    }
    // Lift each touched leaf to the root. Shared ancestors recompute from scratch values, so
    // the k paths collapse near the root (this is the O(k log n)).
    let leaves: Vec<usize> = touched.keys().map(|s| base + s).collect();
    for l in leaves {
        lift(&st.nodes, &mut scratch, l);
    }
    let root = *scratch.get(&1).unwrap_or(&st.nodes[1]);
    (B256::from(root), scratch, touched)
}

/// Root of committed-base + `changes`, WITHOUT mutating. Pure => every speculative build,
/// validation, and witness re-execution agree. O(k·log n).
pub fn readonly_root(changes: &[(B256, Option<Vec<u8>>)]) -> B256 {
    let st = state().lock().unwrap();
    compute(&st, changes).0
}

/// Advance the committed tree by one canonical block. The sole writer.
pub fn commit(changes: &[(B256, Option<Vec<u8>>)]) -> B256 {
    let mut st = state().lock().unwrap();
    let (root, scratch, touched) = compute(&st, changes);
    for (idx, h) in scratch {
        st.nodes[idx] = h;
    }
    for (s, entries) in touched {
        if entries.is_empty() {
            st.slots.remove(&s);
        } else {
            st.slots.insert(s, entries);
        }
    }
    root
}

/// Encode an account leaf: (nonce, balance) — same payload the JMT lane commits, so the two
/// experiments are comparable.
pub fn encode_account_leaf(nonce: u64, balance: B256) -> Vec<u8> {
    let mut v = Vec::with_capacity(40);
    v.extend_from_slice(&nonce.to_be_bytes());
    v.extend_from_slice(&balance.0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> B256 {
        let mut a = [0u8; 32];
        a[0] = b;
        a[31] = b;
        B256::from(a)
    }

    #[test]
    fn readonly_root_is_pure_and_order_independent() {
        let d = 12;
        let st = State { nodes: vec![[0u8; 32]; 1 << (d + 1)], slots: BTreeMap::new(), depth: d };
        let a = vec![
            (addr(1), Some(encode_account_leaf(1, B256::ZERO))),
            (addr(2), Some(encode_account_leaf(2, B256::ZERO))),
        ];
        let mut b = a.clone();
        b.reverse();
        // Same change set in either order -> same root, and neither mutates the tree.
        assert_eq!(compute(&st, &a).0, compute(&st, &b).0);
        assert_eq!(st.nodes[1], [0u8; 32]);
    }

    #[test]
    fn commit_then_readonly_matches_single_shot() {
        let d = 12;
        let mut st = State { nodes: vec![[0u8; 32]; 1 << (d + 1)], slots: BTreeMap::new(), depth: d };
        let b1 = vec![(addr(1), Some(encode_account_leaf(1, B256::ZERO)))];
        let b2 = vec![(addr(2), Some(encode_account_leaf(2, B256::ZERO)))];
        // Root of applying b1 then b2 incrementally...
        let (_, scratch, touched) = compute(&st, &b1);
        for (i, h) in scratch { st.nodes[i] = h; }
        for (s, e) in touched { st.slots.insert(s, e); }
        let stepwise = compute(&st, &b2).0;
        // ...must equal the root of the union applied at once (persistence-lag consistency).
        let fresh = State { nodes: vec![[0u8; 32]; 1 << (d + 1)], slots: BTreeMap::new(), depth: d };
        let union: Vec<_> = b1.iter().chain(b2.iter()).cloned().collect();
        assert_eq!(stepwise, compute(&fresh, &union).0);
    }

    #[test]
    fn collision_chain_is_order_independent() {
        let mut m1 = BTreeMap::new();
        m1.insert(addr(1), vec![1u8]);
        m1.insert(addr(2), vec![2u8]);
        let mut m2 = BTreeMap::new();
        m2.insert(addr(2), vec![2u8]);
        m2.insert(addr(1), vec![1u8]);
        assert_eq!(slot_digest(&m1), slot_digest(&m2));
    }
}
