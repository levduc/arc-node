// Hash-only UTXO commitment: an incremental in-RAM Merkle accumulator.
//
// Motivation: ECMH (a multiset hash) is O(1)/update but rests on the discrete-log
// assumption. The hash-only additive multiset hash (sum of H(utxo) mod 2^256) drops
// DLog but is broken by Wagner's k-sum attack in an adversarial setting. So a
// SECURE hash-only commitment needs a Merkle structure (collision-resistance only).
//
// We keep a dense binary Merkle tree of fixed capacity in RAM. Each UTXO occupies a
// leaf slot (freed on spend); create/spend update the leaf and recompute the path to
// the root -- O(depth) = O(log capacity) hashes, i.e. a CONSTANT, so per-block commit
// cost is flat in the current set size. The commitment is the root (32 bytes), it is
// O(1) to read, and it yields inclusion proofs. Security: keccak collision-resistance,
// no DLog, post-quantum.

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
    #[inline]
    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64)) as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct OutPoint {
    txid: [u8; 32],
    vout: u32,
}
#[derive(Clone, Copy)]
struct Output {
    owner: [u8; 20],
    amount: u64,
    slot: u32,
}

#[inline]
fn utxo_leaf(op: &OutPoint, out: &Output) -> B256 {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&op.txid);
    b[32..36].copy_from_slice(&op.vout.to_le_bytes());
    b[36..56].copy_from_slice(&out.owner);
    b[56..64].copy_from_slice(&out.amount.to_le_bytes());
    keccak256(b)
}

// dense binary Merkle tree, 1-indexed heap; leaves at [cap, 2*cap)
struct Merkle {
    cap: usize,
    depth: u32,
    tree: Vec<B256>,
}
impl Merkle {
    fn new(depth: u32) -> Self {
        let cap = 1usize << depth;
        // precompute empty-subtree hash per level (0 = empty leaf)
        let mut empty = vec![B256::ZERO; depth as usize + 1];
        for h in 1..=depth as usize {
            let mut b = [0u8; 64];
            b[..32].copy_from_slice(empty[h - 1].as_slice());
            b[32..].copy_from_slice(empty[h - 1].as_slice());
            empty[h] = keccak256(b);
        }
        // fill the whole tree with the all-empty configuration in O(cap), no per-node hashing
        let mut tree = vec![B256::ZERO; 2 * cap];
        for i in cap..2 * cap {
            tree[i] = empty[0];
        }
        for h in 1..=depth as usize {
            let s = cap >> h;
            let e = cap >> (h - 1);
            for i in s..e {
                tree[i] = empty[h];
            }
        }
        Merkle { cap, depth, tree }
    }
    #[inline]
    fn set(&mut self, slot: usize, val: B256) {
        let mut idx = self.cap + slot;
        self.tree[idx] = val;
        idx >>= 1;
        while idx >= 1 {
            let mut b = [0u8; 64];
            b[..32].copy_from_slice(self.tree[2 * idx].as_slice());
            b[32..].copy_from_slice(self.tree[2 * idx + 1].as_slice());
            self.tree[idx] = keccak256(b);
            idx >>= 1;
        }
    }
    #[inline]
    fn root(&self) -> B256 {
        self.tree[1]
    }
}

fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let r: u64 = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
    r * 4096
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let depth: u32 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(25); // cap = 33.5M slots
    let max_set: usize = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(25_000_000);
    let block_txs = 10_000usize;

    eprintln!("building Merkle of capacity 2^{depth} = {} slots ...", 1usize << depth);
    let mut m = Merkle::new(depth);
    let mut set: HashMap<OutPoint, Output> = HashMap::new();
    let mut keys: Vec<OutPoint> = Vec::new();
    let mut free: Vec<u32> = Vec::new();
    let mut next_slot: u32 = 0;
    let mut rng = Rng(0xABCDEF);
    let mut nonce: u64 = 0;

    let mut create = |set: &mut HashMap<OutPoint, Output>,
                      keys: &mut Vec<OutPoint>,
                      free: &mut Vec<u32>,
                      next_slot: &mut u32,
                      m: &mut Merkle,
                      rng: &mut Rng,
                      nonce: &mut u64| {
        let mut txid = [0u8; 32];
        txid[..8].copy_from_slice(&nonce.to_le_bytes());
        txid[8..16].copy_from_slice(&rng.next().to_le_bytes());
        *nonce += 1;
        let slot = free.pop().unwrap_or_else(|| {
            let s = *next_slot;
            *next_slot += 1;
            s
        });
        let op = OutPoint { txid, vout: 0 };
        let mut owner = [0u8; 20];
        owner[..8].copy_from_slice(&rng.next().to_le_bytes());
        let out = Output { owner, amount: rng.next() % 1_000_000, slot };
        m.set(slot as usize, utxo_leaf(&op, &out));
        set.insert(op, out);
        keys.push(op);
    };

    for _ in 0..1000 {
        create(&mut set, &mut keys, &mut free, &mut next_slot, &mut m, &mut rng, &mut nonce);
    }

    println!(
        "{:>12} | {:>9} | {:>7} | {:>16} | {:>14}",
        "utxos", "RAM(GB)", "B/utxo", "iMerkle/blk(ms)", "us/update"
    );
    println!("{}", "-".repeat(72));

    let checkpoints = [100_000usize, 1_000_000, 5_000_000, 10_000_000, 25_000_000];
    let mut cp = 0;

    while set.len() < max_set {
        let t0 = Instant::now();
        let mut updates = 0u64;
        for _ in 0..block_txs {
            // spend 1
            if !keys.is_empty() {
                let idx = rng.below(keys.len());
                let op = keys.swap_remove(idx);
                if let Some(out) = set.remove(&op) {
                    m.set(out.slot as usize, B256::ZERO);
                    free.push(out.slot);
                    updates += 1;
                }
            }
            // create 2
            for _ in 0..2 {
                create(&mut set, &mut keys, &mut free, &mut next_slot, &mut m, &mut rng, &mut nonce);
                updates += 1;
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;

        if cp < checkpoints.len() && set.len() >= checkpoints[cp] {
            let ram = rss_bytes();
            println!(
                "{:>12} | {:>9.2} | {:>7} | {:>16.1} | {:>14.2}",
                set.len(),
                ram as f64 / 1e9,
                ram / set.len() as u64,
                ms,
                ms * 1000.0 / updates as f64
            );
            cp += 1;
        }
    }

    println!("{}", "-".repeat(72));
    println!("root (32 bytes, O(1) to read, hash-only): {}", m.root());
    println!(
        "depth {} -> {} hashes/update (constant) => per-block commit is FLAT in set size; collision-resistance only, no DLog.",
        depth, depth
    );
}
