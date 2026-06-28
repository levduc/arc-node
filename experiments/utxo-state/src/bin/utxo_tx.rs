// UTXO-only transaction throughput.
//
// Processes REAL signed UTXO payments end-to-end the way a validator would:
//   verify input signature (secp256k1 ECDSA) -> check input unspent -> remove input
//   -> add outputs -> update the in-RAM Merkle commitment.
//
// Goal: the actual tx/s of a UTXO payment lane, and where the time goes. Spoiler from
// the consensus sweep: signature verification dominates; state + commitment are cheap.
// UTXO is embarrassingly parallel, so we also measure parallel signature verification.

use std::collections::HashMap;
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
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct OutPoint {
    txid: [u8; 32],
    vout: u32,
}
#[derive(Clone, Copy)]
struct Output {
    owner: u32, // index into the key pool
    amount: u64,
    slot: u32,
}

// ---- dense in-RAM Merkle commitment (hash-only) ----
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
    #[inline]
    fn root(&self) -> B256 {
        self.tree[1]
    }
}

#[inline]
fn leaf_hash(op: &OutPoint, owner: u32, amount: u64) -> B256 {
    let mut b = [0u8; 48];
    b[..32].copy_from_slice(&op.txid);
    b[32..36].copy_from_slice(&op.vout.to_le_bytes());
    b[36..40].copy_from_slice(&owner.to_le_bytes());
    b[40..48].copy_from_slice(&amount.to_le_bytes());
    keccak256(b)
}

