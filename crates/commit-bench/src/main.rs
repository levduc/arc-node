//! Controlled MPT-vs-SALT benchmark over an IDENTICAL account set, at arbitrary scale.
//!
//! WHY THIS EXISTS
//! The live fleet can only reach state that fits in RAM, and there the MPT always wins: measured
//! 2.79x (250k accounts) and 2.40x (5M). Extrapolating MPT's ~21%-per-20x growth, trie size alone
//! would need ~10^6 x more state to close that gap -- inside the page-cached regime the MPT simply
//! wins. A crossover requires the STEP CHANGE when trie lookups start missing cache and become disk
//! seeks. This harness reaches that regime directly.
//!
//! It also avoids the two things that made the live comparison impossible at scale:
//!   - the real 169 GB Arc snapshot is full EVM state (contracts + storage); our SALT integration
//!     commits (nonce, balance) only, so it cannot represent that state at all. Here BOTH sides see
//!     the same accounts-only set, so the storage gap is not a confound.
//!   - genesis preseeding pins the whole alloc in unevictable RSS (~250 B/account), which fights
//!     any attempt to cap memory. Here the MPT's state lives in MDBX on disk, where the OS can
//!     evict it, which is the entire point.
//!
//! METHOD
//!   1. Populate a real MDBX `HashedAccounts` table with N accounts (reth's own table/codec).
//!   2. Per "block": pick k accounts, bump their balances, and time BOTH
//!        - MPT : reth's `StateRoot::overlay_root` over that MDBX tx -- the exact code path a
//!                payment EL runs, disk-backed and subject to page-cache eviction.
//!        - SALT: `salt_commitment::readonly_root` over the same k changes.
//!   3. Report per-block cost vs N, so the crossover (if any) is visible.
//!
//! Run cold (the regime that matters) by dropping caches between phases, or under a cgroup:
//!   systemd-run --user --scope -p MemoryMax=4G -p MemorySwapMax=0 \
//!     target/release/commit-bench --accounts 200000000 --changed 200 --blocks 50
//!
//! HONEST SCOPE: this measures the COMMITMENT step only (root computation), not execution or
//! persistence. Those were controlled and near-identical on the live fleet (exec 3.34/3.00,
//! persist 49.1/47.8), which is why isolating the root here is legitimate. SALT is still favoured
//! by committing a narrower leaf and persisting no nodes -- see FAIR-COMPARE.md.
use alloy_primitives::{B256, U256};
use reth_db::{mdbx::DatabaseArguments, ClientVersion, DatabaseEnv};
use reth_db_api::{cursor::DbCursorRW, database::Database, transaction::{DbTx, DbTxMut}};
use std::time::Instant;

fn arg(name: &str, default: u64) -> u64 {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Deterministic hashed address for account i (stands in for keccak(addr)).
fn key(i: u64) -> B256 {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&i.to_be_bytes());
    // spread across the keyspace so the trie is realistically wide, not a dense prefix run
    let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    b[8..16].copy_from_slice(&h.to_be_bytes());
    B256::from(b)
}

