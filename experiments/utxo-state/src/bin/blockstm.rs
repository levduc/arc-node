//! Block-STM for the payment lane, built the way the Gravity/grevm folks do it, then measured.
//!
//! grevm's design (from reading /home/papaduck/grevm src):
//!   1. HINT PASS: statically parse each tx's read/write locations (for a transfer: writes
//!      {from,to}, reads {from} for the balance check). Parallel, cheap.
//!   2. DEPENDENCY DAG: walk txs in order; dep[i] = the latest earlier tx that touched a
//!      location i also touches. Independent txs get no edge and run concurrently.
//!   3. OPTIMISTIC EXECUTION + VALIDATION (Block-STM proper): run optimistically against a
//!      multi-version memory, track read-sets, re-validate in commit order, re-execute on
//!      conflict. grevm keeps the DAG to avoid most re-execution.
//!   4. DEFERRED FEE RECIPIENT ("NoRewardHandler"): every tx would otherwise write the block
//!      fee recipient -> a single hot account that serializes EVERYTHING. grevm defers the
//!      reward: accumulate per-tx, credit the recipient once at commit. THE key trick.
//!
//! We implement the deterministic core of that (hint DAG + level-synchronous parallel apply,
//! which is conflict-free *by construction*: two txs sharing an account have a dependency edge
//! so land in different levels) + the deferred-fee-recipient trick, and measure vs serial on
//! three workloads that bracket reality: disjoint, pooled (our recipient-pool spam), and hot.
//!
//! Run: cargo run --release --bin blockstm -- [n_accounts] [txs_per_block] [state_read_ns]
//! state_read_ns models the real per-tx state-access cost (a payment tx's dominant execute cost
//! is the account read from cache/disk, not the arithmetic). Default 10M 9500 0.

use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A transfer: (from, to, amount). Fee is a flat 1 unit to the recipient.
type Tx = (usize, usize, u64);
const FEE: u64 = 1;
const FEE_RECIPIENT: usize = 0;

#[derive(Clone, Copy)]
enum Workload { Disjoint, Pool(usize), Hot }

fn gen(n: usize, m: usize, w: Workload, seed: u64) -> Vec<Tx> {
    // deterministic LCG (Math.random is banned & we want reproducibility)
    let mut s = seed.wrapping_add(0x9E3779B97F4A7C15);
    let mut next = || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
    (0..m).map(|k| match w {
        // every tx touches a fresh disjoint pair -> maximum parallelism
        Workload::Disjoint => ((2 * k + 1) % n, (2 * k + 2) % n, 100),
        // walk a recipient pool (our --recipient-pool spam pattern): moderate collisions
        Workload::Pool(p) => (1 + (next() as usize % (n - 1)), 1 + (next() as usize % p.min(n - 1)), 100),
        // adversarial: everyone pays ONE hot account
        Workload::Hot => (1 + (k % (n - 1)), 1, 100),
    }).collect()
}

// ---- serial baseline (the ground truth for determinism) ----
fn serial(bal: &mut [u64], txs: &[Tx], read_ns: u64) -> u64 {
    let mut fees = 0u64;
    for &(from, to, amt) in txs {
        spin(read_ns); spin(read_ns); // read from + to
        if from != to && bal[from] >= amt + FEE {
            bal[from] -= amt + FEE; bal[to] += amt; fees += FEE;
        }
    }
    bal[FEE_RECIPIENT] += fees; // deferred even in serial (same result)
    fees
}

// ---- grevm-style: hint DAG -> levels -> level-synchronous parallel apply ----
fn dependency_levels(txs: &[Tx], _n: usize) -> Vec<Vec<usize>> {
    // last_touch[account] = last tx index that touched it; dep = max touching predecessor.
    // level[i] = level[dep]+1. Conflict-free within a level by construction.
    let mut last_touch: HashMap<usize, usize> = HashMap::new();
    let mut level = vec![0usize; txs.len()];
    let mut max_level = 0;
    for (i, &(from, to, _)) in txs.iter().enumerate() {
        let mut lv = 0;
        for a in [from, to] {
            if let Some(&j) = last_touch.get(&a) { lv = lv.max(level[j] + 1); }
        }
        level[i] = lv; max_level = max_level.max(lv);
        last_touch.insert(from, i); last_touch.insert(to, i);
    }
    let mut levels = vec![Vec::new(); max_level + 1];
    for (i, &lv) in level.iter().enumerate() { levels[lv].push(i); }
    levels
}

