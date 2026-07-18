//! LEAN PAYMENT STATE — the paper's "unified store + commitment" made real and DISK-BACKED.
//!
//! Design (the ideal lean state for a payment lane):
//!   - state = 16 shards by first nibble of KeyHash; each shard is ONE redb file holding
//!     BOTH the JMT commitment nodes AND the versioned account records (the JMT value store
//!     IS the account store — one structure, no separate MPT/PlainState split like reth).
//!   - per-block apply: [parallel secp256k1 verify] -> serial balance/nonce execution
//!     (cheap, deterministic) -> 16-way PARALLEL versioned JMT update + durable persist ->
//!     lane root = keccak(16 shard roots). Versioned: any historical root remains queryable.
//!
//! Modes:
//!   lean_state --selftest                       determinism / conservation / history tests
//!   lean_state [n] [per_block] [blocks] [--sigs] [--dir path]   full pipeline benchmark
//! Defaults: n=10_000_000 per_block=4761 blocks=30, dir=/tmp/lean-state (wiped per run).

use borsh::{BorshDeserialize, BorshSerialize};
use jmt::storage::{LeafNode, Node, NodeBatch, NodeKey, TreeReader};
use jmt::{JellyfishMerkleTree, KeyHash, OwnedValue, Version};
use k256::ecdsa::{signature::Signer, signature::Verifier, Signature, SigningKey, VerifyingKey};
use rayon::prelude::*;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;
use std::time::Instant;

const SHARDS: usize = 16;
const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jmt_nodes");
// account records, key = keyhash(32) ++ version_be(8) -> record; latest <= v via range().last()
const VALUES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts_versioned");

struct Shard {
    db: Database,
    version: Version,
}

impl Shard {
    fn open(path: &Path, i: usize) -> anyhow::Result<Self> {
        let db = Database::create(path.join(format!("shard{i:02}.redb")))?;
        // ensure tables exist
        let w = db.begin_write()?;
        w.open_table(NODES)?;
        w.open_table(VALUES)?;
        w.commit()?;
        Ok(Self { db, version: 0 })
    }

    /// Apply one block's write-set for this shard: JMT update at `version`,
    /// persist nodes + values in ONE durable redb transaction, return shard root.
    fn apply(&self, writes: Vec<(KeyHash, Option<OwnedValue>)>, version: Version)
        -> anyhow::Result<[u8; 32]> {
        let tree = JellyfishMerkleTree::<Shard, sha2::Sha256>::new(self);
        let (root, tub) = tree.put_value_set(writes, version)?;
        let w = self.db.begin_write()?;
        {
            let mut nt = w.open_table(NODES)?;
            for (k, v) in tub.node_batch.nodes() {
                nt.insert(borsh::to_vec(k)?.as_slice(), borsh::to_vec(v)?.as_slice())?;
            }
            let mut vt = w.open_table(VALUES)?;
            for ((ver, kh), val) in tub.node_batch.values() {
                let ver: u64 = *ver;
                let mut key = kh.0.to_vec();
                key.extend_from_slice(&ver.to_be_bytes());
                vt.insert(key.as_slice(), borsh::to_vec(val)?.as_slice())?;
            }
        }
        w.commit()?; // durable persist — this is the honest "on disk" point
        Ok(root.0)
    }

    fn get_latest(&self, kh: &KeyHash, max_version: Version) -> anyhow::Result<Option<OwnedValue>> {
        let r = self.db.begin_read()?;
        let vt = r.open_table(VALUES)?;
        let mut lo = kh.0.to_vec(); lo.extend_from_slice(&0u64.to_be_bytes());
        let mut hi = kh.0.to_vec(); hi.extend_from_slice(&max_version.to_be_bytes());
        let found = vt.range(lo.as_slice()..=hi.as_slice())?
            .last().transpose()?
            .map(|(_, v)| Option::<OwnedValue>::deserialize(&mut v.value()).unwrap().unwrap_or_default());
        Ok(found.filter(|v| !v.is_empty()))
    }
}

impl TreeReader for Shard {
    fn get_node_option(&self, key: &NodeKey) -> anyhow::Result<Option<Node>> {
        let r = self.db.begin_read()?;
        let nt = r.open_table(NODES)?;
        Ok(nt.get(borsh::to_vec(key)?.as_slice())?
            .map(|v| Node::deserialize(&mut v.value()).unwrap()))
    }
    fn get_value_option(&self, ver: Version, kh: KeyHash) -> anyhow::Result<Option<OwnedValue>> {
        self.get_latest(&kh, ver)
    }
    fn get_rightmost_leaf(&self) -> anyhow::Result<Option<(NodeKey, LeafNode)>> { Ok(None) }
}

// ---------------- account model ----------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Account { nonce: u64, balance: u128 }

fn enc(a: &Account) -> OwnedValue {
    let mut v = vec![0u8; 24];
    v[..8].copy_from_slice(&a.nonce.to_le_bytes());
    v[8..24].copy_from_slice(&a.balance.to_le_bytes());
    v
}
fn dec(v: &[u8]) -> Account {
    Account {
        nonce: u64::from_le_bytes(v[..8].try_into().unwrap()),
        balance: u128::from_le_bytes(v[8..24].try_into().unwrap()),
    }
}

