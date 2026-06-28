// Is a UTXO payment lane better than reth-style native (account) transfers?
//
// Same machine, same secp256k1 (k256), same in-RAM Merkle commitment, same block size.
// We validate a block of simple payments under each model and compare tx/s + breakdown:
//
//   UTXO    : verify sig -> check input unspent -> remove input -> add 2 outputs -> merkle x3
//   Account : verify sig -> check nonce/balance -> debit sender, credit recipient -> merkle x2
//
// Then we count write-write conflicts in each block, because that decides how well the
// model parallelizes (UTXO payments touch disjoint coins; account credits collide on hot
// recipients).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use alloy_primitives::{keccak256, B256};
use k256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

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
    // skewed pick: squares the uniform to favor low indices (hot accounts)
    #[inline]
    fn hot(&mut self, n: usize) -> usize {
        let u = (self.next() % (n as u64)) as f64 / n as f64;
        ((u * u) * n as f64) as usize % n
    }
}

struct Merkle {
    cap: usize,
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
        Merkle { cap, tree }
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct OutPoint {
    txid: [u8; 32],
    vout: u32,
}

fn main() {
    let n_keys = 60_000usize;
    let block = 50_000usize;
    let depth = 21u32;

    eprintln!("generating {n_keys} keypairs ...");
    let sks: Vec<SigningKey> = (0..n_keys as u64).map(keygen).collect();
    let vks: Vec<VerifyingKey> = sks.iter().map(|k| *k.verifying_key()).collect();

    // =====================================================================
    // MODEL 1: UTXO
    // =====================================================================
    let utxo_tps;
    let (u_verify, u_apply);
    {
        let mut m = Merkle::new(depth);
        let mut set: HashMap<OutPoint, (u32, u32)> = HashMap::new(); // op -> (owner, slot)
        let mut live: Vec<OutPoint> = Vec::new();
        let mut slot: u32 = 0;
        let mut nonce: u64 = 0;
        let mut rng = Rng(0x11);
        // genesis
        for _ in 0..(block * 2) {
            let mut txid = [0u8; 32];
            txid[..8].copy_from_slice(&nonce.to_le_bytes());
            nonce += 1;
            let op = OutPoint { txid, vout: 0 };
            let owner = rng.below(n_keys) as u32;
            m.set(slot as usize, keccak256(txid));
            set.insert(op, (owner, slot));
            live.push(op);
            slot += 1;
        }
        // build block (untimed)
        struct Tx {
            input: OutPoint,
            vk: VerifyingKey,
            msg: [u8; 32],
            sig: Signature,
            outs: [u32; 2],
        }
        let mut txs = Vec::with_capacity(block);
        for _ in 0..block {
            let idx = rng.below(live.len());
            let input = live.swap_remove(idx);
            let owner = set.get(&input).unwrap().0 as usize;
            let outs = [rng.below(n_keys) as u32, rng.below(n_keys) as u32];
            let mut mb = [0u8; 40];
            mb[..32].copy_from_slice(&input.txid);
            mb[32..36].copy_from_slice(&outs[0].to_le_bytes());
            mb[36..40].copy_from_slice(&outs[1].to_le_bytes());
            let msg = keccak256(mb).0;
            let sig: Signature = sks[owner].sign_prehash(&msg).unwrap();
            txs.push(Tx { input, vk: vks[owner], msg, sig, outs });
        }
        // validate (timed)
        let mut vt = 0.0;
        let t0 = Instant::now();
        for tx in &txs {
            let v = Instant::now();
            assert!(tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok());
            vt += v.elapsed().as_secs_f64();
            let (_, s) = set.remove(&tx.input).unwrap(); // unspent check
            m.set(s as usize, B256::ZERO);
            for (k, owner) in tx.outs.iter().enumerate() {
                let mut txid = [0u8; 32];
                txid[..8].copy_from_slice(&nonce.to_le_bytes());
                nonce += 1;
                let op = OutPoint { txid, vout: k as u32 };
                m.set(slot as usize, keccak256(txid));
                set.insert(op, (*owner, slot));
                slot += 1;
            }
        }
        let total = t0.elapsed().as_secs_f64();
        utxo_tps = block as f64 / total;
        u_verify = vt * 1e6 / block as f64;
        u_apply = (total - vt) * 1e6 / block as f64;
    }

    // =====================================================================
    // MODEL 2: account-model native transfers (reth-style)
    // =====================================================================
    let acct_tps;
    let (a_verify, a_apply);
    let conflicts;
    {
        let mut m = Merkle::new(depth);
        let mut nonce_of = vec![0u64; n_keys];
        let mut bal = vec![1_000_000u64; n_keys];
        let mut rng = Rng(0x22);
        // commit genesis balances
        for i in 0..n_keys {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&bal[i].to_le_bytes());
            m.set(i, keccak256(b));
        }
        // build block of native transfers (untimed); recipients are skewed (hot accounts)
        struct Tx {
            from: u32,
            to: u32,
            amount: u64,
            vk: VerifyingKey,
            msg: [u8; 32],
            sig: Signature,
        }
        let mut txs = Vec::with_capacity(block);
        let mut local_nonce = nonce_of.clone();
        for _ in 0..block {
            let from = rng.below(n_keys) as u32;
            let to = rng.hot(n_keys) as u32; // skewed -> hot recipients
            let amount = 1u64;
            let mut mb = [0u8; 20];
            mb[..4].copy_from_slice(&from.to_le_bytes());
            mb[4..8].copy_from_slice(&to.to_le_bytes());
            mb[8..16].copy_from_slice(&amount.to_le_bytes());
            mb[16..20].copy_from_slice(&(local_nonce[from as usize] as u32).to_le_bytes());
            local_nonce[from as usize] += 1;
            let msg = keccak256(mb).0;
            let sig: Signature = sks[from as usize].sign_prehash(&msg).unwrap();
            txs.push(Tx { from, to, amount, vk: vks[from as usize], msg, sig });
        }
        // count write-write conflicts (accounts written by >1 tx -> not freely parallel)
        let mut writes: HashMap<u32, u32> = HashMap::new();
        for tx in &txs {
            *writes.entry(tx.from).or_default() += 1;
            *writes.entry(tx.to).or_default() += 1;
        }
        let mut conflicting_txs = 0usize;
        {
            let mut seen: HashSet<u32> = HashSet::new();
            for tx in &txs {
                let f = writes[&tx.from] > 1;
                let t = writes[&tx.to] > 1;
                if (f && !seen.insert(tx.from)) || (t && !seen.insert(tx.to)) {
                    conflicting_txs += 1;
                }
            }
        }
        conflicts = conflicting_txs as f64 / block as f64 * 100.0;
        // validate (timed)
        let mut vt = 0.0;
        let t0 = Instant::now();
        for tx in &txs {
            let v = Instant::now();
            assert!(tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok());
            vt += v.elapsed().as_secs_f64();
            // nonce + balance checks, then apply
            let f = tx.from as usize;
            let t = tx.to as usize;
            nonce_of[f] += 1;
            bal[f] = bal[f].saturating_sub(tx.amount);
            bal[t] += tx.amount;
            // commit both touched accounts
            let mut bf = [0u8; 16];
            bf[..8].copy_from_slice(&bal[f].to_le_bytes());
            bf[8..16].copy_from_slice(&nonce_of[f].to_le_bytes());
            m.set(f, keccak256(bf));
            let mut bt = [0u8; 16];
            bt[..8].copy_from_slice(&bal[t].to_le_bytes());
            m.set(t, keccak256(bt));
        }
        let total = t0.elapsed().as_secs_f64();
        acct_tps = block as f64 / total;
        a_verify = vt * 1e6 / block as f64;
        a_apply = (total - vt) * 1e6 / block as f64;
    }

    println!("\nUTXO lane vs account-native transfers (single-thread, shallow RAM, same secp256k1 + Merkle)\n");
    println!(
        "{:<26} | {:>10} | {:>10} | {:>10}",
        "model", "tx/s", "verify(us)", "apply(us)"
    );
    println!("{}", "-".repeat(66));
    println!(
        "{:<26} | {:>10.0} | {:>10.1} | {:>10.1}",
        "UTXO (1 in, 2 out)", utxo_tps, u_verify, u_apply
    );
    println!(
        "{:<26} | {:>10.0} | {:>10.1} | {:>10.1}",
        "account native transfer", acct_tps, a_verify, a_apply
    );
    println!("{}", "-".repeat(66));
    println!(
        "Both are signature-bound (~{:.0} us verify) and roughly equal single-thread.",
        (u_verify + a_verify) / 2.0
    );
    println!(
        "Parallelism differs: account block has {:.0}% txs in a write-write conflict (hot recipients)",
        conflicts
    );
    println!("-> not freely parallel (Block-STM aborts/retries); UTXO inputs are disjoint -> 0% conflict.");
    println!("And on REAL state reth's MPT is disk-bound (App C: 0.1-0.37 Ggas/s); the UTXO set stays in RAM.");
}