fn parallel(bal: &[AtomicU64], txs: &[Tx], read_ns: u64, defer_fee: bool) -> u64 {
    let levels = dependency_levels(txs, bal.len());
    let total_fees = AtomicU64::new(0);
    for level in &levels {
        // within a level, txs touch disjoint accounts -> Relaxed atomics are safe & race-free
        level.par_iter().for_each(|&i| {
            let (from, to, amt) = txs[i];
            spin(read_ns); spin(read_ns);
            if from != to {
                let fb = bal[from].load(Ordering::Relaxed);
                if fb >= amt + FEE {
                    bal[from].store(fb - amt - FEE, Ordering::Relaxed);
                    bal[to].fetch_add(amt, Ordering::Relaxed);
                    if defer_fee {
                        total_fees.fetch_add(FEE, Ordering::Relaxed);
                    } else {
                        // NAIVE: credit the hot fee recipient inline -> forces it into every
                        // tx's write-set -> in a real DAG this collapses ALL txs to one chain.
                        bal[FEE_RECIPIENT].fetch_add(FEE, Ordering::Relaxed);
                    }
                }
            }
        });
    }
    let f = total_fees.load(Ordering::Relaxed);
    if defer_fee { bal[FEE_RECIPIENT].fetch_add(f, Ordering::Relaxed); }
    f
}

/// A crude busy-spin to model per-account state-read latency (cache miss / disk / decode).
#[inline]
fn spin(ns: u64) {
    if ns == 0 { return; }
    let end = Instant::now();
    while end.elapsed().as_nanos() < ns as u128 { std::hint::spin_loop(); }
}

fn dag_stats(txs: &[Tx], n: usize) -> (usize, f64) {
    let levels = dependency_levels(txs, n);
    let widths: Vec<usize> = levels.iter().map(|l| l.len()).collect();
    let avg_width = txs.len() as f64 / levels.len() as f64;
    (levels.len(), avg_width)
}

fn main() {
    let a: Vec<u64> = std::env::args().skip(1).filter_map(|x| x.parse().ok()).collect();
    let n = *a.first().unwrap_or(&10_000_000) as usize;
    let m = *a.get(1).unwrap_or(&9500) as usize;
    let read_ns = *a.get(2).unwrap_or(&0);
    let threads = rayon::current_num_threads();
    println!("Block-STM (grevm-style DAG) — {n} accounts, {m} tx/block, state_read={read_ns}ns/access, {threads} threads\n");

    for (name, w) in [("disjoint", Workload::Disjoint),
                      ("pool(10k)", Workload::Pool(10_000)),
                      ("hot-recipient", Workload::Hot)] {
        let txs = gen(n, m, w, 42);
        let (depth, width) = dag_stats(&txs, n);

        // serial
        let mut b1 = vec![1_000_000u64; n];
        let t = Instant::now();
        let f_ser = serial(&mut b1, &txs, read_ns);
        let ser_ms = t.elapsed().as_secs_f64() * 1000.0;

        // parallel, deferred fee (the grevm way)
        let b2: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(1_000_000)).collect();
        let t = Instant::now();
        let f_par = parallel(&b2, &txs, read_ns, true);
        let par_ms = t.elapsed().as_secs_f64() * 1000.0;

        // parallel, NAIVE inline fee (shows why deferral matters)
        let b3: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(1_000_000)).collect();
        let t = Instant::now();
        let _ = parallel(&b3, &txs, read_ns, false);
        let naive_ms = t.elapsed().as_secs_f64() * 1000.0;

        // correctness: deferred-parallel must match serial fee total AND balances
        let ok_fee = f_ser == f_par;
        let ok_bal = (0..n).all(|i| b1[i] == b2[i].load(Ordering::Relaxed));

        println!("workload {name:<14} DAG depth {depth:>6}  avg width {width:>8.0}");
        println!("  serial            {ser_ms:>8.2} ms");
        println!("  parallel (defer)  {par_ms:>8.2} ms   speedup {:>4.1}x   correct={}",
                 ser_ms / par_ms, ok_fee && ok_bal);
        println!("  parallel (naive)  {naive_ms:>8.2} ms   <- inline fee recipient\n");
    }
    println!("note: read_ns=0 measures pure scheduling overhead (transfers are ~free);");
    println!("      re-run with read_ns=1000 to model the real per-tx state-read cost.");
}
