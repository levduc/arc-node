// Double-check: does account-model contention actually hurt vs UTXO?
//
// The previous parallel bench was wrong: it wrote a CONSTANT leaf per account and batched
// everything like UTXO, so it never modeled balance read-modify-write or hot-account
// serialization. Here we model it for real and decompose where the time goes.
//
// Account transfer = read bal[from], bal[to] -> write both (NON-commutative RMW). Two txs
// touching the same account conflict; under parallel execution those updates must serialize
// (we measure execution SERIALLY, the worst case for contention). UTXO inputs are disjoint
// so its execution parallelizes. We report verify / execute / commit separately, at uniform
// vs hot (skewed) recipients, so the contention effect is visible if it exists.

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
        // cube the uniform -> heavy skew toward low indices (a few very hot accounts)
        let u = (self.next() % (n as u64)) as f64 / n as f64;
        ((u * u * u) * n as f64) as usize % n
    }
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

#[inline]
fn h2(a: &B256, b: &B256) -> B256 {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(a.as_slice());
    x[32..].copy_from_slice(b.as_slice());
    keccak256(x)
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
            empty[h] = h2(&empty[h - 1], &empty[h - 1]);
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
            let tref: &Vec<B256> = &self.tree;
            let news: Vec<B256> = parents.par_iter().map(|&p| h2(&tref[2 * p], &tref[2 * p + 1])).collect();
            for (p, v) in parents.iter().zip(news) {
                self.tree[*p] = v;
            }
            affected = parents;
        }
    }
}

struct Tx {
    from: u32,
    to: u32,
    vk: VerifyingKey,
    msg: [u8; 32],
    sig: Signature,
}

fn build(n_keys: usize, block: usize, sks: &[SigningKey], vks: &[VerifyingKey], hot: bool, seed: u64) -> Vec<Tx> {
    let mut rng = Rng(seed);
    let mut txs = Vec::with_capacity(block);
    for _ in 0..block {
        let from = rng.below(n_keys);
        let to = if hot { rng.hot(n_keys) } else { rng.below(n_keys) };
        let mut mb = [0u8; 16];
        mb[..8].copy_from_slice(&(from as u64).to_le_bytes());
        mb[8..16].copy_from_slice(&(to as u64).to_le_bytes());
        let msg = keccak256(mb).0;
        let sig: Signature = sks[from].sign_prehash(&msg).unwrap();
        txs.push(Tx { from: from as u32, to: to as u32, vk: vks[from], msg, sig });
    }
    txs
}

#[inline]
fn acct_leaf(bal: u64, nonce: u64) -> B256 {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&bal.to_le_bytes());
    b[8..16].copy_from_slice(&nonce.to_le_bytes());
    keccak256(b)
}

fn run_account(label: &str, txs: &[Tx], n_keys: usize, depth: u32) {
    let block = txs.len();
    // conflict stats
    let mut deg = vec![0u32; n_keys];
    for tx in txs {
        deg[tx.from as usize] += 1;
        deg[tx.to as usize] += 1;
    }
    let maxdeg = *deg.iter().max().unwrap();
    let touched = deg.iter().filter(|&&d| d > 0).count();
    let conflicted: usize = txs
        .iter()
        .filter(|tx| deg[tx.from as usize] > 1 || deg[tx.to as usize] > 1)
        .count();

    // (1) verify in parallel
    let t = Instant::now();
    assert!(txs.par_iter().all(|tx| tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok()));
    let tv = t.elapsed().as_secs_f64();

    // (2) execute SERIALLY: real balance read-modify-write (worst case for hot-account contention)
    let mut bal = vec![1_000_000u64; n_keys];
    let mut nonce = vec![0u64; n_keys];
    let t = Instant::now();
    for tx in txs {
        let (f, to) = (tx.from as usize, tx.to as usize);
        nonce[f] += 1;
        bal[f] = bal[f].saturating_sub(1);
        bal[to] += 1; // non-commutative RMW: reads current bal[to], writes back
    }
    let te = t.elapsed().as_secs_f64();

    // (3) commit: one Merkle update per DISTINCT touched account (final state), batched-parallel
    let mut changes: Vec<(usize, B256)> = (0..n_keys)
        .filter(|&i| deg[i] > 0)
        .map(|i| (i, acct_leaf(bal[i], nonce[i])))
        .collect();
    let mut m = Merkle::new(depth);
    let t = Instant::now();
    m.update_batch_parallel(&mut changes);
    let tc = t.elapsed().as_secs_f64();

    let total = tv + te + tc;
    println!(
        "{:<26} | {:>8.0} | {:>9.0} | {:>9.0} | {:>9.0} | {:>5.0}% conflict, max acct deg {}",
        label,
        block as f64 / total,
        tv * 1e6 / block as f64,
        te * 1e6 / block as f64,
        tc * 1e6 / block as f64,
        conflicted as f64 / block as f64 * 100.0,
        maxdeg,
    );
    let _ = touched;
}