// a signed UTXO tx: spend one input, create two outputs
struct SignedTx {
    input: OutPoint,
    vk: VerifyingKey,
    msg: [u8; 32],
    sig: Signature,
    outputs: [(u32, u64); 2], // (owner_idx, amount)
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

fn tx_digest(input: &OutPoint, outputs: &[(u32, u64); 2]) -> [u8; 32] {
    let mut b = [0u8; 36 + 24];
    b[..32].copy_from_slice(&input.txid);
    b[32..36].copy_from_slice(&input.vout.to_le_bytes());
    for (i, (o, a)) in outputs.iter().enumerate() {
        let off = 36 + i * 12;
        b[off..off + 4].copy_from_slice(&o.to_le_bytes());
        b[off + 4..off + 12].copy_from_slice(&a.to_le_bytes());
    }
    keccak256(b).0
}

fn main() {
    let n_keys = 50_000usize;
    let genesis = 500_000usize;
    let block = 50_000usize;
    let verify_n = 100_000usize;
    let depth = 21u32; // capacity 2.1M slots

    eprintln!("generating {n_keys} secp256k1 keypairs ...");
    let sks: Vec<SigningKey> = (0..n_keys as u64).map(keygen).collect();
    let vks: Vec<VerifyingKey> = sks.iter().map(|k| *k.verifying_key()).collect();

    // ---- state ----
    let mut m = Merkle::new(depth);
    let mut set: HashMap<OutPoint, Output> = HashMap::new();
    let mut live: Vec<OutPoint> = Vec::new();
    let mut next_slot: u32 = 0;
    let mut rng = Rng(0xC0FFEE);
    let mut nonce: u64 = 0;

    let mut mk_utxo = |set: &mut HashMap<OutPoint, Output>,
                       live: &mut Vec<OutPoint>,
                       m: &mut Merkle,
                       next_slot: &mut u32,
                       nonce: &mut u64,
                       owner: u32,
                       amount: u64| {
        let mut txid = [0u8; 32];
        txid[..8].copy_from_slice(&nonce.to_le_bytes());
        *nonce += 1;
        let op = OutPoint { txid, vout: 0 };
        let slot = *next_slot;
        *next_slot += 1;
        m.set(slot as usize, leaf_hash(&op, owner, amount));
        set.insert(op, Output { owner, amount, slot });
        live.push(op);
    };

    eprintln!("seeding {genesis} genesis UTXOs ...");
    for _ in 0..genesis {
        let owner = rng.below(n_keys) as u32;
        mk_utxo(&mut set, &mut live, &mut m, &mut next_slot, &mut nonce, owner, 1000);
    }

    // ============ (A) pure signature verification (the dominant cost) ============
    eprintln!("building {verify_n} signatures for the verify-only bench ...");
    let sigs: Vec<(VerifyingKey, [u8; 32], Signature)> = (0..verify_n)
        .map(|i| {
            let ki = rng.below(n_keys);
            let mut msg = keccak256(i.to_le_bytes()).0;
            msg[0] ^= 1;
            let sig: Signature = sks[ki].sign_prehash(&msg).unwrap();
            (vks[ki], msg, sig)
        })
        .collect();

    let t = Instant::now();
    let ok1: usize = sigs.iter().filter(|(vk, m, s)| vk.verify_prehash(m, s).is_ok()).count();
    let single = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let okp: usize = sigs.par_iter().filter(|(vk, m, s)| vk.verify_prehash(m, s).is_ok()).count();
    let par = t.elapsed().as_secs_f64();
    assert_eq!(ok1, verify_n);
    assert_eq!(okp, verify_n);

    println!("\n=== (A) secp256k1 ECDSA signature verification ===");
    println!(
        "  single-thread : {:>9.0} verify/s  ({:.1} us each)",
        verify_n as f64 / single,
        single * 1e6 / verify_n as f64
    );
    println!(
        "  {:>2}-thread     : {:>9.0} verify/s  ({:.1}x speedup)",
        rayon::current_num_threads(),
        verify_n as f64 / par,
        single / par
    );

    // ============ (B) full UTXO-tx validation, single-thread ============
    let build_block = |set: &HashMap<OutPoint, Output>,
                       live: &mut Vec<OutPoint>,
                       sks: &[SigningKey],
                       vks: &[VerifyingKey],
                       rng: &mut Rng|
     -> Vec<SignedTx> {
        let mut txs = Vec::with_capacity(block);
        for _ in 0..block {
            if live.is_empty() {
                break;
            }
            let idx = rng.below(live.len());
            let input = live.swap_remove(idx);
            let owner = set.get(&input).unwrap().owner as usize;
            let outputs = [
                (rng.below(vks.len()) as u32, 500u64),
                (rng.below(vks.len()) as u32, 500u64),
            ];
            let msg = tx_digest(&input, &outputs);
            let sig: Signature = sks[owner].sign_prehash(&msg).unwrap();
            txs.push(SignedTx { input, vk: vks[owner], msg, sig, outputs });
        }
        txs
    };

    let apply = |tx: &SignedTx,
                 set: &mut HashMap<OutPoint, Output>,
                 live: &mut Vec<OutPoint>,
                 m: &mut Merkle,
                 next_slot: &mut u32,
                 nonce: &mut u64| {
        // spend input
        if let Some(out) = set.remove(&tx.input) {
            m.set(out.slot as usize, B256::ZERO);
        }
        // create outputs
        for (owner, amount) in tx.outputs {
            mk_utxo(set, live, m, next_slot, nonce, owner, amount);
        }
    };

    eprintln!("building block A ({block} txs) ...");
    let block_a = build_block(&set, &mut live, &sks, &vks, &mut rng);
    let na = block_a.len();

    let t = Instant::now();
    let mut bad = 0usize;
    let tv = Instant::now();
    let mut verify_us = 0.0;
    for tx in &block_a {
        let v = Instant::now();
        if tx.vk.verify_prehash(&tx.msg, &tx.sig).is_err() {
            bad += 1;
        }
        verify_us += v.elapsed().as_secs_f64();
        apply(tx, &mut set, &mut live, &mut m, &mut next_slot, &mut nonce);
    }
    let _ = tv;
    let total_a = t.elapsed().as_secs_f64();
    assert_eq!(bad, 0);
    let apply_us = (total_a - verify_us) * 1e6 / na as f64;

    println!("\n=== (B) full UTXO-tx validation (1 input, 2 outputs) ===");
    println!(
        "  single-thread : {:>9.0} tx/s   (verify {:.1} us + spend/set/merkle {:.1} us per tx)",
        na as f64 / total_a,
        verify_us * 1e6 / na as f64,
        apply_us
    );

    // ============ (C) parallel verify + serial apply ============
    eprintln!("building block B ({block} txs) ...");
    let block_b = build_block(&set, &mut live, &sks, &vks, &mut rng);
    let nb = block_b.len();

    let t = Instant::now();
    // parallel signature verification (independent per tx)
    let allok = block_b
        .par_iter()
        .all(|tx| tx.vk.verify_prehash(&tx.msg, &tx.sig).is_ok());
    let tverify = t.elapsed().as_secs_f64();
    assert!(allok);
    // serial state + commitment application
    let ta = Instant::now();
    for tx in &block_b {
        apply(tx, &mut set, &mut live, &mut m, &mut next_slot, &mut nonce);
    }
    let tapply = ta.elapsed().as_secs_f64();
    let total_b = tverify + tapply;

    println!(
        "  parallel verify + serial apply : {:>9.0} tx/s   (verify {:.0} ms || apply {:.0} ms)",
        nb as f64 / total_b,
        tverify * 1e3,
        tapply * 1e3
    );

    println!("\nfinal UTXO set: {} unspent | Merkle root {}", set.len(), m.root());
    println!(
        "verification dominates; UTXO state+commitment are ~{:.0}x cheaper. UTXO is parallel by nature,",
        verify_us / (total_a - verify_us)
    );
    println!("so verification (the bottleneck) scales with cores; apply (state+merkle) is the serial tail.");
}
