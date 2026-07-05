// Phase 2 — HASH-DOMAIN proofs (post-quantum: collision resistance only; no vector commitments).
//
// A single-leaf Merkle branch is depth hashes, layout-independent. The interesting quantity is the
// BATCHED block witness: the authentication hashes needed to prove a whole block's K touched leaves
// against the root. Adjacent leaves share internal nodes, so the batched multiproof collapses far
// below K independent branches — but ONLY if the touched leaves are logically CLUSTERED.
//
//   * clustered (dense-index / locality keying): block touches a contiguous / windowed range → paths
//     overlap heavily → tiny witness.
//   * scattered (hash-keyed MPT: leaf = keccak(addr) position): leaves never cluster → witness ~ K
//     near-independent branches.
//
// This is the hash-domain, PQ-safe replacement for the (dropped) vector-commitment proof lever: in the
// hash domain binary Merkle is ~proof-optimal, and locality is what shrinks the block witness.
//
// We count authentication hashes exactly (standard Merkle multiproof frontier walk); size = count*32 B.
// Pure combinatorics on leaf POSITIONS — no disk, no hashing needed.
//
// Usage: multiproof --depth 30 --block 500 --trials 200 [--windows 512,4096,65536]

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

/// Number of authentication hashes in the batched Merkle multiproof for `leaves` (0-indexed leaf
/// positions in a binary tree of the given depth). Walks the marked frontier level by level: a marked
/// node whose sibling is NOT marked contributes one provided hash; paired siblings contribute none.
fn multiproof_hashes(mut nodes: Vec<u64>, depth: u32) -> u64 {
    nodes.sort_unstable();
    nodes.dedup();
    let mut count = 0u64;
    for _ in 0..depth {
        let mut parents: Vec<u64> = Vec::with_capacity(nodes.len());
        let mut i = 0;
        while i < nodes.len() {
            let n = nodes[i];
            let sib = n ^ 1;
            if i + 1 < nodes.len() && nodes[i + 1] == sib {
                i += 2; // both known: nothing to provide
            } else {
                count += 1; // sibling must be provided
                i += 1;
            }
            parents.push(n >> 1);
        }
        parents.dedup(); // nodes sorted => parents non-decreasing
        nodes = parents;
    }
    count
}

/// K distinct leaf positions in [0, n) drawn within a window of `w` (w>=k). w==k => contiguous block;
/// w==n => uniform-scattered. Models a block touching accounts within a locality window.
fn pick(rng: &mut Rng, n: u64, k: usize, w: u64) -> Vec<u64> {
    let w = w.clamp(k as u64, n);
    let start = if w >= n { 0 } else { rng.next() % (n - w + 1) };
    if w == k as u64 {
        return (0..k as u64).map(|i| start + i).collect();
    }
    // sample k distinct in [start, start+w) via a small hash set
    use std::collections::HashSet;
    let mut set = HashSet::with_capacity(k);
    while set.len() < k {
        set.insert(start + rng.next() % w);
    }
    set.into_iter().collect()
}

fn avg_hashes(rng: &mut Rng, depth: u32, k: usize, w: u64, trials: usize) -> f64 {
    let n = 1u64 << depth;
    let mut sum = 0u64;
    for _ in 0..trials {
        sum += multiproof_hashes(pick(rng, n, k, w), depth);
    }
    sum as f64 / trials as f64
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut depth = 30u32;
    let mut block = 500usize;
    let mut trials = 200usize;
    let mut windows = vec![0u64]; // filled below with block, some windows, and n
    let mut custom_windows: Option<Vec<u64>> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--depth" => {
                i += 1;
                depth = args[i].parse().unwrap();
            }
            "--block" => {
                i += 1;
                block = args[i].parse().unwrap();
            }
            "--trials" => {
                i += 1;
                trials = args[i].parse().unwrap();
            }
            "--windows" => {
                i += 1;
                custom_windows = Some(args[i].split(',').map(|s| s.parse().unwrap()).collect());
            }
            _ => {}
        }
        i += 1;
    }
    let n = 1u64 << depth;
    let _ = windows.pop();
    windows.push(block as u64); // contiguous
    if let Some(cw) = custom_windows {
        windows.extend(cw);
    } else {
        for &w in &[block as u64 * 8, block as u64 * 128, block as u64 * 2048] {
            if w < n {
                windows.push(w);
            }
        }
    }
    windows.push(n); // scattered

    let mut rng = Rng(0x1234_5678);
    let per_tx_branch = depth as usize; // single-leaf proof = depth hashes
    println!(
        "# multiproof depth={depth} N={n} block={block} trials={trials} | single-leaf branch = {} hashes = {} B",
        per_tx_branch,
        per_tx_branch * 32
    );
    println!(
        "# {:>12}  {:>10}  {:>12}  {:>14}  {:>12}",
        "window", "hashes", "witness(KiB)", "amortized/tx", "vs K-indep"
    );
    let k_indep = (block * depth as usize) as f64; // K independent branches (no sharing)
    for &w in &windows {
        let h = avg_hashes(&mut rng, depth, block, w, trials);
        let label = if w == block as u64 {
            "contiguous".to_string()
        } else if w == n {
            "scattered".to_string()
        } else {
            format!("win={w}")
        };
        println!(
            "  {:>12}  {:>10.0}  {:>12.1}  {:>14.2}  {:>11.1}x",
            label,
            h,
            h * 32.0 / 1024.0,
            h / block as f64,
            k_indep / h,
        );
    }
    println!(
        "# 'scattered' models a hash-keyed MPT (leaf=keccak(addr) → no clustering); 'contiguous'/'win' \
model dense-index locality. Witness is PQ-safe (hashes only)."
    );
}