fn key_of(i: u64) -> KeyHash {
    let mut addr = [0u8; 20];
    addr[12..].copy_from_slice(&(0x20_0000_0000u64 + i).to_be_bytes());
    KeyHash::with::<sha2::Sha256>(addr)
}
fn shard_of(kh: &KeyHash) -> usize { (kh.0[0] >> 4) as usize }

struct LeanState {
    shards: Vec<Shard>,
    version: Version, // block height; all shards move in lockstep
}

impl LeanState {
    fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let shards = (0..SHARDS).map(|i| Shard::open(dir, i)).collect::<Result<_, _>>()?;
        Ok(Self { shards, version: 0 })
    }

    fn account(&self, kh: &KeyHash) -> anyhow::Result<Account> {
        Ok(self.shards[shard_of(kh)]
            .get_latest(kh, self.version.saturating_sub(1).max(0))?
            .map(|v| dec(&v)).unwrap_or_default())
    }

    /// Apply a block of (from, to, amount) transfers. Returns (lane_root, phase timings ms).
    fn apply_block(&mut self, transfers: &[(KeyHash, KeyHash, u128)])
        -> anyhow::Result<([u8; 32], f64, f64)> {
        use std::collections::HashMap;
        let t0 = Instant::now();
        // EXECUTE (serial; balance RMW is trivially cheap — measured in the contention study)
        let mut touched: HashMap<KeyHash, Account> = HashMap::with_capacity(transfers.len() * 2);
        for (from, to, amt) in transfers {
            let mut fa = match touched.get(from) { Some(a) => *a, None => self.account(from)? };
            let mut ta = match touched.get(to) { Some(a) => *a, None => self.account(to)? };
            if fa.balance >= *amt {
                fa.balance -= amt; fa.nonce += 1; ta.balance += amt;
                touched.insert(*from, fa); touched.insert(*to, ta);
            }
        }
        let exec_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // COMMIT: partition write-set, 16-way parallel JMT + durable persist
        let t1 = Instant::now();
        let mut per_shard: Vec<Vec<(KeyHash, Option<OwnedValue>)>> =
            (0..SHARDS).map(|_| Vec::new()).collect();
        for (kh, acct) in &touched {
            per_shard[shard_of(kh)].push((*kh, Some(enc(acct))));
        }
        let v = self.version;
        let roots: Vec<[u8; 32]> = self.shards.par_iter().zip(per_shard.into_par_iter())
            .map(|(shard, writes)| shard.apply(writes, v))
            .collect::<Result<_, _>>()?;
        self.version += 1;
        let commit_ms = t1.elapsed().as_secs_f64() * 1000.0;

        use tiny_keccak::{Hasher as _, Keccak};
        let mut h = Keccak::v256();
        for r in &roots { h.update(r); }
        let mut root = [0u8; 32]; h.finalize(&mut root);
        Ok((root, exec_ms, commit_ms))
    }
}

// ---------------- self-tests ----------------

fn selftest() -> anyhow::Result<()> {
    let d1 = std::env::temp_dir().join("lean-selftest-a");
    let d2 = std::env::temp_dir().join("lean-selftest-b");
    for d in [&d1, &d2] { let _ = std::fs::remove_dir_all(d); }
    let mut a = LeanState::open(&d1)?;
    let mut b = LeanState::open(&d2)?;

    // genesis: 1000 accounts x 1e18
    let genesis: Vec<(KeyHash, KeyHash, u128)> = vec![];
    let seed: Vec<(KeyHash, Option<OwnedValue>)> =
        (0..1000).map(|i| (key_of(i), Some(enc(&Account { nonce: 0, balance: 10u128.pow(18) })))).collect();
    for st in [&mut a, &mut b] {
        let mut per: Vec<Vec<_>> = (0..SHARDS).map(|_| Vec::new()).collect();
        for (kh, v) in &seed { per[shard_of(kh)].push((*kh, v.clone())); }
        let ver = st.version;
        for (s, w) in st.shards.iter().zip(per.into_iter()) { s.apply(w, ver)?; }
        st.version += 1;
        let _ = &genesis;
    }

    // three blocks of deterministic transfers
    let mut roots_a = Vec::new();
    let mut roots_b = Vec::new();
    for blk in 0..3u64 {
        let txs: Vec<(KeyHash, KeyHash, u128)> =
            (0..500).map(|k| (key_of((blk * 7 + k) % 1000), key_of((k * 13 + 1) % 1000), 1_000)).collect();
        roots_a.push(a.apply_block(&txs)?.0);
        roots_b.push(b.apply_block(&txs)?.0);
    }
    assert_eq!(roots_a, roots_b, "DETERMINISM: independent instances must agree");
    assert_ne!(roots_a[0], roots_a[1], "roots must change across blocks");

    // conservation: total balance unchanged
    let total: u128 = (0..1000).map(|i| a.account(&key_of(i)).unwrap().balance).sum();
    assert_eq!(total, 1000 * 10u128.pow(18), "CONSERVATION");

    // history: version-0 record still readable (versioned store)
    let old = a.shards[shard_of(&key_of(0))].get_latest(&key_of(0), 0)?;
    assert!(old.is_some(), "HISTORY: version-0 value queryable");
    assert_eq!(dec(&old.unwrap()).balance, 10u128.pow(18));

    println!("SELFTEST PASS: determinism across instances, root progression, conservation, versioned history");
    Ok(())
}

