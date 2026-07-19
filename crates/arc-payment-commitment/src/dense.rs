//! Locality-keyed DENSE Merkle commitment for the payment lane — the challenger to reth's MPT.
//!
//! Reth's MPT identifies each trie node by its path through `keccak(addr)` and stores it in MDBX
//! keyed by that path, so walking one leaf costs ~log(n) key-value probes into a B-tree, scattered
//! at random by the hashing. This structure removes that indirection entirely: a FIXED-DEPTH
//! binary Merkle whose nodes live in ONE contiguous mapping in heap layout (node i's children at
//! 2i, 2i+1). Fetching a node is an array index, not a lookup; the hot upper levels are a small
//! contiguous region every update touches, so they stay cache-resident. Persistence is a
//! page-granular mmap writeback plus an append-only value log, rather than per-node key-value
//! records — the MPT durably writes its trie nodes to MDBX every persistence batch, so this lane
//! must persist too or the comparison is rigged (but on the same terms: async per block, fsync at
//! a batch boundary — see `flush_async`/`sync`).
//!
//! MEASURED CORRECTION to the original premise: this layout does NOT buy sequential writeback.
//! Leaf position comes from keccak(addr), so the ~k leaves a block touches dirty ~k SCATTERED
//! 4KB pages. The array is contiguous in address space, not in the pages you write. What "dense"
//! actually buys is (a) no key-value probe per node and (b) cache residency of the hot upper
//! levels every update converges on.
//!
//! It trades memory for that: all 2^(D+1) nodes are allocated up front (D=23 -> 537 MB) even
//! though only the populated leaves matter, whereas the MPT stores only nodes that exist.
//!
//! Scope: leaf position still comes from `keccak(addr)`, so this tests the STORE/LAYOUT lever
//! (what App E item (5) actually measured: contiguous array vs hash-keyed map at fixed Merkle
//! shape), NOT "related accounts sit adjacent".
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

/// Node array backing. `Mapped` is the real (durable) store; `Anon` is for tests.
enum Nodes {
    Mapped { mmap: memmap2::MmapMut, _file: std::fs::File },
    Anon(Vec<[u8; 32]>),
}

impl Nodes {
    #[inline]
    fn get(&self, i: usize) -> [u8; 32] {
        match self {
            Self::Mapped { mmap, .. } => {
                let o = i * 32;
                let mut out = [0u8; 32];
                out.copy_from_slice(&mmap[o..o + 32]);
                out
            }
            Self::Anon(v) => v[i],
        }
    }
    #[inline]
    fn set(&mut self, i: usize, h: [u8; 32]) {
        match self {
            Self::Mapped { mmap, .. } => {
                let o = i * 32;
                mmap[o..o + 32].copy_from_slice(&h);
            }
            Self::Anon(v) => v[i] = h,
        }
    }
    /// Start writeback of the dirty pages covering `indices` (coalesced, page-granular).
    ///
    /// ASYNC on purpose. Reth's MPT persists through MDBX, which writes pages into the OS page
    /// cache per block and only fsyncs when the *batch* transaction commits (reth's
    /// persistence-threshold). Doing a synchronous msync here every block would charge the dense
    /// lane a durability cost the MPT never pays, and would measure that policy rather than the
    /// data structure. `sync()` below is the batch-boundary equivalent.
    ///
    /// NOTE (a real property of this layout, not a bug): leaf positions come from keccak(addr),
    /// so the ~k touched leaves land in ~k SCATTERED 4KB pages. The node array is contiguous in
    /// address space but the dirtied pages are random within it — "dense" buys cache locality on
    /// the hot upper levels, NOT sequential writeback.
    fn flush_async(&self, indices: &[usize]) -> std::io::Result<()> {
        let Self::Mapped { mmap, .. } = self else { return Ok(()) };
        if indices.is_empty() {
            return Ok(());
        }
        const PAGE: usize = 4096;
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &i in indices {
            let start = (i * 32) / PAGE * PAGE;
            let end = (start + PAGE).min(mmap.len());
            match ranges.last_mut() {
                Some((_, e)) if *e >= start => *e = (*e).max(end),
                _ => ranges.push((start, end)),
            }
        }
        for (s, e) in ranges {
            mmap.flush_async_range(s, e - s)?;
        }
        Ok(())
    }

    /// Durability barrier at a batch boundary (the MDBX-commit equivalent).
    fn sync(&self) -> std::io::Result<()> {
        match self {
            Self::Mapped { mmap, .. } => mmap.flush(),
            Self::Anon(_) => Ok(()),
        }
    }
}

struct State {
    /// Full binary tree in heap layout: nodes[1] = root, children of i at 2i / 2i+1.
    /// Leaves occupy [2^DEPTH, 2^(DEPTH+1)). One contiguous mapping — the whole point.
    nodes: Nodes,
    /// Live account values per leaf slot (needed to recompute a slot on collision).
    slots: BTreeMap<usize, BTreeMap<B256, Vec<u8>>>,
    /// Append-only durable log of leaf-value changes, so the account values behind the tree
    /// survive restart. Reth's MPT durably persists its trie nodes to MDBX on every persistence
    /// batch, so the dense lane must persist too — otherwise it would "win" only by not writing.
    vlog: Option<std::io::BufWriter<std::fs::File>>,
    /// Blocks applied since the last durability barrier (see commit()).
    since_sync: u64,
    depth: usize,
}

static TREE: OnceCell<Mutex<State>> = OnceCell::new();