fn run_utxo(label: &str, n_keys: usize, block: usize, sks: &[SigningKey], vks: &[VerifyingKey], depth: u32) {
    let mut rng = Rng(0x7777);
    // each tx: spend distinct input slot i, create 2 new output slots. zero contention.
    struct U {
        vk: VerifyingKey,
        msg: [u8; 32],
        sig: Signature,
        ch: [(usize, B256); 3],
    }
    let mut txs = Vec::with_capacity(block);
    let mut next = block;
    for i in 0..block {
        let owner = rng.below(n_keys);
        let (o1, o2) = (next, next + 1);
        next += 2;
        let mut mb = [0u8; 24];
        mb[..8].copy_from_slice(&(i as u64).to_le_bytes());
        mb[8..16].copy_from_slice(&(o1 as u64).to_le_bytes());
        mb[16..24].copy_from_slice(&(o2 as u64).to_le_bytes());
        let msg = keccak256(mb).0;
        let sig: Signature = sks[owner].sign_prehash(&msg).unwrap();
        txs.push(U {
            vk: vks[owner],
            msg,
            sig,
            ch: [
                (i, B256::ZERO),
                (o1, keccak256((o1 as u64).to_le_bytes())),
                (o2, keccak256((o2 as u64).to_le_bytes())),
            ],
        });
    }
    let t = Instant::now();
    assert!(txs.par_iter().all(|tx| tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok()));
    let tv = t.elapsed().as_secs_f64();
    // execute is disjoint -> parallel; here just collect changes (cost is ~free, no RMW)
    let t = Instant::now();
    let mut changes: Vec<(usize, B256)> = Vec::with_capacity(block * 3);
    for tx in &txs {
        changes.extend_from_slice(&tx.ch);
    }
    let te = t.elapsed().as_secs_f64();
    let mut m = Merkle::new(depth);
    let t = Instant::now();
    m.update_batch_parallel(&mut changes);
    let tc = t.elapsed().as_secs_f64();
    let total = tv + te + tc;
    println!(
        "{:<26} | {:>8.0} | {:>9.0} | {:>9.0} | {:>9.0} | {:>5}% conflict (disjoint coins)",
        label,
        block as f64 / total,
        tv * 1e6 / block as f64,
        te * 1e6 / block as f64,
        tc * 1e6 / block as f64,
        0,
    );
}

fn main() {
    let n_keys = 60_000usize;
    let block = 50_000usize;
    let depth = 21u32;
    eprintln!("threads {} | {n_keys} keys ...", rayon::current_num_threads());
    let sks: Vec<SigningKey> = (0..n_keys as u64).map(keygen).collect();
    let vks: Vec<VerifyingKey> = sks.iter().map(|k| *k.verifying_key()).collect();

    println!("\nThroughput + breakdown (per-tx us), {} cores, {} txs/block\n", rayon::current_num_threads(), block);
    println!(
        "{:<26} | {:>8} | {:>9} | {:>9} | {:>9} |",
        "model", "tx/s", "verify", "execute", "commit"
    );
    println!("{}", "-".repeat(96));
    let uni = build(n_keys, block, &sks, &vks, false, 0x1);
    let hot = build(n_keys, block, &sks, &vks, true, 0x2);
    run_account("account (uniform to)", &uni, n_keys, depth);
    run_account("account (hot recipients)", &hot, n_keys, depth);
    run_utxo("UTXO (disjoint)", n_keys, block, &sks, &vks, depth);
    println!("{}", "-".repeat(96));
    println!("execute = the contended part (account: serial balance RMW = worst-case contention).");
    println!("Caveat: this measures the contended *work*; a real Block-STM also pays scheduler/abort");
    println!("overhead under contention that this does not model. UTXO avoids that machinery entirely.");
}
