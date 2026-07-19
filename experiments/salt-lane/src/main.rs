//! SALT (MegaETH) as a payment-lane state commitment — feasibility + cost probe.
//!
//! Purpose: before wiring SALT into reth's two seams (read-only overlay root / per-block commit),
//! answer three questions cheaply, in a workspace detached from the arc-node tree:
//!   1. Does it BUILD on our stable toolchain with the ark-* patches isolated here? (SALT pins
//!      nightly-2026-03-20 for its own dev; that may or may not be a hard requirement.)
//!   2. Does its API give us the read-only/commit split reth needs? `update_fin(&kvs)` computes
//!      state updates without mutating the store; `store.update_state(updates)` advances the base.
//!      That is exactly the shape the JMT and dense lanes already use.
//!   3. What does a root update COST per block, so it is comparable to the MPT / JMT / dense
//!      numbers measured on the live head-to-head?
//!
//! Workload mirrors the head-to-head: ~800 changed accounts per block, every transfer paying a
//! brand-new recipient, so the account set grows the way the live runs grow it.
//!
//! NOTE on fairness: SALT is explicitly an IN-MEMORY design (its claim is ~1 GB for 3B keys with
//! no random disk I/O for root updates), so unlike the dense lane it is not expected to pay a
//! per-block write cost. Durability would come from snapshotting, not per-block persistence. This
//! probe therefore measures COMPUTE only, and any comparison must say so rather than quietly
//! crediting SALT for skipping I/O.

use hashbrown::HashMap;
use salt::{EphemeralSaltState, MemStore, StateRoot};
use std::time::Instant;
use tiny_keccak::{Hasher, Keccak};

/// 32-byte hashed account key, the same shape reth hands the overlay.
fn hashed_key(n: u64) -> Vec<u8> {
    let mut k = Keccak::v256();
    k.update(&n.to_be_bytes());
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out.to_vec()
}

/// Account leaf: (nonce, balance) — the same payload the JMT and dense lanes commit, so the
/// per-key work is comparable.
fn account_leaf(nonce: u64, balance: u128) -> Vec<u8> {
    let mut v = Vec::with_capacity(40);
    v.extend_from_slice(&nonce.to_be_bytes());
    v.extend_from_slice(&balance.to_be_bytes());
    v
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let blocks: usize = std::env::var("BLOCKS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    let per_block: u64 =
        std::env::var("PER_BLOCK").ok().and_then(|v| v.parse().ok()).unwrap_or(800);

    println!("SALT payment-lane probe: {blocks} blocks x {per_block} changed accounts/block");

    let store = MemStore::new();
    let mut state = EphemeralSaltState::new(&store);

    let mut next_account: u64 = 0;
    let mut samples: Vec<f64> = Vec::with_capacity(blocks);
    let mut stage_state: Vec<f64> = Vec::with_capacity(blocks);
    let mut stage_trie: Vec<f64> = Vec::with_capacity(blocks);

    for b in 0..blocks {
        // Each block: `per_block` fresh recipients (grow the set) — matches --fresh-recipients.
        let mut kvs: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
        for _ in 0..per_block {
            kvs.insert(hashed_key(next_account), Some(account_leaf(1, 1_000_000)));
            next_account += 1;
        }

        // SALT is TWO stages and both are needed for a state root:
        //   (1) state layer  — SALT-encode the kv changes into bucket updates (no commitment)
        //   (2) trie layer   — the IPA/Pedersen commitment that actually yields root_hash
        // reth's read-only overlay root == stages 1+2 without persisting; reth's commit ==
        // store.update_state + store.update_trie. Timing only stage 1 measures bucket placement,
        // NOT a root.
        let t0 = Instant::now();
        let state_updates = state.update_fin(&kvs)?;
        let state_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let mut state_root = StateRoot::new(&store);
        let (_root_hash, trie_updates) = state_root.update_fin(&state_updates)?;
        let trie_ms = t1.elapsed().as_secs_f64() * 1000.0;

        // Commit both layers (the write_hashed_state equivalent).
        store.update_state(state_updates);
        store.update_trie(trie_updates);

        let total_ms = state_ms + trie_ms;
        samples.push(total_ms);
        stage_state.push(state_ms);
        stage_trie.push(trie_ms);
        if b % 25 == 0 {
            println!(
                "  block {b:>5}  accounts={:>9}  root={total_ms:.3} ms  (state {state_ms:.3} + trie/commitment {trie_ms:.3})",
                next_account
            );
        }
    }

    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
    };
    let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
    let median = pct(&mut samples, 0.5);
    let p99 = pct(&mut samples, 0.99);
    let med_state = pct(&mut stage_state, 0.5);
    let med_trie = pct(&mut stage_trie, 0.5);
    println!("\n--- SALT root-update cost ({} accounts) ---", next_account);
    println!("median {median:.3} ms | mean {mean:.3} ms | p99 {p99:.3} ms");
    println!("  stage split (median): state/bucket {med_state:.3} ms + trie/commitment {med_trie:.3} ms");
    println!("per changed account: {:.1} us", median * 1000.0 / per_block as f64);
    println!("\nCOMPUTE ONLY (in-memory store) — not comparable to lanes that also persist.");
    Ok(())
}
