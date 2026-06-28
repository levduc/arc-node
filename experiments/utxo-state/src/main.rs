// UTXO-state experiment for the Arc payment-lane thesis.
//
// Question: as transaction volume grows, can a UTXO payment state (a) stay in RAM,
// and (b) be committed cheaply, vs. the account+MPT model whose state root is the
// disk-bound bottleneck we measured on Arc (27.8 ms cold).
//
// We keep an UNSPENT-output set in RAM and, per block, compare three ways to commit it:
//   (1) MPT root          -- the EVM baseline (alloy-trie, the same MPT reth uses). O(n).
//   (2) sorted-Merkle root -- a simple binary Merkle over the set. O(n).
//   (3) ECMH accumulator  -- elliptic-curve multiset hash: create = add a point,
//                            spend = subtract a point. O(updates/block), n-INDEPENDENT,
//                            and the current commitment is O(1) to read. This is the
//                            "simple way of committing state".
//
// We report, vs. UTXO-set size: resident RAM, bytes/UTXO, and per-commit cost.

use std::collections::HashMap;
use std::time::Instant;

use alloy_primitives::{keccak256, B256};
use alloy_trie::{HashBuilder, Nibbles};
use curve25519_dalek_ng::ristretto::RistrettoPoint;
use sha2::Sha512;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct OutPoint {
    txid: [u8; 32],
    vout: u32,
}

#[derive(Clone, Copy)]
struct Output {
    owner: [u8; 20],
    amount: u64,
}

// tiny deterministic PRNG (xorshift64*) -- no external rng dep
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

#[inline]
fn op_bytes(op: &OutPoint) -> [u8; 36] {
    let mut b = [0u8; 36];
    b[..32].copy_from_slice(&op.txid);
    b[32..].copy_from_slice(&op.vout.to_le_bytes());
    b
}

#[inline]
fn utxo_bytes(op: &OutPoint, out: &Output) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[..36].copy_from_slice(&op_bytes(op));
    b[36..56].copy_from_slice(&out.owner);
    b[56..64].copy_from_slice(&out.amount.to_le_bytes());
    b
}

// map a UTXO to a curve point for the multiset-hash accumulator
#[inline]
fn ecmh_point(op: &OutPoint, out: &Output) -> RistrettoPoint {
    RistrettoPoint::hash_from_bytes::<Sha512>(&utxo_bytes(op, out))
}

fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let resident: u64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    resident * 4096
}

// (2) sorted-Merkle root over all UTXO leaf hashes -- O(n), full rebuild
fn merkle_root(set: &HashMap<OutPoint, Output>) -> B256 {
    if set.is_empty() {
        return B256::ZERO;
    }
    let mut level: Vec<B256> = set
        .iter()
        .map(|(op, out)| keccak256(utxo_bytes(op, out)))
        .collect();
    level.sort_unstable();
    while level.len() > 1 {
        let mut next = Vec::with_capacity((level.len() + 1) / 2);
        let mut i = 0;
        while i < level.len() {
            if i + 1 < level.len() {
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(level[i].as_slice());
                buf[32..].copy_from_slice(level[i + 1].as_slice());
                next.push(keccak256(buf));
            } else {
                next.push(level[i]);
            }
            i += 2;
        }
        level = next;
    }
    level[0]
}

// (1) MPT root over the UTXO set via alloy-trie (the EVM-native trie) -- O(n), full rebuild
fn mpt_root(set: &HashMap<OutPoint, Output>) -> B256 {
    let mut leaves: Vec<(B256, [u8; 28])> = Vec::with_capacity(set.len());
    for (op, out) in set.iter() {
        let key = keccak256(op_bytes(op));
        let mut val = [0u8; 28];
        val[..20].copy_from_slice(&out.owner);
        val[20..].copy_from_slice(&out.amount.to_le_bytes());
        leaves.push((key, val));
    }
    leaves.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut hb = HashBuilder::default();
    for (key, val) in &leaves {
        hb.add_leaf(Nibbles::unpack(key.as_slice()), val.as_slice());
    }
    hb.root()
}

