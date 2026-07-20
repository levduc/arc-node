//! Fair comparison of authenticated key-value primitives.
//!
//! Every pairwise fight so far was confounded by something: different workloads, different block
//! fullness, foreign load generators, one side holding fewer keys, or one side measured through
//! reth's slow synchronous path while the other ran optimised. This runs ALL FOUR primitives over
//! an IDENTICAL key set with an IDENTICAL change set at an IDENTICAL scale, on one machine, back
//! to back, reporting build time, memory, and per-update cost.
//!
//! | primitive | commitment | backing store | evictable under memory pressure |
//! |---|---|---|---|
//! | MPT   | keccak Merkle-Patricia | MDBX (reth)      | yes (mmap/page cache) |
//! | JMT   | keccak Jellyfish MT    | redb             | yes (KV on disk) |
//! | dense | keccak fixed-depth     | mmap array       | yes (page cache) |
//! | SALT  | IPA/Pedersen (EC)      | MemStore (heap)  | NO (anonymous, OOMs) |
//!
//! WHAT THIS CONTROLS
//!   same N keys, same k changed per round, same machine, same run.
//!
//! WHAT IT DELIBERATELY DOES NOT EQUALISE (stated, not hidden):
//!   - LEAF CONTENT. The MPT commits a full account leaf RLP(nonce, balance, storageRoot,
//!     codeHash); the other three commit (nonce, balance) only. The MPT therefore does strictly
//!     more work per leaf, and no flag here changes that -- it is inherent to committing EVM
//!     accounts vs committing balances. Read every MPT number with that in mind.
//!   - DURABILITY. MPT and JMT persist their nodes; dense msyncs pages; SALT persists nothing.
//!     Persistence is reported separately where it applies.
//!
//! Run under a cgroup to compare behaviour past RAM -- that is where the ordering flips:
//!   systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0 \
//!     commit-bench --primitives --accounts 20000000 --changed 200 --rounds 20
use alloy_primitives::{B256, U256};
use std::time::Instant;

fn rss_gb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        })
        .map(|kb| kb / 1048576.0)
        .unwrap_or(0.0)
}

/// Uniformly-distributed key, standing in for keccak(address).
///
/// MUST be uniform in the HIGH bytes. An earlier version put the sequential index there
/// (`b[..8] = i.to_be_bytes()`), and since the dense Merkle derives its slot from the TOP bits,
/// every key with i < 2^(64-depth) mapped to slot 0 -- the whole set collided into one bucket and
/// slot_digest hashed all N entries per update. That produced a perfectly linear 42/212/450 ms at
/// 200k/1M/2M keys and looked like an O(N) bug in dense. It was a bug in this generator.
fn key(i: u64) -> B256 {
    let mut b = [0u8; 32];
    // splitmix64 -> avalanche, so the top bits vary with i
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    b[..8].copy_from_slice(&z.to_be_bytes());
    b[8..16].copy_from_slice(&i.to_be_bytes());
    B256::from(b)
}

fn leaf(nonce: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(40);
    v.extend_from_slice(&nonce.to_be_bytes());
    v.extend_from_slice(&B256::from(U256::from(1_000_000u64).to_be_bytes::<32>()).0);
    v
}

pub struct Row {
    pub name: &'static str,
    pub build_s: f64,
    pub rss_gb: f64,
    pub median_ms: f64,
    pub p99_ms: f64,
    pub note: String,
}

fn median(v: &mut Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}
fn p99(v: &mut Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * 0.99) as usize).min(v.len() - 1)]
}

/// Round r changes accounts [r*k, (r+1)*k) -- fresh keys each round, so no round measures the
/// previous round's warm cache. (A fixed key set made an earlier measurement report 19.8 ms with
/// ZERO disk reads: it was timing page-cache hits, not the structure.)
fn changes_for(round: u64, k: u64, n: u64) -> Vec<(B256, Option<Vec<u8>>)> {
    (0..k).map(|j| { let i = (round * k + j) % n; (key(i), Some(leaf(2 + round))) }).collect()
}

