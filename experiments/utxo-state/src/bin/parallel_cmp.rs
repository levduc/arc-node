// Parallel throughput: UTXO lane vs account-native transfers.
//
// Both use the same secp256k1 (k256) and the same in-RAM Merkle, and both parallelize:
//   * signature verification  -> rayon par_iter (independent per tx, both models)
//   * Merkle commitment update -> batched, recomputed level-by-level in parallel
// We report single-thread vs all-cores tx/s and the speedup, to see whether UTXO has a
// real *parallel* advantage over reth-style native transfers for SIMPLE payments.

use std::time::Instant;

use alloy_primitives::{keccak256, B256};
use k256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};
use rayon::prelude::*;

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
    #[inline]
    fn hot(&mut self, n: usize) -> usize {
        let u = (self.next() % (n as u64)) as f64 / n as f64;
        ((u * u) * n as f64) as usize % n
    }
}

struct Merkle {
    cap: usize,
    depth: u32,
    tree: Vec<B256>,
}
impl Merkle {
    fn new(depth: u32) -> Self {
        let cap = 1usize << depth;
        let mut empty = vec![B256::ZERO; depth as usize + 1];
        for h in 1..=depth as usize {
            let mut b = [0u8; 64];
            b[..32].copy_from_slice(empty[h - 1].as_slice());
            b[32..].copy_from_slice(empty[h - 1].as_slice());
            empty[h] = keccak256(b);
        }
        let mut tree = vec![B256::ZERO; 2 * cap];
        for h in 1..=depth as usize {
            let (s, e) = (cap >> h, cap >> (h - 1));
            for i in s..e {
                tree[i] = empty[h];
            }
        }
        Merkle { cap, depth, tree }
    }
    // serial update of one leaf + path
    fn set_serial(&mut self, slot: usize, val: B256) {
        let mut idx = self.cap + slot;
        self.tree[idx] = val;
        idx >>= 1;
        while idx >= 1 {
            self.tree[idx] = h2(&self.tree[2 * idx], &self.tree[2 * idx + 1]);
            idx >>= 1;
        }
    }
    // batched, level-by-level parallel update
    fn update_batch_parallel(&mut self, changes: &[(usize, B256)]) {
        for (slot, val) in changes {
            self.tree[self.cap + *slot] = *val;
        }
        let mut affected: Vec<usize> = changes.iter().map(|(s, _)| self.cap + s).collect();
        affected.sort_unstable();
        affected.dedup();
        for _ in 0..self.depth {
            let mut parents: Vec<usize> = affected.iter().map(|&i| i >> 1).collect();
            parents.sort_unstable();
            parents.dedup();
            let tree_ref: &Vec<B256> = &self.tree;
            let news: Vec<B256> = parents
                .par_iter()
                .map(|&p| h2(&tree_ref[2 * p], &tree_ref[2 * p + 1]))
                .collect();
            for (p, v) in parents.iter().zip(news) {
                self.tree[*p] = v;
            }
            affected = parents; // always walk all `depth` levels up to the root
        }
    }
    fn root(&self) -> B256 {
        self.tree[1]
    }
}

#[inline]
fn h2(a: &B256, b: &B256) -> B256 {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(a.as_slice());
    x[32..].copy_from_slice(b.as_slice());
    keccak256(x)
}

fn keygen(i: u64) -> SigningKey {
    let mut seed = keccak256(i.to_le_bytes());
    loop {
        match SigningKey::from_slice(seed.as_slice()) {
            Ok(k) => return k,
            Err(_) => seed = keccak256(seed.as_slice()),
        }
    }
}

struct Tx {
    vk: VerifyingKey,
    msg: [u8; 32],
    sig: Signature,
    changes: [(usize, B256); 3], // (slot, new_leaf); UTXO uses 3, account pads the 3rd with input==output noop
    used: u8,                    // number of valid change entries
}

