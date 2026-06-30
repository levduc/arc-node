// Payment-lane state growth + hot/cold "heat map".
//
// Part 1 — live account state grows with the number of UNIQUE ACCOUNTS (users),
//          NOT with transaction count. A transfer between two *existing* accounts
//          only mutates two balances (no new trie entries); state size is flat.
//          A transfer that funds a *new* account adds one entry. We also time the
//          per-block commitment (re-merklize touched leaves) to show it depends on
//          accounts-touched-per-block, not on chain length.
//
// Part 2 — when the user set exceeds a validator's RAM you can't keep all accounts
//          resident. But payment traffic is skewed: a small "hot" set is active.
//          We simulate skewed access through an LRU (the "heat map" of active
//          accounts) and measure the hit rate vs cache size -> how much RAM the
//          working set actually needs, and what spills to cold disk.
//
// Deterministic (fixed-seed xorshift); reproduces exactly. Run: cargo run --release --bin state_growth

use std::collections::HashMap;
use std::mem::size_of;
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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    // log-uniform rank in [0, n): heavily skewed toward low ranks (~Zipfian).
    #[inline]
    fn skewed(&mut self, n: u64) -> u64 {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64; // [0,1)
        let r = ((n as f64).powf(u)) as u64; // n^u  -> log-uniform
        r.saturating_sub(1).min(n - 1)
    }
}

#[inline]
fn h2(a: &B256, b: &B256) -> B256 {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(a.as_slice());
    x[32..].copy_from_slice(b.as_slice());
    keccak256(x)
}
#[inline]
fn leaf(id: usize, bal: u64) -> B256 {
    let mut x = [0u8; 16];
    x[..8].copy_from_slice(&(id as u64).to_le_bytes());
    x[8..].copy_from_slice(&bal.to_le_bytes());
    keccak256(x)
}

// dense binary Merkle (locality-keyed) — same shape a compact account commitment uses
struct Merkle {
    cap: usize,
    tree: Vec<B256>,
}
impl Merkle {
    fn new(depth: u32) -> Self {
        let cap = 1usize << depth;
        Merkle { cap, tree: vec![B256::ZERO; 2 * cap] }
    }
    #[inline]
    fn set(&mut self, slot: usize, l: B256) {
        let mut i = self.cap + slot;
        self.tree[i] = l;
        while i > 1 {
            let p = i >> 1;
            self.tree[p] = h2(&self.tree[2 * p], &self.tree[2 * p + 1]);
            i = p;
        }
    }
    fn root(&self) -> B256 {
        self.tree[1]
    }
}

const ACCT_BYTES: usize = 100; // realistic per-account on-disk/in-RAM footprint (addr+balance+nonce+trie overhead)

fn part1() {
    println!("\n===== PART 1: state size vs transaction count =====");
    let depth = 20u32; // 1,048,576 leaves
    let k = 1_000_000usize; // fixed user set
    let per_block = 1_000usize;
    let blocks = 5_000usize; // 5,000,000 transfers total
    let mut bal = vec![1_000_000u64; k];
    let mut m = Merkle::new(depth);
    for s in 0..k {
        m.set(s, leaf(s, bal[s]));
    }
    let mut rng = Rng(0x51A7E);
    println!("\n(a) FIXED {k} accounts, transfers only AMONG them:");
    println!(
        "{:>12} | {:>11} | {:>10} | {:>16}",
        "txs", "#accounts", "state MB", "commit µs/block"
    );
    let mut touched: Vec<usize> = Vec::with_capacity(2 * per_block);
    for blk in 1..=blocks {
        touched.clear();
        for _ in 0..per_block {
            let a = rng.below(k as u64) as usize;
            let b = rng.below(k as u64) as usize;
            if a != b && bal[a] > 0 {
                bal[a] -= 1;
                bal[b] += 1;
                touched.push(a);
                touched.push(b);
            }
        }
        let t = Instant::now();
        for &s in &touched {
            m.set(s, leaf(s, bal[s]));
        }
        let us = t.elapsed().as_micros();
        if blk % 1000 == 0 {
            let mb = (k * ACCT_BYTES) as f64 / 1e6;
            println!("{:>12} | {:>11} | {:>10.1} | {:>16}", blk * per_block, k, mb, us);
        }
    }
    println!("root = {} (state unchanged in size)", &m.root().to_string()[..14]);

    println!("\n(b) Each tx funds a NEW account:");
    println!("{:>12} | {:>11} | {:>10}", "txs", "#accounts", "state MB");
    let mut n = 1usize;
    for blk in 1..=blocks {
        n += per_block; // one new account per tx
        if blk % 1000 == 0 {
            let mb = (n * ACCT_BYTES) as f64 / 1e6;
            println!("{:>12} | {:>11} | {:>10.1}", blk * per_block, n, mb);
        }
    }
    let _ = size_of::<u64>();
    println!("\n=> FIXED set: #accounts and state RAM are CONSTANT as txs climb to 5M; per-block");
    println!("   commit time is ~flat (depends on accounts touched per block, not chain length).");
    println!("=> NEW-account: state grows 1:1 with accounts. State tracks USERS, not transactions.");
}