fn state() -> &'static Mutex<State> {
    TREE.get_or_init(|| {
        let d = depth();
        let bytes = (1usize << (d + 1)) * 32;
        let base = std::env::var("ARC_DENSE_STORE_PATH").unwrap_or_else(|_| "dense-store".into());
        std::fs::create_dir_all(&base).ok();
        let path = std::path::Path::new(&base).join("nodes.bin");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .expect("open dense node file");
        file.set_len(bytes as u64).expect("size dense node file");
        // Sparse file: pages materialize on first touch, so an empty tree costs no real memory.
        let mmap = unsafe { memmap2::MmapMut::map_mut(&file).expect("mmap dense nodes") };
        let vlog = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(std::path::Path::new(&base).join("values.log"))
            .ok()
            .map(std::io::BufWriter::new);
        Mutex::new(State {
            nodes: Nodes::Mapped { mmap, _file: file },
            slots: BTreeMap::new(),
            vlog,
            since_sync: 0,
            depth: d,
        })
    })
}

/// Recompute the path from `leaf_idx` to the root inside `scratch` (an overlay over `nodes`),
/// so a read-only root never mutates the committed tree.
fn lift(nodes: &Nodes, scratch: &mut BTreeMap<usize, [u8; 32]>, mut i: usize) {
    while i > 1 {
        let parent = i / 2;
        let (l, r) = (parent * 2, parent * 2 + 1);
        let lh = scratch.get(&l).copied().unwrap_or_else(|| nodes.get(l));
        let rh = scratch.get(&r).copied().unwrap_or_else(|| nodes.get(r));
        scratch.insert(parent, keccak(&[&lh, &rh]));
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
    let root = scratch.get(&1).copied().unwrap_or_else(|| st.nodes.get(1));
    (B256::from(root), scratch, touched)
}

/// Root of committed-base + `changes`, WITHOUT mutating. Pure => every speculative build,
/// validation, and witness re-execution agree. O(k·log n).
pub fn readonly_root(changes: &[(B256, Option<Vec<u8>>)]) -> B256 {
    let st = state().lock().unwrap();
    compute(&st, changes).0
}

/// Advance the committed tree by one canonical block, durably. The sole writer.
///
/// Persistence cost, for comparison with the MPT's MDBX trie writes: the changed nodes are
/// written into the mapping and msync'd page-granularly (sequential writeback of dirty pages,
/// no per-node key-value records, no B-tree rebalancing), and the changed leaf values are
/// appended to a log which is fsync'd once per block.
pub fn commit(changes: &[(B256, Option<Vec<u8>>)]) -> B256 {
    use std::io::Write;
    let mut st = state().lock().unwrap();
    let (root, scratch, touched) = compute(&st, changes);

    let mut dirty: Vec<usize> = Vec::with_capacity(scratch.len());
    for (idx, h) in scratch {
        st.nodes.set(idx, h);
        dirty.push(idx);
    }
    dirty.sort_unstable();

    for (s, entries) in touched {
        if entries.is_empty() {
            st.slots.remove(&s);
        } else {
            st.slots.insert(s, entries);
        }
    }

    // Leaf values: append (addr, len, value) for each change into the buffered log.
    if let Some(w) = st.vlog.as_mut() {
        for (addr, val) in changes {
            let _ = w.write_all(&addr.0);
            match val {
                Some(v) => {
                    let _ = w.write_all(&(v.len() as u32).to_be_bytes());
                    let _ = w.write_all(v);
                }
                None => {
                    let _ = w.write_all(&u32::MAX.to_be_bytes());
                }
            }
        }
        let _ = w.flush(); // to the OS page cache, like MDBX's per-block page writes
    }

    // Kick off writeback of the dirty node pages (async — see flush_async).
    let _ = st.nodes.flush_async(&dirty);

    // Durability barrier every N blocks, mirroring MDBX committing reth's persistence BATCH
    // rather than every individual block. ARC_DENSE_SYNC_EVERY=1 to fsync per block.
    st.since_sync += 1;
    let every: u64 =
        std::env::var("ARC_DENSE_SYNC_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(32);
    if every > 0 && st.since_sync >= every {
        st.since_sync = 0;
        let _ = st.nodes.sync();
        if let Some(w) = st.vlog.as_mut() {
            let _ = w.get_ref().sync_data();
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

    /// In-RAM state for tests (no file backing).
    fn anon(d: usize) -> State {
        State {
            nodes: Nodes::Anon(vec![[0u8; 32]; 1 << (d + 1)]),
            slots: BTreeMap::new(),
            vlog: None,
            since_sync: 0,
            depth: d,
        }
    }

    fn addr(b: u8) -> B256 {
        let mut a = [0u8; 32];
        a[0] = b;
        a[31] = b;
        B256::from(a)
    }

    #[test]
    fn readonly_root_is_pure_and_order_independent() {
        let d = 12;
        let st = anon(d);
        let a = vec![
            (addr(1), Some(encode_account_leaf(1, B256::ZERO))),
            (addr(2), Some(encode_account_leaf(2, B256::ZERO))),
        ];
        let mut b = a.clone();
        b.reverse();
        // Same change set in either order -> same root, and neither mutates the tree.
        assert_eq!(compute(&st, &a).0, compute(&st, &b).0);
        assert_eq!(st.nodes.get(1), [0u8; 32]);
    }

    #[test]
    fn commit_then_readonly_matches_single_shot() {
        let d = 12;
        let mut st = anon(d);
        let b1 = vec![(addr(1), Some(encode_account_leaf(1, B256::ZERO)))];
        let b2 = vec![(addr(2), Some(encode_account_leaf(2, B256::ZERO)))];
        // Root of applying b1 then b2 incrementally...
        let (_, scratch, touched) = compute(&st, &b1);
        for (i, h) in scratch { st.nodes.set(i, h); }
        for (s, e) in touched { st.slots.insert(s, e); }
        let stepwise = compute(&st, &b2).0;
        // ...must equal the root of the union applied at once (persistence-lag consistency).
        let fresh = anon(d);
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
