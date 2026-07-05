// Real Ethereum MPT baseline (alloy-trie) — replaces hand-waving about "the MPT" with the actual
// structure's per-key footprint. Isolates two real numbers the same-shape binary "hashed" proxy
// could not give:
//   * path nodes per key   = the random nodes an update touches (the beyond-RAM I/O driver)
//   * real proof bytes/key = the actual Ethereum MPT witness (16-ary branch nodes carry 16 hashes
//                            each), vs a binary Merkle branch of depth*32 B.
//
// Method: build the real Ethereum MPT (keccak-keyed accounts, the genuine random keyspace) with
// alloy-trie's HashBuilder + ProofRetainer; extract each target key's path nodes. This measures MPT
// STRUCTURE faithfully (real root, real nodes). It does NOT model on-disk KV I/O (HashBuilder streams
// in RAM) — that remains the deferred full MPT-over-KV build; but the path-node count + byte size are
// exactly what drive that I/O, measured on the real thing.
//
// Usage: mpt_baseline --count 4000000 --targets 400

use std::time::Instant;

use alloy_primitives::{keccak256, B256};
use alloy_trie::{proof::ProofRetainer, HashBuilder, Nibbles};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut count: usize = 1_000_000;
    let mut targets_n: usize = 300;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--count" => {
                i += 1;
                count = args[i].parse().unwrap();
            }
            "--targets" => {
                i += 1;
                targets_n = args[i].parse().unwrap();
            }
            _ => {}
        }
        i += 1;
    }

    // Real random keyspace: account key = keccak(address); here address = keccak(index) so keys are
    // uniformly distributed 32-byte hashes, exactly as Ethereum's MPT sees them.
    let t = Instant::now();
    let mut keys: Vec<B256> = (0..count)
        .map(|j| keccak256(keccak256((j as u64).to_le_bytes()).as_slice()))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let gen = t.elapsed();

    // Representative account leaf value (~70 B RLP: nonce, balance, storageRoot, codeHash).
    let value = [0x11u8; 70];

    // Retain proofs for evenly-spaced target keys.
    let step = (keys.len() / targets_n).max(1);
    let targets: Vec<Nibbles> = (0..targets_n)
        .map(|j| Nibbles::unpack(keys[(j * step).min(keys.len() - 1)].as_slice()))
        .collect();

    let retainer = ProofRetainer::new(targets.clone());
    let mut hb = HashBuilder::default().with_proof_retainer(retainer);

    let t = Instant::now();
    for k in &keys {
        hb.add_leaf(Nibbles::unpack(k.as_slice()), &value);
    }
    let root = hb.root();
    let build = t.elapsed();

    let proofs = hb.take_proof_nodes();
    let (mut tot_nodes, mut tot_bytes, mut max_nodes) = (0usize, 0usize, 0usize);
    for tk in &targets {
        let nodes = proofs.matching_nodes(tk);
        tot_nodes += nodes.len();
        max_nodes = max_nodes.max(nodes.len());
        for (_p, b) in nodes {
            tot_bytes += b.len();
        }
    }
    let n = targets.len() as f64;
    let depth_bin = (keys.len() as f64).log2().ceil();

    println!(
        "# mpt_baseline count={} (unique {}) targets={} | keygen={:.1}s build+root={:.1}s root={}",
        count,
        keys.len(),
        targets.len(),
        gen.as_secs_f64(),
        build.as_secs_f64(),
        &root.to_string()[..10],
    );
    println!(
        "REAL MPT     : path nodes/key = {:.1} (max {}) | proof bytes/key = {:.0} B",
        tot_nodes as f64 / n,
        max_nodes,
        tot_bytes as f64 / n,
    );
    println!(
        "BINARY Merkle: path nodes/key = {:.0}       | proof bytes/key = {:.0} B  (depth*32)",
        depth_bin,
        depth_bin * 32.0,
    );
    println!(
        "# real MPT touches FEWER, LARGER nodes (16-ary): {:.1} nodes vs {:.0}; proof {:.0}B vs {:.0}B.",
        tot_nodes as f64 / n,
        depth_bin,
        tot_bytes as f64 / n,
        depth_bin * 32.0,
    );
}