/// Measure reth's MPT root against an EXISTING datadir (e.g. the real Arc snapshot), read-only.
///
/// This is the only way to reach the regime that matters: the Arc testnet snapshot is a 168 GB
/// mdbx.dat on a 78 GB machine, so the trie CANNOT be page-cached and every lookup is a real disk
/// seek. Synthetic populate-then-measure cannot reproduce that -- it bulk-loads a contiguous,
/// cache-friendly layout (measured: same ~5M accounts cost 3.23 ms bulk-loaded vs 8.62 ms grown
/// organically).
///
/// Accounts are sampled by SEEKING to pseudo-random keys, so access is scattered across the
/// keyspace the way real transaction load is -- not a sequential walk of one hot page.
///
/// SALT does not participate here: at the measured ~1.83 KB/account of the shipped MemStore, a
/// state this size would need hundreds of GB of unevictable heap. That limitation is the finding,
/// not an omission.
/// Seed SALT from the REAL snapshot's HashedAccounts, in chunks, reporting RSS as it grows.
/// Replaces an extrapolation ("SALT would need ~81 GB for 44.4M accounts, so it cannot fit") with
/// an actual attempt. Either it fits -- and we can then measure SALT on real Arc state -- or it
/// OOMs, which confirms the limit empirically instead of by arithmetic.
fn seed_salt_from_snapshot(path: &str, limit: u64) -> eyre::Result<u64> {
    use reth_db_api::cursor::DbCursorRO;
    let db = reth_db::open_db_read_only(std::path::Path::new(path), Default::default())?;
    let mut total = 0u64;
    let t0 = Instant::now();
    // MDBX aborts a read transaction held too long ("read transaction has been timed out",
    // -96000) -- seeding 44M accounts takes minutes, so ONE long-lived tx cannot be used. Re-open
    // a fresh tx per chunk and resume from the last key seen.
    let mut resume: Option<B256> = None;
    loop {
        let tx = db.tx()?;
        let mut cur = tx.cursor_read::<reth_db_api::tables::HashedAccounts>()?;
        let mut entry = match resume {
            None => cur.first()?,
            Some(k) => {
                let e = cur.seek(k)?;
                // seek lands ON the resume key, which was already committed; step past it
                if e.map(|(kk, _)| kk) == Some(k) { cur.next()? } else { e }
            }
        };
        let mut batch: Vec<(B256, Option<Vec<u8>>)> = Vec::with_capacity(1_000_000);
        let mut done = true;
        while let Some((k, a)) = entry {
            batch.push((
                k,
                Some(arc_payment_commitment::salt_commitment::encode_account_leaf(
                    a.nonce,
                    B256::from(a.balance.to_be_bytes::<32>()),
                )),
            ));
            resume = Some(k);
            if batch.len() == 1_000_000 {
                done = false; // more may remain; close this tx and continue with a fresh one
                break;
            }
            entry = cur.next()?;
        }
        drop(cur);
        drop(tx);
        if batch.is_empty() { break; }
        let n = batch.len() as u64;
        arc_payment_commitment::salt_commitment::commit(&batch);
        total += n;
        println!("  seeded {total:>10} accounts  rss={:.1} GB  elapsed={:.0?}", rss_gb(), t0.elapsed());
        if done { break; }
        if limit > 0 && total >= limit { break; }
    }
    println!("  SEEDED {total} accounts into SALT, rss={:.1} GB, {:.0?}", rss_gb(), t0.elapsed());
    Ok(total)
}

fn rss_gb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok())
            })
        })
        .map(|kb| kb / 1048576.0)
        .unwrap_or(0.0)
}

