// Does locality-keyed beat hash-keyed for a state+commitment structure?
//
// Two authenticated structures of the SAME shape (binary Merkle, same #hashes per update),
// differing ONLY in how nodes are stored/addressed:
//   * dense  : contiguous Vec, node found by direct index (a locality-keyed structure)
//   * sparse : HashMap<node_index, hash>, only non-empty nodes stored (an MPT-style
//              hash-keyed store: random access + hashing to find each node)
//
// Same leaves, same roots. We measure UPDATE throughput as the populated set grows, to see
// whether the hash-keyed store degrades (cache misses + map overhead) while the dense one
// holds. This isolates the cost the MPT pays for being a general hash-keyed trie.

use std::collections::HashMap;
use std::time::Instant;

use alloy_primitives::{keccak256, B256};

struct Rng(u64);
impl Rng {
    #[inline]
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

#[inline]
fn h2(a: &B256, b: &B256) -> B256 {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(a.as_slice());
    x[32..].copy_from_slice(b.as_slice());
    keccak256(x)
}

fn empties(depth: u32) -> Vec<B256> {
    let mut e = vec![B256::ZERO; depth as usize + 1];
    for h in 1..=depth as usize {
        e[h] = h2(&e[h - 1], &e[h - 1]);
    }
    e
}

// dense contiguous Merkle (locality-keyed)
struct Dense {
    cap: usize,
    tree: Vec<B256>,
}
impl Dense {
    fn new(depth: u32, e: &[B256]) -> Self {
        let cap = 1usize << depth;
        let mut tree = vec![B256::ZERO; 2 * cap];
        for h in 1..=depth as usize {
            for i in (cap >> h)..(cap >> (h - 1)) {
                tree[i] = e[h];
            }
        }
        Dense { cap, tree }
    }
    #[inline]
    fn set(&mut self, slot: usize, val: B256) {
        let mut idx = self.cap + slot;
        self.tree[idx] = val;
        while idx > 1 {
            let p = idx >> 1;
            self.tree[p] = h2(&self.tree[2 * p], &self.tree[2 * p + 1]);
            idx = p;
        }
    }
    fn root(&self) -> B256 {
        self.tree[1]
    }
}

// sparse hash-keyed Merkle (MPT-style: only non-empty nodes, random-access store)
struct Sparse {
    cap: usize,
    depth: u32,
    map: HashMap<usize, B256>,
    e: Vec<B256>,
}
impl Sparse {
    fn new(depth: u32, e: &[B256]) -> Self {
        Sparse { cap: 1usize << depth, depth, map: HashMap::new(), e: e.to_vec() }
    }
    #[inline]
    fn get(&self, idx: usize, level: usize) -> B256 {
        *self.map.get(&idx).unwrap_or(&self.e[level])
    }
    #[inline]
    fn set(&mut self, slot: usize, val: B256) {
        let mut idx = self.cap + slot;
        if val == self.e[0] {
            self.map.remove(&idx);
        } else {
            self.map.insert(idx, val);
        }
        let mut level = 0usize;
        while idx > 1 {
            let sib = idx ^ 1;
            let lv = self.get(idx & !1, level);
            let rv = self.get((idx & !1) + 1, level);
            let _ = sib;
            let h = h2(&lv, &rv);
            let p = idx >> 1;
            level += 1;
            if h == self.e[level] {
                self.map.remove(&p);
            } else {
                self.map.insert(p, h);
            }
            idx = p;
        }
    }
    fn root(&self) -> B256 {
        self.get(1, self.depth as usize)
    }
}

fn main() {
    let depth = 24u32; // cap 16.7M leaves
    let cap = 1usize << depth;
    let e = empties(depth);
    let probe = 200_000usize; // timed random updates at each checkpoint
    // populate to these sizes, then measure update throughput on the populated set
    let sizes = [100_000usize, 1_000_000, 4_000_000, 12_000_000];

    println!("\nUpdate throughput vs populated state size (depth {depth}, {probe} random updates timed)\n");
    println!(
        "{:>12} | {:>16} | {:>16} | {:>9}",
        "populated", "dense (upd/s)", "sparse (upd/s)", "dense x"
    );
    println!("{}", "-".repeat(64));

    let mut dense = Dense::new(depth, &e);
    let mut sparse = Sparse::new(depth, &e);
    let mut rng = Rng(0x1234);
    let mut populated = 0usize;

    for &target in &sizes {
        // populate up to `target` distinct leaves in both
        while populated < target {
            let v = keccak256((populated as u64).to_le_bytes());
            dense.set(populated, v);
            sparse.set(populated, v);
            populated += 1;
        }
        assert_eq!(dense.root(), sparse.root(), "roots must match");

        // build the SAME probe updates and apply to both (keeps them in sync)
        let mut r = Rng(rng.next());
        let ups: Vec<(usize, B256)> = (0..probe)
            .map(|_| ((r.next() as usize) % populated, keccak256(r.next().to_le_bytes())))
            .collect();
        let t = Instant::now();
        for &(slot, v) in &ups {
            dense.set(slot, v);
        }
        let td = t.elapsed().as_secs_f64();
        let t = Instant::now();
        for &(slot, v) in &ups {
            sparse.set(slot, v);
        }
        let ts = t.elapsed().as_secs_f64();
        assert_eq!(dense.root(), sparse.root(), "roots must match after probe");

        println!(
            "{:>12} | {:>16.0} | {:>16.0} | {:>8.1}x",
            target,
            probe as f64 / td,
            probe as f64 / ts,
            ts / td
        );
        let _ = cap;
    }
    println!("{}", "-".repeat(64));
    println!("Same tree shape + same hashes/update; only the node store differs.");
    println!("dense = contiguous array (locality-keyed); sparse = HashMap by node index (MPT-style).");
    println!("The MPT also keys by keccak(addr), so even sequential account writes hit random nodes.");
}
