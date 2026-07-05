// Phase 1 — account state-commitment latency BEYOND RAM.
//
// Question: when payment state exceeds RAM, does dense (locality) keying beat hash keying for the
// cost of applying a block and recomputing the authenticated root? locality.rs answered this in RAM
// (~1.4x); this answers it on disk, where the MPT actually pays (27.8 ms cold on real Arc state).
//
// Method: ONE file-backed mmap holds a full binary Merkle tree over N dense accounts (leaf = balance).
// Two PHYSICAL layouts over the SAME logical tree, SAME footprint, SAME values — only node placement
// differs:
//   * dense  : physical(j) = j                          (contiguous; a locality-keyed store)
//   * hashed : physical(j) = (j * ODD) & (2^m - 1)      (storage-free bijection scattering each
//              node to a random page; models a hash-keyed node store / MPT-in-KV placement)
// Between measured blocks we madvise(MADV_DONTNEED) the region to EVICT our pages, so the next block
// reads genuinely cold from disk — modelling state whose working set exceeds RAM, without needing a
// literal >RAM build every run (we validate against a real >RAM build separately). Per block we count
// cold bytes read (/proc/self/io read_bytes), major page faults (getrusage.ru_majflt), and wall time.
//
// A Merkle branch proof is depth*32 bytes and is layout-independent (reported once); the proof
// dimension proper (vector commitments) is Phase 3.
//
// Usage:
//   disk_bench --depth 26 --block 500 --blocks 40 [--zipf] [--layout dense|hashed|both]
//              [--dir /path/for/bigfile]
//   depth D => N = 2^D accounts, node file = 2^(D+1) * 32 bytes (e.g. D=30 => 64 GiB).

use std::fs::OpenOptions;
use std::time::Instant;

use alloy_primitives::{keccak256, B256};
use memmap2::MmapMut;

// ---- tiny xorshift rng (deterministic per seed) ----
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

#[inline]
fn h2(a: &[u8], b: &[u8]) -> B256 {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(a);
    x[32..].copy_from_slice(b);
    keccak256(x)
}

/// Odd multiplier => bijection on Z/2^m; scatters logical node indices across physical slots.
const ODD: u64 = 0x9E3779B97F4A7C15; // golden-ratio odd constant

struct Tree {
    file: std::fs::File,
    mmap: MmapMut,
    depth: u32,
    cap: usize, // 2^depth leaves
    mask: u64,  // num_nodes-1, for the hashed permutation
    dense: bool, // physical layout
}

impl Tree {
    fn new(path: &str, depth: u32, dense: bool) -> std::io::Result<Self> {
        let cap = 1usize << depth;
        let num_nodes = 2 * cap;
        let bytes = (num_nodes as u64) * 32;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.set_len(bytes)?;
        let mmap = unsafe { MmapMut::map_mut(&f)? };
        Ok(Self {
            file: f,
            mmap,
            depth,
            cap,
            mask: (num_nodes as u64) - 1,
            dense,
        })
    }

    #[inline]
    fn phys(&self, logical: usize) -> usize {
        if self.dense {
            logical
        } else {
            ((logical as u64).wrapping_mul(ODD) & self.mask) as usize
        }
    }

    #[inline]
    fn get(&self, logical: usize) -> &[u8] {
        let off = self.phys(logical) * 32;
        &self.mmap[off..off + 32]
    }

    #[inline]
    fn set(&mut self, logical: usize, v: &[u8]) {
        let off = self.phys(logical) * 32;
        self.mmap[off..off + 32].copy_from_slice(v);
    }

    /// Build a FULL tree: N dense accounts with random nonzero balances, internal nodes computed
    /// bottom-up. This is the one-time state build (its cost is reported separately, not per-block).
    fn build(&mut self, seed: u64) {
        let mut rng = Rng(seed);
        // leaves: logical index cap .. 2*cap
        for i in 0..self.cap {
            let mut leaf = [0u8; 32];
            leaf[..8].copy_from_slice(&rng.next().to_le_bytes());
            leaf[24..].copy_from_slice(&(i as u64).to_le_bytes()); // ensure distinct
            self.set(self.cap + i, &leaf);
        }
        // internal: level by level up to root (logical index 1)
        let mut width = self.cap;
        let mut base = self.cap;
        while width > 1 {
            let parent_base = base / 2;
            for p in 0..(width / 2) {
                let l = self.get(base + 2 * p).to_vec();
                let r = self.get(base + 2 * p + 1).to_vec();
                let h = h2(&l, &r);
                self.set(parent_base + p, h.as_slice());
            }
            width /= 2;
            base = parent_base;
        }
    }

    fn root(&self) -> B256 {
        B256::from_slice(self.get(1))
    }

    /// Apply a block: set new balances at `leaves` (logical leaf indices) then recompute all affected
    /// paths to the root, deduped level-by-level so each dirty node is recomputed once.
    fn apply_block(&mut self, leaves: &[usize], seed: u64) {
        let mut rng = Rng(seed);
        // write new leaf balances; collect dirty parent indices (dedup via sort)
        let mut dirty: Vec<usize> = Vec::with_capacity(leaves.len());
        for &li in leaves {
            let mut leaf = [0u8; 32];
            leaf[..8].copy_from_slice(&rng.next().to_le_bytes());
            leaf[24..].copy_from_slice(&((li - self.cap) as u64).to_le_bytes());
            self.set(li, &leaf);
            dirty.push(li / 2);
        }
        for _ in 0..self.depth {
            dirty.sort_unstable();
            dirty.dedup();
            let mut next: Vec<usize> = Vec::with_capacity(dirty.len());
            for &node in &dirty {
                let l = self.get(2 * node).to_vec();
                let r = self.get(2 * node + 1).to_vec();
                let h = h2(&l, &r);
                self.set(node, h.as_slice());
                if node > 1 {
                    next.push(node / 2);
                }
            }
            dirty = next;
            if dirty.is_empty() {
                break;
            }
        }
    }