fn existing(path: &str, k_changed: u64, n_blocks: u64) -> eyre::Result<()> {
    use reth_db_api::cursor::DbCursorRO;
    // --with-salt: seed SALT from this same state first, so both structures hold it
    if std::env::args().any(|a| a == "--with-salt") {
        let limit = arg("--salt-limit", 0);
        println!("seeding SALT from the real snapshot (limit={} 0=all)…", limit);
        seed_salt_from_snapshot(path, limit)?;
    }
    println!("opening EXISTING datadir read-only: {path}");
    let db = reth_db::open_db_read_only(std::path::Path::new(path), Default::default())?;

    // Pseudo-random probe keys. --seed MUST be varied between runs: with a fixed seed every run
    // samples the SAME accounts, so run 2 onwards measures PAGE-CACHE HITS, not trie work. That
    // error produced a bogus "357 ms, disk-bound" reading (run 1, cold) followed by 19.8 ms with
    // ZERO disk reads (run 2, same accounts, cached).
    let mut st: u64 = arg("--seed", 0x243F_6A88_85A3_08D3);
    let mut next = || { st ^= st << 13; st ^= st >> 7; st ^= st << 17; st };

    let with_salt = std::env::args().any(|a| a == "--with-salt");
    println!("{:>6} {:>10} {:>14} {:>14}", "block", "sampled", "MPT root ms", "SALT root ms");
    let mut times = Vec::new();
    let mut salt_times: Vec<f64> = Vec::new();
    for b in 0..n_blocks {
        // sample k existing accounts by seeking to scattered keys
        let tx = db.tx()?;
        let mut changes = Vec::new();
        {
            let mut cur = tx.cursor_read::<reth_db_api::tables::HashedAccounts>()?;
            for _ in 0..k_changed {
                let mut probe = [0u8; 32];
                for c in probe.chunks_mut(8) { c.copy_from_slice(&next().to_be_bytes()); }
                if let Ok(Some((key, mut acct))) = cur.seek(B256::from(probe)) {
                    acct.nonce += 1;
                    acct.balance = acct.balance.saturating_add(U256::from(1u64));
                    changes.push((key, acct));
                }
            }
        }
        let mut post = reth_trie::HashedPostState::default();
        for (k, a) in &changes { post.accounts.insert(*k, Some(*a)); }
        let sorted = post.into_sorted();

        let t = Instant::now();
        let _root = <reth_trie::StateRoot<
            reth_trie_db::DatabaseTrieCursorFactory<_, reth_trie_db::LegacyKeyAdapter>,
            reth_trie_db::DatabaseHashedCursorFactory<_>,
        > as reth_trie_db::DatabaseStateRoot<_>>::overlay_root(&tx, &sorted)?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        drop(tx);
        times.push(ms);

        // SALT over the SAME sampled accounts, on the SAME state (all 44.4M seeded above)
        let mut salt_ms = f64::NAN;
        if with_salt {
            let salt_changes: Vec<(B256, Option<Vec<u8>>)> = changes
                .iter()
                .map(|(k, a)| {
                    (
                        *k,
                        Some(arc_payment_commitment::salt_commitment::encode_account_leaf(
                            a.nonce,
                            B256::from(a.balance.to_be_bytes::<32>()),
                        )),
                    )
                })
                .collect();
            let t = Instant::now();
            let _ = arc_payment_commitment::salt_commitment::readonly_root(&salt_changes);
            salt_ms = t.elapsed().as_secs_f64() * 1000.0;
            salt_times.push(salt_ms);
        }
        println!("{b:>6} {:>10} {ms:>14.3} {salt_ms:>14.3}", changes.len());
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = times[times.len() / 2];
    println!("
=== MPT on REAL Arc state, {k_changed} changed/block ===");
    println!("first (coldest) {:.3} ms | median {:.3} ms | max {:.3} ms",
             times.first().copied().unwrap_or(0.0), med, times.last().copied().unwrap_or(0.0));
    if !salt_times.is_empty() {
        salt_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sm = salt_times[salt_times.len() / 2];
        println!("SALT median {sm:.3} ms  (all 44.4M accounts resident)");
        println!("=> SALT faster by {:.1}x on REAL Arc state", med / sm);
    }
    println!("compare: 0.246 ms synthetic-cached 1M, 3.23 ms live fleet 5M preseeded");
    Ok(())
}

mod primitives;

fn main() -> eyre::Result<()> {
    if std::env::args().any(|a| a == "--primitives") {
        let n = arg("--accounts", 5_000_000);
        let k = arg("--changed", 200);
        let rounds = arg("--rounds", 20);
        let rows = primitives::run(n, k, rounds)?;
        primitives::print(&rows, n, k);
        return Ok(());
    }
    // --count <datadir/db>: how many accounts are actually in this state? Read from MDBX table
    // metadata (instant), not a walk. Needed before claiming SALT can or cannot hold it -- that
    // claim was previously made on an ASSUMED account count, which is not good enough.
    if let Some(i) = std::env::args().position(|a| a == "--count") {
        let path = std::env::args().nth(i + 1).expect("--count <datadir/db>");
        let db = reth_db::open_db_read_only(std::path::Path::new(&path), Default::default())?;
        let tx = db.tx()?;
        let accounts = tx.entries::<reth_db_api::tables::HashedAccounts>()?;
        let storages = tx.entries::<reth_db_api::tables::HashedStorages>()?;
        let acct_trie = tx.entries::<reth_db_api::tables::AccountsTrie>()?;
        println!("HashedAccounts  {accounts:>14}");
        println!("HashedStorages  {storages:>14}");
        println!("AccountsTrie    {acct_trie:>14}");
        println!("\nSALT MemStore at measured 1.83 KB/account:");
        println!("  accounts only -> {:.1} GB", accounts as f64 * 1830.0 / 1e9);
        return Ok(());
    }
    if let Some(i) = std::env::args().position(|a| a == "--existing") {
        let path = std::env::args().nth(i + 1).expect("--existing <datadir>");
        return existing(&path, arg("--changed", 200), arg("--blocks", 20));
    }
    let n_accounts = arg("--accounts", 5_000_000);
    let k_changed = arg("--changed", 200);
    let n_blocks = arg("--blocks", 30);
    let dir = std::env::var("BENCH_DIR").unwrap_or_else(|_| "/tmp/commit-bench-db".into());

    println!("accounts={n_accounts} changed/block={k_changed} blocks={n_blocks} db={dir}");
    std::fs::create_dir_all(&dir)?;
    let db = reth_db::init_db(&dir, DatabaseArguments::new(ClientVersion::default()))?;

    // ---- populate: N accounts into reth's real HashedAccounts table ----
    let t0 = Instant::now();
    let mut written = 0u64;
    const BATCH: u64 = 200_000;
    while written < n_accounts {
        let tx = db.tx_mut()?;
        {
            let mut cur = tx.cursor_write::<reth_db_api::tables::HashedAccounts>()?;
            let end = (written + BATCH).min(n_accounts);
            for i in written..end {
                cur.append(
                    key(i),
                    &reth_primitives_traits::Account {
                        nonce: 1,
                        balance: U256::from(1_000_000u64),
                        bytecode_hash: None,
                    },
                )?;
            }
            written = end;
        }
        tx.commit()?;
        if written % 2_000_000 == 0 {
            println!("  populated {written}/{n_accounts} ({:.0?})", t0.elapsed());
        }
    }
    println!("populated {n_accounts} accounts in {:.1?}", t0.elapsed());
    println!(
        "  on-disk: {:.2} GB  (compare against the memory cap to know if you are past RAM)",
        fs_size(&dir) as f64 / 1e9
    );

    // ---- build the MPT's intermediate trie nodes, then SEED SALT with the same N accounts ----
    // Without this both sides measure the wrong thing: reth's overlay_root with an EMPTY
    // AccountsTrie rebuilds the whole trie every block (O(N) from scratch, measured 815 ms at 1M
    // vs 3.23 ms at 5M on the live fleet -- the tell that it was wrong), and SALT would hold only
    // the k keys it was asked to commit rather than all N. Both must hold N and update k.
    let t = Instant::now();
    {
        let tx = db.tx()?;
        let sr = <reth_trie::StateRoot<
            reth_trie_db::DatabaseTrieCursorFactory<_, reth_trie_db::LegacyKeyAdapter>,
            reth_trie_db::DatabaseHashedCursorFactory<_>,
        > as reth_trie_db::DatabaseStateRoot<_>>::from_tx(&tx);
        let (_root, updates) = sr.root_with_updates()?;
        drop(tx);
        let tx = db.tx_mut()?;
        {
            let mut cur = tx.cursor_write::<reth_db_api::tables::AccountsTrie>()?;
            let mut nodes: Vec<(reth_trie::Nibbles, _)> = updates
                .account_nodes_ref()
                .iter()
                .map(|(p, n)| (*p, n.clone()))
                .collect();
            nodes.sort_by(|a, b| a.0.cmp(&b.0));
            for (path, node) in &nodes {
                cur.upsert(reth_trie::StoredNibbles(*path), node)?;
            }
        }
        tx.commit()?;
    }
    println!("built MPT intermediate trie nodes in {:.1?}", t.elapsed());

    let t = Instant::now();
    arc_payment_commitment::salt_commitment::ensure_seeded(|| {
        (0..n_accounts)
            .map(|i| {
                (
                    key(i),
                    Some(arc_payment_commitment::salt_commitment::encode_account_leaf(
                        1,
                        B256::from(U256::from(1_000_000u64).to_be_bytes::<32>()),
                    )),
                )
            })
            .collect()
    });
    println!("seeded SALT with {n_accounts} accounts in {:.1?}", t.elapsed());

    // NOTE: append() requires ascending keys, so accounts are inserted in sorted order. That is
    // the FRIENDLIEST possible layout for the MPT -- a real chain scatters writes over time and
    // fragments the store (measured: same ~5M accounts cost the MPT 8.62 ms grown organically vs
    // 3.23 ms bulk-loaded). So any MPT number here is a LOWER BOUND on its real cost.

    println!("\n{:>6} {:>14} {:>14}   {}", "block", "MPT root ms", "SALT root ms", "ratio");
    let mut mpt = Vec::new();
    let mut salt = Vec::new();
    for b in 0..n_blocks {
        // same k accounts change for both structures
        let changes: Vec<(B256, reth_primitives_traits::Account)> = (0..k_changed)
            .map(|j| {
                let i = (b * k_changed + j) % n_accounts;
                (
                    key(i),
                    reth_primitives_traits::Account {
                        nonce: 2 + b,
                        balance: U256::from(1_000_000u64 + b),
                        bytecode_hash: None,
                    },
                )
            })
            .collect();

        // ---- MPT: reth's own overlay_root over the MDBX tx (disk-backed) ----
        let mut post = reth_trie::HashedPostState::default();
        for (k, a) in &changes {
            post.accounts.insert(*k, Some(*a));
        }
        let sorted = post.into_sorted();
        let tx = db.tx()?;
        let t = Instant::now();
        let _root = <reth_trie::StateRoot<
            reth_trie_db::DatabaseTrieCursorFactory<_, reth_trie_db::LegacyKeyAdapter>,
            reth_trie_db::DatabaseHashedCursorFactory<_>,
        > as reth_trie_db::DatabaseStateRoot<_>>::overlay_root(&tx, &sorted)?;
        let mpt_ms = t.elapsed().as_secs_f64() * 1000.0;
        drop(tx);

        // ---- SALT: same k changes ----
        let salt_changes: Vec<(B256, Option<Vec<u8>>)> = changes
            .iter()
            .map(|(k, a)| {
                (
                    *k,
                    Some(arc_payment_commitment::salt_commitment::encode_account_leaf(
                        a.nonce,
                        B256::from(a.balance.to_be_bytes::<32>()),
                    )),
                )
            })
            .collect();
        let t = Instant::now();
        let _ = arc_payment_commitment::salt_commitment::readonly_root(&salt_changes);
        let salt_ms = t.elapsed().as_secs_f64() * 1000.0;

        mpt.push(mpt_ms);
        salt.push(salt_ms);
        if b < 5 || b % 10 == 0 {
            println!("{b:>6} {mpt_ms:>14.3} {salt_ms:>14.3}   {:.2}x", salt_ms / mpt_ms.max(1e-9));
        }
    }

    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (m, s) = (med(&mut mpt), med(&mut salt));
    println!("\n=== {n_accounts} accounts, {k_changed} changed/block ===");
    println!("MPT  median {m:.3} ms");
    println!("SALT median {s:.3} ms");
    if s < m {
        println!("=> SALT faster by {:.2}x  <-- CROSSOVER", m / s);
    } else {
        println!("=> MPT faster by {:.2}x", s / m);
    }
    println!("\nBoth structures hold all {n_accounts} accounts and update {k_changed}/block.");
    Ok(())
}

fn fs_size(p: &str) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}