// O(1) LRU (intrusive doubly-linked list in Vecs) — the in-RAM "heat map" of active accounts.
const NIL: u32 = u32::MAX;
struct Lru {
    cap: usize,
    map: HashMap<u64, u32>,
    key: Vec<u64>,
    prev: Vec<u32>,
    next: Vec<u32>,
    head: u32,
    tail: u32,
    size: usize,
}
impl Lru {
    fn new(cap: usize) -> Self {
        Lru {
            cap,
            map: HashMap::with_capacity(cap * 2),
            key: Vec::with_capacity(cap),
            prev: Vec::with_capacity(cap),
            next: Vec::with_capacity(cap),
            head: NIL,
            tail: NIL,
            size: 0,
        }
    }
    #[inline]
    fn unlink(&mut self, i: u32) {
        let (p, n) = (self.prev[i as usize], self.next[i as usize]);
        if p != NIL { self.next[p as usize] = n } else { self.head = n }
        if n != NIL { self.prev[n as usize] = p } else { self.tail = p }
    }
    #[inline]
    fn push_front(&mut self, i: u32) {
        self.prev[i as usize] = NIL;
        self.next[i as usize] = self.head;
        if self.head != NIL { self.prev[self.head as usize] = i }
        self.head = i;
        if self.tail == NIL { self.tail = i }
    }
    // returns true on cache HIT
    fn access(&mut self, k: u64) -> bool {
        if let Some(&i) = self.map.get(&k) {
            self.unlink(i);
            self.push_front(i);
            return true;
        }
        let i = if self.size < self.cap {
            let i = self.key.len() as u32;
            self.key.push(k);
            self.prev.push(NIL);
            self.next.push(NIL);
            self.size += 1;
            i
        } else {
            // evict LRU tail, reuse its node
            let t = self.tail;
            let old = self.key[t as usize];
            self.map.remove(&old);
            self.unlink(t);
            self.key[t as usize] = k;
            t
        };
        self.map.insert(k, i);
        self.push_front(i);
        false
    }
}

// pick an account under a two-level skew: `share` of accesses hit the hottest `hot` users.
#[inline]
fn skewed_pick(rng: &mut Rng, n: u64, hot: u64, share_pct: u64) -> u64 {
    if rng.below(100) < share_pct {
        rng.below(hot) // hot/active user
    } else {
        rng.below(n) // anyone (the long cold tail)
    }
}

fn part2() {
    println!("\n===== PART 2: hot/cold heat map (when users exceed RAM) =====");
    let n: u64 = 1_000_000_000; // 1B users  (~100 GB at 100 B/acct -> EXCEEDS a 64 GB validator)
    let accesses = 50_000_000usize;
    let hot: u64 = 1_000_000; // active working set: ~1M accounts (~100 MB) transacting in this window
    let share_pct = 90u64; // they receive 90% of payment activity; 10% is the cold long tail
    println!(
        "\n{} users total (~{:.0} GB resident at {} B/acct -> exceeds a 64 GB node).",
        n,
        (n as f64 * ACCT_BYTES as f64) / 1e9,
        ACCT_BYTES
    );
    println!(
        "Skew: {}% of txs hit a {}-account active set (~{:.0} MB); the other {}% spread over all {} users.",
        share_pct,
        hot,
        (hot as f64 * ACCT_BYTES as f64) / 1e6,
        100 - share_pct,
        n
    );
    println!("Trace: {} accesses (LRU = the in-RAM heat map of active accounts).\n", accesses);
    println!("{:>14} | {:>9} | {:>11}", "RAM cache (H)", "hit rate", "cache RAM");
    for &h in &[100_000usize, 250_000, 500_000, 1_000_000, 2_000_000, 5_000_000] {
        let mut lru = Lru::new(h);
        let mut rng = Rng(0xC0FFEE); // same trace for every cache size
        let mut hits = 0usize;
        for _ in 0..accesses {
            if lru.access(skewed_pick(&mut rng, n, hot, share_pct)) {
                hits += 1;
            }
        }
        let hr = 100.0 * hits as f64 / accesses as f64;
        let ram_mb = (h * ACCT_BYTES) as f64 / 1e6;
        println!("{:>14} | {:>8.2}% | {:>8.0} MB", h, hr, ram_mb);
    }
    println!("\n=> 1B users need ~100 GB to hold every account (won't fit a 64 GB node). But the ACTIVE");
    println!("   working set is small: caching ~1M hot accounts (~100 MB) already captures most of the");
    println!("   90% hot traffic; the ~10% cold-tail accesses are the irreducible disk hits. So total");
    println!("   users can far exceed RAM as long as the working set (the heat map) fits — that, not");
    println!("   total user count, is the lever. (Hit rate is capped near the 90% hot share by design.)");
}

fn main() {
    println!("Payment-lane state growth + heat map (deterministic, reproducible)");
    part1();
    part2();
}