    /// Flush dirty pages to the backing file so a subsequent fadvise(DONTNEED) can drop them.
    fn flush(&self) {
        let _ = self.mmap.flush();
    }

    /// Evict our pages so the next access is a genuine cold disk read (models working set > RAM).
    /// madvise(DONTNEED) alone only drops the mapping's PTEs; the page CACHE retains the pages, so
    /// re-access is a minor fault. posix_fadvise(DONTNEED) on the fd drops the clean page-cache pages
    /// (dirty ones must be flushed first) → the next access reads from disk and registers in
    /// /proc/self/io read_bytes.
    fn evict(&self) {
        use std::os::unix::io::AsRawFd;
        self.flush(); // write back this block's dirty pages so fadvise can drop them
        unsafe {
            libc::posix_fadvise(self.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            libc::madvise(
                self.mmap.as_ptr() as *mut libc::c_void,
                self.mmap.len(),
                libc::MADV_DONTNEED,
            );
        }
    }
}

// ---- /proc/self/io read_bytes + getrusage majflt ----
fn read_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}
fn majflt() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_majflt as u64
}

fn pick_leaves(rng: &mut Rng, cap: usize, k: usize, zipf: bool) -> Vec<usize> {
    let mut v = Vec::with_capacity(k);
    for _ in 0..k {
        let idx = if zipf {
            // crude locality: square-bias toward low indices (active accounts cluster)
            let r = (rng.next() as f64) / (u64::MAX as f64);
            ((r * r) * (cap as f64)) as usize
        } else {
            (rng.next() as usize) % cap
        };
        v.push(cap + idx.min(cap - 1)); // logical leaf index
    }
    v
}

fn run_layout(dir: &str, depth: u32, block: usize, blocks: usize, zipf: bool, dense: bool) {
    let name = if dense { "dense " } else { "hashed" };
    let path = format!("{dir}/disk_bench_{}.bin", if dense { "dense" } else { "hashed" });
    let cap = 1usize << depth;
    let file_gib = (2.0 * cap as f64 * 32.0) / (1024.0 * 1024.0 * 1024.0);

    let mut t = Tree::new(&path, depth, dense).expect("mmap create");
    let t0 = Instant::now();
    t.build(0xC0FFEE);
    t.flush(); // persist the built tree so the first eviction starts from a clean on-disk state
    let build_s = t0.elapsed().as_secs_f64();
    let root0 = t.root();

    let mut rng = Rng(0xABCDEF ^ (dense as u64));
    let (mut sum_bytes, mut sum_majflt, mut sum_ms) = (0u64, 0u64, 0.0f64);
    for _ in 0..blocks {
        let leaves = pick_leaves(&mut rng, cap, block, zipf);
        t.evict(); // cold: working set > RAM
        let (b0, f0) = (read_bytes(), majflt());
        let s = Instant::now();
        t.apply_block(&leaves, rng.next());
        let ms = s.elapsed().as_secs_f64() * 1000.0;
        let (db, df) = (read_bytes() - b0, majflt() - f0);
        sum_bytes += db;
        sum_majflt += df;
        sum_ms += ms;
    }
    let n = blocks as f64;
    let proof_bytes = depth as usize * 32;
    println!(
        "{name}  D={depth} N={cap} file={file_gib:.1}GiB build={build_s:.1}s root={} | \
per-block: cold_read={:.2}MiB majflt={:.0} latency={:.2}ms | proof={}B ({} sibs)",
        &root0.to_string()[..10],
        sum_bytes as f64 / n / (1024.0 * 1024.0),
        sum_majflt as f64 / n,
        sum_ms / n,
        proof_bytes,
        depth,
    );
    let _ = std::fs::remove_file(&path);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut depth = 24u32;
    let mut block = 500usize;
    let mut blocks = 40usize;
    let mut zipf = false;
    let mut layout = "both".to_string();
    let mut dir = std::env::var("DISK_BENCH_DIR").unwrap_or_else(|_| "/tmp".to_string());
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
            "--blocks" => {
                i += 1;
                blocks = args[i].parse().unwrap();
            }
            "--zipf" => zipf = true,
            "--layout" => {
                i += 1;
                layout = args[i].clone();
            }
            "--dir" => {
                i += 1;
                dir = args[i].clone();
            }
            _ => {}
        }
        i += 1;
    }
    println!(
        "# disk_bench depth={depth} block={block} blocks={blocks} zipf={zipf} dir={dir} \
(cold via madvise DONTNEED; per-block reads from /proc/self/io)"
    );
    if layout == "dense" || layout == "both" {
        run_layout(&dir, depth, block, blocks, zipf, true);
    }
    if layout == "hashed" || layout == "both" {
        run_layout(&dir, depth, block, blocks, zipf, false);
    }
}