// ---------------- benchmark ----------------

fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "--selftest") { return selftest(); }
    let nums: Vec<u64> = argv.iter().skip(1).filter_map(|a| a.parse().ok()).collect();
    let n = *nums.first().unwrap_or(&10_000_000);
    let per_block = *nums.get(1).unwrap_or(&4761) as usize;
    let blocks = *nums.get(2).unwrap_or(&30);
    let with_sigs = argv.iter().any(|a| a == "--sigs");
    let dir = argv.iter().position(|a| a == "--dir")
        .map(|i| std::path::PathBuf::from(&argv[i + 1]))
        .unwrap_or_else(|| "/tmp/lean-state".into());
    let _ = std::fs::remove_dir_all(&dir);
    let mut st = LeanState::open(&dir)?;

    println!("[lean-state] building {n} accounts on disk at {dir:?} (16 shards, durable commits)...");
    let t0 = Instant::now();
    let chunk = 200_000u64;
    let mut i = 0;
    while i < n {
        let hi = (i + chunk).min(n);
        let mut per: Vec<Vec<_>> = (0..SHARDS).map(|_| Vec::new()).collect();
        for j in i..hi {
            let kh = key_of(j);
            per[shard_of(&kh)].push((kh, Some(enc(&Account { nonce: 0, balance: 10u128.pow(18) }))));
        }
        let ver = st.version;
        st.shards.par_iter().zip(per.into_par_iter())
            .map(|(s, w)| s.apply(w, ver).map(|_| ()))
            .collect::<Result<Vec<_>, _>>()?;
        st.version += 1;
        i = hi;
        if i % 2_000_000 == 0 { println!("  {i:>9} accts {:>6.1}s", t0.elapsed().as_secs_f64()); }
    }
    let du: u64 = walk_size(&dir);
    println!("[lean-state] build: {:.1}s, on-disk {} MB", t0.elapsed().as_secs_f64(), du / 1_000_000);

    // optional: real signed transfers (sign off-clock, verify on-clock)
    let keys: Vec<SigningKey> = if with_sigs {
        (0..64).map(|i| SigningKey::from_bytes((&[i as u8 + 1; 32]).into()).unwrap()).collect()
    } else { vec![] };

    println!("[lean-state] measuring {blocks} blocks x {per_block} transfers (sigs={with_sigs})...");
    let mut cursor = 0u64;
    let (mut ve, mut ex, mut cm) = (0f64, 0f64, 0f64);
    let mut times = Vec::new();
    for _ in 0..blocks {
        let txs: Vec<(KeyHash, KeyHash, u128)> = (0..per_block as u64)
            .map(|k| { let a = (cursor + k) % n; let b = (cursor + k + n / 2) % n;
                       (key_of(a), key_of(b), 1_000u128) }).collect();
        cursor = (cursor + per_block as u64) % n;
        // signatures: SIGN off-clock (sender-side work), VERIFY on-clock in parallel (node work)
        let sigs: Vec<(VerifyingKey, [u8; 32], Signature)> = if with_sigs {
            (0..per_block).map(|k| {
                let ki = k % keys.len();
                let m = *key_of(k as u64).0.as_slice().first_chunk::<32>().unwrap();
                (*keys[ki].verifying_key(), m, keys[ki].sign(&m))
            }).collect()
        } else { vec![] };
        let tv = Instant::now();
        if with_sigs {
            assert!(sigs.par_iter().all(|(vk, m, sg)| vk.verify(m, sg).is_ok()));
        }
        let verify_ms = tv.elapsed().as_secs_f64() * 1000.0;
        let (_root, exec_ms, commit_ms) = st.apply_block(&txs)?;
        ve += verify_ms; ex += exec_ms; cm += commit_ms;
        times.push(verify_ms + exec_ms + commit_ms);
    }
    times.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let b = blocks as f64;
    println!("[lean-state] @{}M accts, {} tx/blk: TOTAL avg {:.2} ms/blk (p50 {:.2} p95 {:.2})",
             n / 1_000_000, per_block,
             (ve + ex + cm) / b, times[times.len() / 2], times[(times.len() as f64 * 0.95) as usize]);
    println!("  phases: sig-verify {:.2} | execute {:.2} | commit+persist(JMT x16, durable) {:.2} ms/blk",
             ve / b, ex / b, cm / b);
    println!("  per-tx: {:.1} µs   (reference: live lane MPT root alone 2.6-12 ms; naive JMT 80.9 ms)",
             (ve + ex + cm) / b * 1000.0 / per_block as f64);
    Ok(())
}

fn walk_size(p: &Path) -> u64 {
    std::fs::read_dir(p).map(|d| d.flatten()
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum()).unwrap_or(0)
}