fn rand_output(rng: &mut Rng) -> Output {
    let mut owner = [0u8; 20];
    let r = rng.next().to_le_bytes();
    owner[..8].copy_from_slice(&r);
    Output {
        owner,
        amount: rng.next() % 1_000_000,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let max_set: usize = args
        .get(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(25_000_000);
    // tree-based full roots are O(n); only measure them up to a feasible size
    let tree_cap: usize = args
        .get(2)
        .and_then(|x| x.parse().ok())
        .unwrap_or(5_000_000);
    let block_txs: usize = 10_000;
    let checkpoints: Vec<usize> = vec![
        100_000, 500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 25_000_000,
    ];

    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut set: HashMap<OutPoint, Output> = HashMap::new();
    let mut keys: Vec<OutPoint> = Vec::new();
    let mut acc = RistrettoPoint::default(); // group identity
    let mut nonce: u64 = 0;

    // genesis UTXOs
    for _ in 0..1000 {
        let mut txid = [0u8; 32];
        txid[..8].copy_from_slice(&nonce.to_le_bytes());
        nonce += 1;
        let op = OutPoint { txid, vout: 0 };
        let out = rand_output(&mut rng);
        acc += ecmh_point(&op, &out);
        set.insert(op, out);
        keys.push(op);
    }

    println!(
        "{:>12} | {:>9} | {:>7} | {:>15} | {:>12} | {:>12}",
        "utxos", "RAM(GB)", "B/utxo", "ECMH/blk(ms)", "Merkle(ms)", "MPT(ms)"
    );
    println!("{}", "-".repeat(80));

    let mut cp_idx = 0;
    let mut last_ecmh_ms = 0.0f64;

    while set.len() < max_set {
        // ---- process one block; ECMH commitment is maintained incrementally ----
        let t0 = Instant::now();
        for _ in 0..block_txs {
            // spend one existing UTXO
            if !keys.is_empty() {
                let idx = rng.below(keys.len());
                let op = keys.swap_remove(idx);
                if let Some(out) = set.remove(&op) {
                    acc -= ecmh_point(&op, &out);
                }
            }
            // create two new UTXOs (net +1 per tx -> set grows ~ with tx count)
            let mut txid = [0u8; 32];
            txid[..8].copy_from_slice(&nonce.to_le_bytes());
            txid[8..16].copy_from_slice(&rng.next().to_le_bytes());
            nonce += 1;
            for vout in 0..2u32 {
                let op = OutPoint { txid, vout };
                let out = rand_output(&mut rng);
                acc += ecmh_point(&op, &out);
                set.insert(op, out);
                keys.push(op);
            }
        }
        last_ecmh_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // ---- checkpoint reporting ----
        if cp_idx < checkpoints.len() && set.len() >= checkpoints[cp_idx] {
            let n = set.len();
            let ram = rss_bytes();
            let (merkle_ms, mpt_ms) = if n <= tree_cap {
                let t = Instant::now();
                let _ = merkle_root(&set);
                let m = t.elapsed().as_secs_f64() * 1000.0;
                let t = Instant::now();
                let _ = mpt_root(&set);
                let p = t.elapsed().as_secs_f64() * 1000.0;
                (m, p)
            } else {
                (f64::NAN, f64::NAN)
            };
            let merkle_s = if merkle_ms.is_nan() {
                "  (skipped)".to_string()
            } else {
                format!("{:.0}", merkle_ms)
            };
            let mpt_s = if mpt_ms.is_nan() {
                "  (skipped)".to_string()
            } else {
                format!("{:.0}", mpt_ms)
            };
            println!(
                "{:>12} | {:>9.2} | {:>7} | {:>15.1} | {:>12} | {:>12}",
                n,
                ram as f64 / 1e9,
                ram / n as u64,
                last_ecmh_ms,
                merkle_s,
                mpt_s
            );
            cp_idx += 1;
        }
    }

    println!("{}", "-".repeat(80));
    println!(
        "final commitment (ECMH, O(1) to read): 0x{}",
        hex::encode(acc.compress().to_bytes())
    );
    println!(
        "block = {} txs (spend 1, create 2). ECMH/blk is n-independent; Merkle/MPT full roots are O(n).",
        block_txs
    );
}

// minimal hex (avoid an extra dep)
mod hex {
    pub fn encode(b: [u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for x in b {
            s.push_str(&format!("{:02x}", x));
        }
        s
    }
}