pub fn run(n: u64, k: u64, rounds: u64) -> eyre::Result<Vec<Row>> {
    let mut rows = Vec::new();
    let base = std::env::var("BENCH_DIR").unwrap_or_else(|_| "/tmp/prim-bench".into());
    println!("N={n} keys, {k} changed/round, {rounds} rounds, dir={base}\n");

    // ---------------- JMT (redb) ----------------
    {
        let dir = format!("{base}/jmt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::env::set_var("ARC_JMT_STORE_PATH", &dir);
        let rss0 = rss_gb();
        let t = Instant::now();
        let mut done = 0u64;
        while done < n {
            let end = (done + 1_000_000).min(n);
            let batch: Vec<_> =
                (done..end).map(|i| (key(i), Some(leaf(1)))).collect();
            arc_payment_commitment::persistent::commit(&batch);
            done = end;
        }
        let build = t.elapsed().as_secs_f64();
        let mut ts = Vec::new();
        for r in 0..rounds {
            let c = changes_for(r, k, n);
            let t = Instant::now();
            let _ = arc_payment_commitment::persistent::readonly_root(&c);
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        rows.push(Row {
            name: "JMT (redb)",
            build_s: build,
            rss_gb: rss_gb() - rss0,
            median_ms: median(&mut ts),
            p99_ms: p99(&mut ts),
            note: "keccak; nodes in a KV store on disk".into(),
        });
        println!("  JMT done ({build:.0}s build)");
    }

    // ---------------- dense Merkle (mmap) ----------------
    {
        let dir = format!("{base}/dense");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::env::set_var("ARC_DENSE_STORE_PATH", &dir);
        // depth must cover N with headroom, else collision chains dominate
        let depth = (64 - (n as f64 * 1.6) .max(2.0).log2().ceil() as u32).min(28).max(20);
        std::env::set_var("ARC_DENSE_DEPTH", format!("{}", ((n as f64 * 1.6).log2().ceil() as u32).clamp(20, 28)));
        let _ = depth;
        let rss0 = rss_gb();
        let t = Instant::now();
        let mut done = 0u64;
        while done < n {
            let end = (done + 1_000_000).min(n);
            let batch: Vec<_> = (done..end).map(|i| (key(i), Some(leaf(1)))).collect();
            arc_payment_commitment::dense::commit(&batch);
            done = end;
        }
        let build = t.elapsed().as_secs_f64();
        let mut ts = Vec::new();
        for r in 0..rounds {
            let c = changes_for(r, k, n);
            let t = Instant::now();
            let _ = arc_payment_commitment::dense::readonly_root(&c);
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        rows.push(Row {
            name: "dense Merkle (mmap)",
            build_s: build,
            rss_gb: rss_gb() - rss0,
            median_ms: median(&mut ts),
            p99_ms: p99(&mut ts),
            note: "keccak; fixed depth; GRINDABLE (see RESULTS-2h-dense-vs-mpt.md)".into(),
        });
        println!("  dense done ({build:.0}s build)");
    }

    // ---------------- SALT (heap) ----------------
    {
        let rss0 = rss_gb();
        let t = Instant::now();
        let mut done = 0u64;
        while done < n {
            let end = (done + 1_000_000).min(n);
            let batch: Vec<_> = (done..end).map(|i| (key(i), Some(leaf(1)))).collect();
            arc_payment_commitment::salt_commitment::commit(&batch);
            done = end;
        }
        let build = t.elapsed().as_secs_f64();
        let mut ts = Vec::new();
        for r in 0..rounds {
            let c = changes_for(r, k, n);
            let t = Instant::now();
            let _ = arc_payment_commitment::salt_commitment::readonly_root(&c);
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        rows.push(Row {
            name: "SALT (heap)",
            build_s: build,
            rss_gb: rss_gb() - rss0,
            median_ms: median(&mut ts),
            p99_ms: p99(&mut ts),
            note: "IPA/Pedersen EC; unevictable -> OOMs under a cap".into(),
        });
        println!("  SALT done ({build:.0}s build)");
    }

    Ok(rows)
}

pub fn print(rows: &[Row], n: u64, k: u64) {
    println!("\n=== authenticated KV primitives: N={n}, {k} changed/round ===");
    println!("{:<22}{:>10}{:>10}{:>12}{:>11}   {}", "primitive", "build s", "RSS GB", "median ms", "p99 ms", "note");
    for r in rows {
        println!(
            "{:<22}{:>10.0}{:>10.2}{:>12.3}{:>11.3}   {}",
            r.name, r.build_s, r.rss_gb, r.median_ms, r.p99_ms, r.note
        );
    }
    println!("\nMPT is measured separately (needs an MDBX datadir): --accounts N without --primitives,");
    println!("or --existing <db> against a real snapshot. Its leaf is RLP(nonce,balance,storageRoot,");
    println!("codeHash) -- strictly more work per leaf than the (nonce,balance) the others commit.");
}