fn run(label: &str, txs: &[Tx], depth: u32) {
    let n = txs.len();

    // ---------- single-thread ----------
    let mut m1 = Merkle::new(depth);
    let t = Instant::now();
    for tx in txs {
        assert!(tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok());
        for k in 0..tx.used as usize {
            let (slot, val) = tx.changes[k];
            m1.set_serial(slot, val);
        }
    }
    let single = t.elapsed().as_secs_f64();

    // ---------- parallel ----------
    let mut mp = Merkle::new(depth);
    let t = Instant::now();
    let allok = txs.par_iter().all(|tx| tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok());
    assert!(allok);
    let mut changes: Vec<(usize, B256)> = Vec::with_capacity(n * 3);
    for tx in txs {
        for k in 0..tx.used as usize {
            changes.push(tx.changes[k]);
        }
    }
    mp.update_batch_parallel(&changes);
    let par = t.elapsed().as_secs_f64();

    assert_eq!(m1.root(), mp.root(), "roots must match");
    println!(
        "{:<22} | {:>9.0} | {:>10.0} | {:>7.1}x | root {}",
        label,
        n as f64 / single,
        n as f64 / par,
        single / par,
        &mp.root().to_string()[..10]
    );
}

fn main() {
    let n_keys = 60_000usize;
    let block = 50_000usize;
    let depth = 21u32;
    eprintln!("threads: {} | generating {n_keys} keys ...", rayon::current_num_threads());
    let sks: Vec<SigningKey> = (0..n_keys as u64).map(keygen).collect();
    let vks: Vec<VerifyingKey> = sks.iter().map(|k| *k.verifying_key()).collect();
    let mut rng = Rng(0x55);

    // ---- UTXO block: spend 1 (slot), create 2 (new slots). All slots disjoint. ----
    let mut utxo_txs = Vec::with_capacity(block);
    let mut next_slot = block; // inputs occupy [0, block); outputs start after
    for i in 0..block {
        let owner = rng.below(n_keys);
        let input_slot = i; // distinct input per tx
        let o1 = next_slot;
        let o2 = next_slot + 1;
        next_slot += 2;
        let mut mb = [0u8; 24];
        mb[..8].copy_from_slice(&(input_slot as u64).to_le_bytes());
        mb[8..16].copy_from_slice(&(o1 as u64).to_le_bytes());
        mb[16..24].copy_from_slice(&(o2 as u64).to_le_bytes());
        let msg = keccak256(mb).0;
        let sig: Signature = sks[owner].sign_prehash(&msg).unwrap();
        utxo_txs.push(Tx {
            vk: vks[owner],
            msg,
            sig,
            changes: [
                (input_slot, B256::ZERO),         // spend
                (o1, keccak256((o1 as u64).to_le_bytes())),
                (o2, keccak256((o2 as u64).to_le_bytes())),
            ],
            used: 3,
        });
    }

    // ---- account block: native transfers, skewed (hot) recipients -> write conflicts ----
    let mut acct_txs = Vec::with_capacity(block);
    for _ in 0..block {
        let from = rng.below(n_keys);
        let to = rng.hot(n_keys);
        let mut mb = [0u8; 16];
        mb[..8].copy_from_slice(&(from as u64).to_le_bytes());
        mb[8..16].copy_from_slice(&(to as u64).to_le_bytes());
        let msg = keccak256(mb).0;
        let sig: Signature = sks[from].sign_prehash(&msg).unwrap();
        acct_txs.push(Tx {
            vk: vks[from],
            msg,
            sig,
            changes: [
                (from, keccak256((from as u64).to_le_bytes())),
                (to, keccak256((to as u64 ^ 0x99).to_le_bytes())),
                (0, B256::ZERO),
            ],
            used: 2,
        });
    }

    println!(
        "\nParallel throughput, {} cores ({} txs/block, same secp256k1 + Merkle)\n",
        rayon::current_num_threads(),
        block
    );
    println!(
        "{:<22} | {:>9} | {:>10} | {:>8} |",
        "model", "single t/s", "parallel/s", "speedup"
    );
    println!("{}", "-".repeat(72));
    run("UTXO (1 in, 2 out)", &utxo_txs, depth);
    run("account transfer", &acct_txs, depth);
    println!("{}", "-".repeat(72));
    println!("For SIMPLE payments both are signature-bound; verification parallelizes for");
    println!("both, so parallel throughput is similar. UTXO's structural wins are elsewhere:");
    println!("RAM-resident small state (vs reth's disk-bound shared MPT) and conflict-free");
    println!("execution for COMPLEX txs (simple transfers barely exercise that).");
}
