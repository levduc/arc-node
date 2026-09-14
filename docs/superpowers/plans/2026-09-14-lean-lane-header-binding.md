# Lean Lane v0.2 — Header Binding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind the lean payment-lane block into the EVM block header (`prev_randao`) so consensus votes on the plain EVM block hash, the lean node holds the lean bytes at decide time, and the CL delta shrinks.

**Architecture:** Two repos. `lean-lane` (node) gains a commitment index and commitment-addressed verbs (`arc_getBlockBytes{commitment}`, `arc_newBlock{commitment}`). `arc-node` (CL) builds the lean block before the EVM payload, passes its commitment as `prev_randao`, validates header/lean agreement, anchors by commitment at decide, and drops the two-lane value id, the store keying, the in-memory stash and the sync offset scan.

**Tech Stack:** Rust (stable per each repo's `rust-toolchain.toml`), tokio, jsonrpsee (node RPC), reqwest + serde_json (CL shim client), mockall (CL unit tests), quake + docker compose (testnets), tailscale ssh (fleet).

**Spec:** `docs/superpowers/specs/2026-09-14-lean-lane-header-binding-design.md` (this branch). Read it first; every task below cites its section.

## Global Constraints

- arc-node work is on branch `lean-lane-v0.2` in worktree `/home/papaduck/arc-lean-v0.1` (base `main@97f8da0`, reth v2.2.0). Never commit to `main` or `lean-lane-v0.1`.
- lean-lane work is on branch `v0.2` in `/home/papaduck/lean-lane`. Every v0.1 RPC form must keep working (spec §4).
- The CL crate is `arc-node-consensus`; check with `cargo check -p arc-node-consensus --tests`. `cargo check -p arc-malachitebft-app` checks nothing.
- Flag off (`ARC_PAYMENT_LEAN_LANE` unset) must remain upstream behaviour and upstream wire bytes (spec §5, §5.9). Every lane arm sits behind `lean_shim.is_some()`.
- Lean block commitment formula is unchanged (spec §3); the CL never trusts a claimed commitment.
- Numbers reported anywhere must carry cadence and fullness (CLAUDE.md §6).
- On fleet machines every artefact of a run lives under `/home/papaduck/arc-runs/<run-id>/`; nothing else is created or removed in `$HOME` (spec §8.5).
- Commit messages end with the two attribution trailers used on this branch (see `git log -1 --format=%B`).
- `cli_db_migrate::test_migrate_command_without_home_flag` fails on this machine with the default `HOME`; it is pre-existing and passes with `HOME=$(mktemp -d)`. It is not a regression.

---

## File map

**lean-lane (`/home/papaduck/lean-lane`)**
- Modify `crates/lean-lane-node/src/node.rs` — commitment index, `block_wire_bytes_by_commitment`, `shim_new_by_commitment`, staged retention.
- Modify `crates/lean-lane-node/src/rpc.rs` — `arc_getBlockBytes{commitment}`, `arc_newBlock{commitment}`.
- Create `crates/lean-lane-node/tests/by_commitment.rs` — tests for both.
- Modify `docs/integration.md` — v0.2 verb table.

**arc-node (`/home/papaduck/arc-lean-v0.1`)**
- Modify `crates/eth-engine/src/lean_shim.rs` — two new client calls, `LeanBuilder` trait.
- Modify `crates/eth-engine/src/engine.rs` — `generate_block` takes `prev_randao`.
- Modify `crates/malachite-app/src/payload.rs` — generator trait + `generate_payload_with_retry` build the lean block first; binding check in `validate_consensus_block`.
- Modify `crates/types/src/block.rs` — `lean_binding_ok`, `header_lean_commitment`; remove `commit_lanes`, `value_id`, `lean_lane_commitment`, `DecidedBlock` lane variants.
- Revert `crates/consensus-db/` to upstream.
- Modify `crates/malachite-app/src/handlers/{get_value,decided,received_proposal_part,started_round,process_synced_value,get_decided_values}.rs`, `state.rs` — remove stash, anchor by commitment, serve by header commitment.
- Modify `docs/lean-lane-integration.md`, `scripts/lean-testnet.sh`; create `scripts/fleet-lean.sh`, `scripts/fleet.env.example`, `scripts/fleet-split-compose.py`, `crates/quake/scenarios/fleet4-lean.toml`.

---

### Task 1: Lean node — commitment index and `arc_getBlockBytes{commitment}`

**Files:**
- Modify: `crates/lean-lane-node/src/node.rs` (struct `LaneNode` ~line 84; `open` ~153; the two `log.append` sites in `commit_block` and `promote_staged`; `block_wire_bytes` ~785)
- Modify: `crates/lean-lane-node/src/rpc.rs` (`GetBlockParams` ~line 106; `arc_getBlockBytes` ~140)
- Create: `crates/lean-lane-node/tests/by_commitment.rs`

**Interfaces:**
- Produces: `LaneNode::block_wire_bytes_by_commitment(&self, c: B256) -> Result<Option<Vec<u8>>, String>`; RPC `arc_getBlockBytes` accepting `{"commitment": "0x…"}` or `{"number": n}`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/lean-lane-node/tests/by_commitment.rs
use alloy_primitives::{Address, B256};
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn node(dir: &std::path::Path) -> LaneNode {
    LaneNode::open(Config { datadir: dir.to_path_buf(), ..Default::default() }).unwrap().0
}

async fn seed_and_submit(n: &LaneNode, key: u64, nonce: u32, outputs: usize) {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output { to: Address::with_last_byte((i % 200) as u8), amount: 3 })
        .collect();
    let tx = LeanTx::sign(nonce, outs, &sk(key), &domain);
    let signed = LeanSigned::new(tx);
    let sender = signed.tx().recover_sender(&domain).unwrap();
    if nonce == 0 {
        n.seed_account(
            sender,
            Acct { nonce: 0, balance: 10 * (lean_fee(outputs) + outputs as u128 * 3 * AMOUNT_UNIT) },
        )
        .await;
    }
    n.submit_raw(signed.raw()).await.unwrap();
}

/// Build+apply one block on `n`; returns (commitment, wire bytes).
async fn advance(n: &LaneNode, key: u64, nonce: u32) -> (B256, Vec<u8>) {
    seed_and_submit(n, key, nonce, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    (c, wire)
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_blocks_are_found_by_commitment() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c1, w1) = advance(&n, 1, 0).await;
    let (c2, w2) = advance(&n, 1, 1).await;
    assert_eq!(n.block_wire_bytes_by_commitment(c1).await.unwrap(), Some(w1));
    assert_eq!(n.block_wire_bytes_by_commitment(c2).await.unwrap(), Some(w2));
    assert_eq!(n.block_wire_bytes_by_commitment(B256::repeat_byte(9)).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_blocks_are_found_by_commitment() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 2, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(n.block_wire_bytes_by_commitment(c).await.unwrap(), Some(wire));
}

#[tokio::test(flavor = "multi_thread")]
async fn lookup_by_commitment_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (c1, w1) = {
        let n = node(dir.path());
        advance(&n, 3, 0).await
    };
    let n = node(dir.path());
    assert_eq!(n.block_wire_bytes_by_commitment(c1).await.unwrap(), Some(w1));
}
```

`shim_build`'s exact signature is at `node.rs:412`; if its parameters differ from `(parent, number, timestamp_ms, budget_gas)`, adapt the call, not the assertion.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/papaduck/lean-lane && cargo test -p lean-lane-node --test by_commitment 2>&1 | tail -5`
Expected: compile error `no method named block_wire_bytes_by_commitment`.

- [ ] **Step 3: Implement the index and lookup**

In `node.rs`, add to `struct LaneNode` (after `staged`):

```rust
    /// commitment -> block number for canonical blocks known to this
    /// process (replayed at open, appended since). Misses fall back to a
    /// bounded backward scan of the log (see `number_for_commitment`).
    pub by_commitment: Mutex<std::collections::HashMap<B256, u64>>,
```

In `open`, collect the index during replay and initialise the field:

```rust
        let mut by_commitment = std::collections::HashMap::new();
        log.replay(snap_number, |block| {
            let items = decode_block_txs(&block.txs);
            replayed_txs += items.len() as u64;
            apply_block(&mut state, &items, block.number, beneficiary);
            head = (block.commitment, block.number, block.timestamp_ms);
            by_commitment.insert(block.commitment, block.number);
            replayed_blocks += 1;
        })?;
```
and in the `Self { .. }` literal: `by_commitment: Mutex::new(by_commitment),`.

Find both append sites: `grep -n "\.append(&" crates/lean-lane-node/src/node.rs` (expect exactly 2, in `commit_block` and `promote_staged`). Immediately after each successful append, insert `self.by_commitment.lock().await.insert(block.commitment, block.number);` (use the local names in scope; in `promote_staged` the block is the destructured `block`).

Add the lookup next to `block_wire_bytes`:

```rust
    /// Resolve a commitment to a canonical block number: the in-process
    /// index first, then a bounded backward scan of the log from the oldest
    /// indexed number (older than that = not served; a peer with a fresher
    /// snapshot will serve it).
    async fn number_for_commitment(&self, c: B256) -> Result<Option<u64>, String> {
        if let Some(n) = self.by_commitment.lock().await.get(&c).copied() {
            return Ok(Some(n));
        }
        const SCAN_LIMIT: u64 = 8192;
        let oldest = self.by_commitment.lock().await.values().copied().min();
        let Some(mut n) = oldest else { return Ok(None) };
        let floor = n.saturating_sub(SCAN_LIMIT).max(1);
        while n > floor {
            n -= 1;
            let b = self.log.lock().await.read_block(n).map_err(|e| e.to_string())?;
            let Some(b) = b else { return Ok(None) };
            self.by_commitment.lock().await.insert(b.commitment, n);
            if b.commitment == c {
                return Ok(Some(n));
            }
        }
        Ok(None)
    }

    /// Shim: `arc_getBlockBytes{commitment}` — canonical, staged or queued
    /// block with that commitment.
    pub async fn block_wire_bytes_by_commitment(
        &self,
        c: B256,
    ) -> Result<Option<Vec<u8>>, String> {
        if let Some(n) = self.number_for_commitment(c).await? {
            return self.block_wire_bytes(n).await;
        }
        if let Some((_, e)) = self.staged.lock().await.iter().find(|(k, _)| *k == c) {
            return Ok(Some(e.wire.clone()));
        }
        if let Some(b) = self.sync_queue.lock().await.values().find(|b| b.commitment == c) {
            return Ok(Some(b.to_wire_bytes()));
        }
        Ok(None)
    }
```

In `rpc.rs`, change the params struct and handler:

```rust
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct GetBlockParams {
        number: Option<u64>,
        commitment: Option<B256>,
    }
```
```rust
    m.register_async_method("arc_getBlockBytes", |params, node, _| async move {
        let p: GetBlockParams = params.parse()?;
        let wire = match (p.commitment, p.number) {
            (Some(c), _) => node.block_wire_bytes_by_commitment(c).await.map_err(err)?,
            (None, Some(n)) => node.block_wire_bytes(n).await.map_err(err)?,
            (None, None) => return Err(err("arc_getBlockBytes: need number or commitment".into())),
        };
        Ok::<_, ErrorObjectOwned>(json!({
            "blockBytes": wire.map(|w| base64::engine::general_purpose::STANDARD.encode(w)),
        }))
    })
    .unwrap();
```
`B256` deserialises from a `0x…` hex string via alloy's serde (already used by `AnnounceParams`).

- [ ] **Step 4: Run tests**

Run: `cd /home/papaduck/lean-lane && cargo test -p lean-lane-node 2>&1 | grep -E "test result|FAILED|error\[" `
Expected: every suite `ok`, including `by_commitment` (3 passed).

- [ ] **Step 5: Commit**

```bash
cd /home/papaduck/lean-lane && git add -A crates/lean-lane-node && git commit -m "node: commitment index; arc_getBlockBytes accepts a commitment

Canonical blocks are indexed commitment->number during replay and on append,
with a bounded (8192) backward log scan on a miss; staged and queued blocks
are searched too. Lets a CL that only holds the EVM header (prev_randao =
lean commitment) fetch the matching lean bytes from any node."
```

---

### Task 2: Lean node — `arc_newBlock{commitment}` and staged retention

**Files:**
- Modify: `crates/lean-lane-node/src/node.rs` (staged clear on head advance ~line 376; cap `8` in `stage_block` ~line 501; new `shim_new_by_commitment` next to `shim_new` ~583)
- Modify: `crates/lean-lane-node/src/rpc.rs` (`NewBlockParams` ~86, `arc_newBlock` ~90)
- Modify: `crates/lean-lane-node/tests/by_commitment.rs`

**Interfaces:**
- Consumes: `block_wire_bytes_by_commitment` (Task 1).
- Produces: `LaneNode::shim_new_by_commitment(&self, c: B256) -> Result<NewBlockOutcome, String>`; RPC `arc_newBlock` accepting `{"commitment": "0x…"}` alone.

- [ ] **Step 1: Write the failing tests** (append to `tests/by_commitment.rs`)

```rust
#[tokio::test(flavor = "multi_thread")]
async fn new_block_by_commitment_promotes_a_staged_block() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 4, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(n.head.lock().await.0, c);
    // idempotent: already canonical
    assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_block_by_unknown_commitment_is_syncing() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let head_n = n.head.lock().await.1;
    assert_eq!(
        n.shim_new_by_commitment(B256::repeat_byte(7)).await.unwrap(),
        NewBlockOutcome::Syncing { head: head_n }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_entry_survives_an_unrelated_head_move() {
    // Stage block N+1 candidate A, then apply a different N+1 (B). A's
    // bytes must still be served by commitment (it is a losing candidate,
    // a peer may still ask for it), and promoting A must now answer
    // SYNCING, not panic.
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 5, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (ca, wa) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.stage_block(&wa).await.unwrap(), StageOutcome::Staged(ca));
    let (cb, wb) = n.shim_build(head_c, head_n + 1, ts + 2000, 1_000_000).await.unwrap();
    assert_ne!(ca, cb);
    assert_eq!(n.shim_new(&wb).await.unwrap(), NewBlockOutcome::Valid(cb));
    assert_eq!(n.block_wire_bytes_by_commitment(ca).await.unwrap(), Some(wa));
    assert!(matches!(n.shim_new_by_commitment(ca).await, Err(_) | Ok(NewBlockOutcome::Syncing { .. })));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p lean-lane-node --test by_commitment 2>&1 | tail -5`
Expected: compile error `no method named shim_new_by_commitment`.

- [ ] **Step 3: Implement**

Staged retention: at the head-advance site (`node.rs` ~376, currently `self.staged.lock().await.clear();`) replace with:

```rust
        // head advanced: keep entries that are still AHEAD of the new head
        // (losing candidates for this height are dropped when the height is
        // passed; bytes for the current height stay servable by commitment)
        let new_head_n = self.head.lock().await.1;
        self.staged.lock().await.retain(|(_, e)| e.block.number > new_head_n);
```
Read the surrounding code first: if the head is updated after this line, use the number of the block being applied instead of re-reading the head. Raise the cap in `stage_block` from `8` to `32`.

Add next to `shim_new`:

```rust
    /// Shim: `arc_newBlock{commitment}` — the CL only knows the commitment
    /// (it is in the EVM header). Idempotent for canonical blocks; otherwise
    /// apply from our staged/queued copy, else fetch the bytes from a peer by
    /// commitment, else SYNCING.
    pub async fn shim_new_by_commitment(&self, c: B256) -> Result<NewBlockOutcome, String> {
        if self.number_for_commitment(c).await?.is_some() {
            return Ok(NewBlockOutcome::Valid(c));
        }
        if let Some(bytes) = self.block_wire_bytes_by_commitment(c).await? {
            return self.shim_new(&bytes).await;
        }
        for cl in &self.peer_clients {
            use jsonrpsee::core::client::ClientT;
            let mut params = jsonrpsee::core::params::ObjectParams::new();
            let _ = params.insert("commitment", format!("{c}"));
            let r: Result<serde_json::Value, _> = cl.request("arc_getBlockBytes", params).await;
            let Ok(v) = r else { continue };
            let Some(b64) = v.get("blockBytes").and_then(|x| x.as_str()) else { continue };
            let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            else { continue };
            match LeanBlock::from_wire_bytes(&bytes) {
                Ok(b) if b.commitment == c => {
                    self.stats.block_pulls.fetch_add(1, Ordering::Relaxed);
                    return self.shim_new(&bytes).await;
                }
                _ => continue, // peer served the wrong block: ignore it
            }
        }
        let head_n = self.head.lock().await.1;
        Ok(NewBlockOutcome::Syncing { head: head_n })
    }
```

`rpc.rs`:

```rust
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct NewBlockParams {
        block_bytes: Option<String>,
        commitment: Option<B256>,
    }
    m.register_async_method("arc_newBlock", |params, node, _| async move {
        let p: NewBlockParams = params.parse()?;
        let outcome = match (p.block_bytes, p.commitment) {
            (Some(b64), _) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .map_err(|e| err(e.to_string()))?;
                node.shim_new(&bytes).await.map_err(err)?
            }
            (None, Some(c)) => node.shim_new_by_commitment(c).await.map_err(err)?,
            (None, None) => return Err(err("arc_newBlock: need blockBytes or commitment".into())),
        };
        match outcome {
            crate::node::NewBlockOutcome::Valid(commitment) => Ok::<_, ErrorObjectOwned>(
                json!({ "status": "VALID", "commitment": format!("{commitment}") }),
            ),
            crate::node::NewBlockOutcome::Syncing { head } => {
                Ok(json!({ "status": "SYNCING", "number": head }))
            }
        }
    })
    .unwrap();
```
`arc_stageBlock` also parses `NewBlockParams`; change it to require `block_bytes` (`p.block_bytes.ok_or_else(|| err("arc_stageBlock: need blockBytes".into()))?`).

- [ ] **Step 4: Run all node tests**

Run: `cargo test -p lean-lane-node 2>&1 | grep -E "test result|FAILED|error\["`
Expected: all `ok`; `by_commitment` 6 passed. If `staged.rs` asserts the old "cleared on head move" behaviour, update that assertion to the retention rule and say so in the commit message.

- [ ] **Step 5: Two-node peer fetch check** (integration, no new harness): run `./scripts/lean-smoke.sh` — it must still pass (it exercises `arc_newBlock{blockBytes}` and gossip on three nodes). Then a manual peer-fetch probe on two loopback nodes:

```bash
cd /home/papaduck/lean-lane && cargo build --release -p lean-lane-node
D=$(mktemp -d); F=$D/fund.txt; ./scripts/gen-lean-fund.sh 10 $F >/dev/null 2>&1
(setsid ./target/release/lean-lane-node run --datadir $D/a --port 8591 --shim --fund-file $F --fund-balance 10000000000000000000 >$D/a.log 2>&1 &)
(setsid ./target/release/lean-lane-node run --datadir $D/b --port 8592 --shim --peers http://127.0.0.1:8591 --fund-file $F --fund-balance 10000000000000000000 >$D/b.log 2>&1 &)
sleep 2
rpc(){ curl -s -X POST http://127.0.0.1:$1 -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}"; }
H=$(rpc 8591 arc_getHead '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
B=$(rpc 8591 arc_buildBlock "{\"parentCommitment\":\"$H\",\"number\":1,\"timestampMs\":$(($(date +%s)*1000)),\"budgetGas\":1000000}")
C=$(echo "$B" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
echo "$B" | python3 -c 'import sys,json;print(json.dumps({"blockBytes":json.load(sys.stdin)["result"]["blockBytes"]}))' > $D/blk
curl -s -X POST http://127.0.0.1:8591 -H 'content-type: application/json' --data-binary @<(printf '{"jsonrpc":"2.0","id":1,"method":"arc_newBlock","params":%s}' "$(cat $D/blk)") | head -c 120; echo
rpc 8592 arc_newBlock "{\"commitment\":\"$C\"}"; echo   # expect VALID: fetched from 8591 by commitment
rpc 8592 arc_getHead '{}'; echo
pkill -f "port 8591" ; pkill -f "port 8592"; rm -rf $D
```
Expected: node b answers `{"status":"VALID","commitment":"<C>"}` and its head is `<C>` at number 1.

- [ ] **Step 6: Commit**

```bash
git add -A crates/lean-lane-node scripts && git commit -m "node: arc_newBlock by commitment; staged entries survive unrelated head moves

The CL now anchors a decided height by the commitment carried in the EVM
header: promote our staged copy, else apply a queued copy, else fetch the
bytes from a peer by commitment (verified by recomputed commitment), else
SYNCING. Staged entries are retained while still ahead of the head (cap 32)
so losing candidates stay servable to peers."
```

---

### Task 3: Lean node — contract doc, smoke, tag

**Files:**
- Modify: `docs/integration.md` (§2 verb table)
- Modify: `README.md` (one line under "Get it": branch `v0.2`)

- [ ] **Step 1: Update the verb table** in `docs/integration.md` §2 by replacing the `arc_newBlock` and `arc_getBlockBytes` rows with:

```markdown
| `arc_newBlock` | `{blockBytes}` **or** `{commitment}` | `{"status":"VALID", commitment}` (appended, or already known — idempotent by commitment) or `{"status":"SYNCING", number:<local head>}`. By commitment: promotes a staged copy, else applies a queued copy, else fetches the bytes from `--peers` by commitment and verifies them, else SYNCING |
| `arc_getBlockBytes` | `{number}` **or** `{commitment}` | `{blockBytes}` or `{blockBytes: null}` — canonical, staged or queued block |
```
and add to §3 a row: `| decide (v0.2) | arc_newBlock{commitment = EVM header prev_randao} | the node holds the bytes (staged at validation); the CL carries none |`.

- [ ] **Step 2: Smoke** — `./scripts/lean-smoke.sh` must print `✅ PASS`.

- [ ] **Step 3: Commit and tag**

```bash
git add -A docs README.md && git commit -m "docs: v0.2 contract — newBlock/getBlockBytes by commitment" && git tag -a v0.2.0-rc1 -m "lean-lane v0.2 rc1: commitment-addressed newBlock/getBlockBytes"
```

---

### Task 4: CL shim client — commitment calls and `LeanBuilder`

**Files:**
- Modify: `crates/eth-engine/src/lean_shim.rs`
- Modify: `crates/eth-engine/Cargo.toml` (add `mockall` under `[dev-dependencies]` only if not already a workspace dev-dep of this crate — check `grep -n mockall crates/eth-engine/Cargo.toml`; `arc-eth-engine` already exposes `MockEngineAPI`, so it is present)

**Interfaces:**
- Produces:
  - `LeanShim::new_block_by_commitment(&self, commitment: BlockHash) -> eyre::Result<NewBlockStatus>`
  - `LeanShim::get_block_bytes_by_commitment(&self, commitment: BlockHash) -> eyre::Result<Option<Vec<u8>>>`
  - `pub trait LeanBuilder { async fn build_lean_block(&self, parent: LeanHead, timestamp_ms: u64, budget_gas: u64) -> eyre::Result<LeanBuilt>; }` with `pub struct LeanBuilt { pub commitment: BlockHash, pub bytes: Vec<u8> }`, `#[cfg_attr(test, mockall::automock)]`, implemented for `LeanShim` (calls `build_block`, recomputes nothing here — the CL recomputes via `LeanLanePayload::new`).
  - `LeanShim::head_for_build(&self) -> eyre::Result<LeanHead>` is just `get_head`; no new fn.

- [ ] **Step 1: Write the failing test** (in `lean_shim.rs`, new `#[cfg(test)] mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_params_are_by_name_hex() {
        let c = BlockHash::repeat_byte(0xab);
        let v = commitment_params(c);
        assert_eq!(v["commitment"].as_str().unwrap(), format!("{c}"));
        assert!(v.get("blockBytes").is_none());
    }
}
```

- [ ] **Step 2: Run** `cargo test -p arc-eth-engine commitment_params 2>&1 | tail -3` — expected: compile error, `commitment_params` not found.

- [ ] **Step 3: Implement**

```rust
/// By-name params for the commitment-addressed verbs (v0.2).
fn commitment_params(commitment: BlockHash) -> Value {
    json!({ "commitment": format!("{commitment}") })
}

impl LeanShim {
    /// v0.2: anchor by commitment. The node promotes its staged copy, applies a
    /// queued copy, or fetches the bytes from a peer; SYNCING means keep polling.
    pub async fn new_block_by_commitment(&self, commitment: BlockHash) -> eyre::Result<NewBlockStatus> {
        let r = self.call("arc_newBlock", commitment_params(commitment)).await?;
        if r.get("commitment").is_some() {
            return Ok(NewBlockStatus::Valid(Self::parse_commitment(&r)?));
        }
        match r.get("status").and_then(|s| s.as_str()) {
            Some("SYNCING") => Ok(NewBlockStatus::Syncing),
            other => Err(eyre!("lean shim: newBlock{{commitment}} response has neither commitment nor a known status (status={other:?})")),
        }
    }

    /// v0.2: canonical/staged/queued block bytes by commitment (sync serving).
    pub async fn get_block_bytes_by_commitment(&self, commitment: BlockHash) -> eyre::Result<Option<Vec<u8>>> {
        let r = self.call("arc_getBlockBytes", commitment_params(commitment)).await?;
        match r.get("blockBytes") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(
                base64::engine::general_purpose::STANDARD.decode(s).wrap_err("lean shim: getBlockBytes bad base64")?,
            )),
            Some(other) => Err(eyre!("lean shim: getBlockBytes unexpected type: {other}")),
        }
    }
}

/// A freshly built lean block as the node returned it. The CL recomputes the
/// commitment from `bytes` before using it (never trust `commitment` alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeanBuilt {
    pub commitment: BlockHash,
    pub bytes: Vec<u8>,
}

/// The proposer's view of the lane: build the next block on `parent`.
/// Mockable so the payload pipeline can be unit-tested without a node.
#[cfg_attr(test, mockall::automock)]
pub trait LeanBuilder: Send + Sync {
    async fn build_lean_block(&self, parent: LeanHead, timestamp_ms: u64, budget_gas: u64) -> eyre::Result<LeanBuilt>;
}

impl LeanBuilder for LeanShim {
    async fn build_lean_block(&self, parent: LeanHead, timestamp_ms: u64, budget_gas: u64) -> eyre::Result<LeanBuilt> {
        let (commitment, bytes) = self.build_block(parent.commitment, parent.number + 1, timestamp_ms, budget_gas).await?;
        Ok(LeanBuilt { commitment, bytes })
    }
}
```
`LeanHead` needs `Clone + Copy` (it already derives them). Because the mock must be usable from `arc-node-consensus` tests, gate it with a feature instead of `cfg(test)`: add `mocks = ["dep:mockall"]`-style gating only if the crate already does this for `MockEngineAPI` (`grep -n "automock" crates/eth-engine/src/engine.rs` shows how; copy that exact attribute form).

- [ ] **Step 4: Run** `cargo test -p arc-eth-engine 2>&1 | grep -E "test result|error"` — all ok.

- [ ] **Step 5: Commit**

```bash
git add crates/eth-engine && git commit -m "eth-engine: LeanShim v0.2 calls (newBlock/getBlockBytes by commitment) and a mockable LeanBuilder"
```

---

### Task 5: Payload generator carries `prev_randao`; lean block built before the EVM payload

**Files:**
- Modify: `crates/eth-engine/src/engine.rs` (`generate_block` ~line 380: add `prev_randao: B256` before `deadline`; `PayloadAttributes { prev_randao, .. }` at ~405)
- Modify: `crates/malachite-app/src/payload.rs` (`PayloadGenerator` trait ~105, `EnginePayloadGenerator` ~118, `generate_payload_with_retry` ~40, tests ~1300+)
- Modify: `crates/malachite-app/src/handlers/get_value.rs` (`build_block`)

**Interfaces:**
- Consumes: `LeanBuilder`, `LeanBuilt`, `LeanHead` (Task 4).
- Produces:
  - `PayloadGenerator::generate_block(&self, parent, timestamp, fee_recipient, prev_randao: B256)`.
  - `pub struct LeanBuild<'a> { pub builder: &'a dyn LeanBuilder, pub head: LeanHead, pub budget_gas: u64 }`.
  - `generate_payload_with_retry(previous_block, fee_recipient, generator, metrics, lean: Option<LeanBuild<'_>>) -> eyre::Result<(ExecutionPayloadV3, Option<LeanLanePayload>)>`.

- [ ] **Step 1: Write the failing test** (payload.rs tests; `MockPayloadGenerator` is already automocked there)

```rust
    #[tokio::test]
    async fn lean_block_is_built_first_and_its_commitment_becomes_prev_randao() {
        use arc_eth_engine::lean_shim::{LeanBuilt, LeanHead, MockLeanBuilder};
        // a real lean block so the CL's recompute agrees with the claim
        let parent = LeanHead { commitment: B256::repeat_byte(1), number: 4, timestamp_ms: 0 };
        let previous_block = test_execution_block(9, 1_000);
        let expect_ts_ms = std::cmp::max(previous_block.timestamp, Engine::timestamp_now()) * 1000;
        let bytes = lean_block_bytes(parent.commitment, 5, expect_ts_ms); // helper below
        let commitment = arc_consensus_types::block::decode_lean_block(&bytes).unwrap().commitment;

        let mut builder = MockLeanBuilder::new();
        let b2 = bytes.clone();
        builder.expect_build_lean_block()
            .withf(move |p, ts, budget| p.number == 4 && *ts == expect_ts_ms && *budget == 7)
            .times(1)
            .returning(move |_, _, _| Ok(LeanBuilt { commitment, bytes: b2.clone() }));

        let mut generator = MockPayloadGenerator::new();
        generator.expect_generate_block()
            .withf(move |_, _, _, prev_randao| *prev_randao == commitment)
            .times(1)
            .returning(|_, ts, _, _| Ok(test_payload(ts)));

        let metrics = AppMetrics::default();
        let (payload, lean) = generate_payload_with_retry(
            &previous_block, &Address::default(), &generator, &metrics,
            Some(LeanBuild { builder: &builder, head: parent, budget_gas: 7 }),
        ).await.unwrap();
        assert_eq!(lean.unwrap().commitment(), commitment);
        assert_eq!(payload.timestamp() * 1000, expect_ts_ms);
    }

    /// Canonical lean block bytes with no transactions (spec §3 layout).
    fn lean_block_bytes(parent: B256, number: u64, timestamp_ms: u64) -> Vec<u8> {
        let mut b = Vec::with_capacity(52);
        b.extend_from_slice(parent.as_slice());
        b.extend_from_slice(&number.to_le_bytes());
        b.extend_from_slice(&timestamp_ms.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }
```
`test_execution_block` / `test_payload` are the existing helpers in this test module; if `test_execution_block` does not exist, build an `ExecutionBlock` with `block_number: 9, timestamp: 1_000, block_hash: B256::repeat_byte(9)` and the other fields defaulted as the neighbouring tests do. The `Engine::timestamp_now()` race is benign: a second boundary changes both sides.

- [ ] **Step 2: Run** `cargo test -p arc-node-consensus lean_block_is_built_first 2>&1 | tail -5` — expected: compile errors (`LeanBuild` unknown, wrong arity).

- [ ] **Step 3: Implement**

`engine.rs`: add `prev_randao: B256` as the parameter before `deadline` in `generate_block` and use it in the attributes (`prev_randao,` replacing `prev_randao: B256::ZERO,` at the payload-attributes site inside `generate_block` only; the other `B256::ZERO` sites are forkchoice-only and stay). Update every caller (`grep -rn "generate_block(" crates --include='*.rs'`) to pass `B256::ZERO` except the one in `payload.rs` below. Keep the doc comment, amended: "Arc has no beacon chain; with the lean payment lane on, the CL places the lean block's commitment here so the EVM header binds it; otherwise zero."

`payload.rs`:

```rust
pub trait PayloadGenerator: Send + Sync {
    async fn generate_block(
        &self,
        parent: &ExecutionBlock,
        timestamp: u64,
        fee_recipient: &Address,
        prev_randao: B256,
    ) -> eyre::Result<ExecutionPayloadV3>;
}
// EnginePayloadGenerator passes prev_randao through to engine.generate_block(parent, timestamp, fee_recipient, prev_randao, self.deadline)

/// Everything the proposer needs to build the lean block that the EVM
/// header will commit to (spec §5.4).
pub struct LeanBuild<'a> {
    pub builder: &'a dyn arc_eth_engine::lean_shim::LeanBuilder,
    pub head: arc_eth_engine::lean_shim::LeanHead,
    pub budget_gas: u64,
}

pub async fn generate_payload_with_retry(
    previous_block: &ExecutionBlock,
    fee_recipient: &Address,
    generator: &impl PayloadGenerator,
    metrics: &AppMetrics,
    lean: Option<LeanBuild<'_>>,
) -> eyre::Result<(ExecutionPayloadV3, Option<LeanLanePayload>)> {
    // ... unchanged retry policy ...
    let call_once = || async {
        // ... unchanged timestamp logic ...
        // LEAN lane: build the lane block FIRST, timestamp-locked to the EVM
        // payload we are about to request, and bind it into the EVM header.
        let lean_payload = match &lean {
            Some(l) => {
                let built = l.builder.build_lean_block(l.head, timestamp * 1000, l.budget_gas).await
                    .wrap_err("lean lane: buildBlock failed")?;
                let lane = LeanLanePayload::new(built.bytes).wrap_err("lean lane: built block failed strict decode")?;
                if lane.commitment() != built.commitment {
                    return Err(eyre::eyre!("lean lane: recomputed commitment {} != shim's claimed {}", lane.commitment(), built.commitment));
                }
                Some(lane)
            }
            None => None,
        };
        let prev_randao = lean_payload.as_ref().map(|l| l.commitment()).unwrap_or(B256::ZERO);
        let _guard = metrics.start_engine_api_timer("generate_block");
        let payload = generator.generate_block(previous_block, timestamp, fee_recipient, prev_randao).await?;
        Ok((payload, lean_payload))
    };
    // ... unchanged retry wrapper ...
}
```
A retry rebuilds the lean block because `call_once` runs again (the closure borrows `lean`; use `let lean = lean.as_ref();` outside and `lean.map(...)` inside if the borrow checker objects). The `.when(...)` predicate stays: only unknown-payload-id errors retry; a lean build error does not.

`dyn LeanBuilder` requires the trait to be object-safe with `async fn`; if the compiler rejects `&dyn LeanBuilder`, make `LeanBuild` generic instead: `pub struct LeanBuild<'a, B: LeanBuilder> { pub builder: &'a B, .. }` and `lean: Option<LeanBuild<'_, impl LeanBuilder>>`.

`get_value.rs::build_block`: replace the current "generate payload, then build lean block" body with a single call:

```rust
    let lean = match lean_shim {
        Some(shim) => {
            let head = shim.get_head().await.wrap_err("lean lane: failed to fetch head for build")?;
            Some(LeanBuild { builder: shim, head, budget_gas: lean_budget_gas })
        }
        None => None,
    };
    let (execution_payload, lean_payload) =
        generate_payload_with_retry(previous_block, fee_recipient, &generator, metrics, lean).await?;
```
then the existing fire-and-forget `stage_block` on `lean_payload` bytes and the `ConsensusBlock { .., lean_payload }` literal. Delete the old post-payload lean build block.

Update every existing test that calls `generate_payload_with_retry` (pass `None`, destructure the tuple) and every `MockPayloadGenerator` / `TestPayloadGenerator` `generate_block` to the 4-argument form.

- [ ] **Step 4: Run** `cargo test -p arc-node-consensus -p arc-eth-engine 2>&1 | grep -E "test result|FAILED|error\["` — all ok (the migrate test excepted).

- [ ] **Step 5: Commit**

```bash
git add crates && git commit -m "consensus: build the lean block first and bind its commitment as prev_randao

generate_payload_with_retry builds the lane block (timestamp-locked), recomputes
its commitment, and passes it to reth as the payload's prev_randao, so the EVM
block hash now covers the lean block. Flag off passes zero as before."
```

---

### Task 6: Validation — header/lean binding check

**Files:**
- Modify: `crates/types/src/block.rs` (`impl ConsensusBlock`)
- Modify: `crates/malachite-app/src/payload.rs` (`validate_consensus_block`, lean arm)

**Interfaces:**
- Produces: `ConsensusBlock::header_lean_commitment(&self) -> Option<BlockHash>` (the payload's `prev_randao` when non-zero), `ConsensusBlock::lean_binding_ok(&self) -> bool`.

- [ ] **Step 1: Failing tests**

`types/src/block.rs` (`lane_tests` module):
```rust
    #[test]
    fn binding_ok_when_header_carries_the_lean_commitment() {
        let mut b = block_with_lean(0x11, 3); // existing helper building a ConsensusBlock with a lean payload
        let c = b.lean_payload.as_ref().unwrap().commitment();
        b.execution_payload.payload_inner.payload_inner.prev_randao = c;
        assert!(b.lean_binding_ok());
        assert_eq!(b.header_lean_commitment(), Some(c));
    }
    #[test]
    fn binding_fails_when_header_disagrees() {
        let mut b = block_with_lean(0x11, 3);
        b.execution_payload.payload_inner.payload_inner.prev_randao = B256::repeat_byte(0xee);
        assert!(!b.lean_binding_ok());
    }
    #[test]
    fn no_lean_payload_is_always_bound_and_zero_header_means_no_lane() {
        let b = block_without_lean(0x11);
        assert!(b.lean_binding_ok());
        assert_eq!(b.header_lean_commitment(), None);
    }
```
If `block_with_lean` / `block_without_lean` do not exist under those names, use the module's existing constructors (grep `fn .*lean` in the test module) and rename in the test.

`payload.rs` tests:
```rust
    #[tokio::test]
    async fn lean_header_mismatch_is_invalid_before_any_shim_call() {
        let mut validator = MockPayloadValidator::new();
        validator.expect_validate_payload().returning(|_| Ok(PayloadValidationResult::Valid));
        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(1).withf(|ip| ip.reason.contains("header/lean mismatch")).returning(|_| Ok(()));
        let metrics = AppMetrics::default();
        let mut block = test_block();
        let bytes = lean_block_bytes(B256::repeat_byte(1), 5, block.execution_payload.timestamp() * 1000);
        block.lean_payload = Some(LeanLanePayload::new(bytes).unwrap());
        block.execution_payload.payload_inner.payload_inner.prev_randao = B256::repeat_byte(0xee);
        let v = validate_consensus_block(&validator, None, &block, &store, &metrics).await.unwrap();
        assert_eq!(v, Validity::Invalid);
    }
```

- [ ] **Step 2: Run** `cargo test -p arc-consensus-types binding_ && cargo test -p arc-node-consensus lean_header_mismatch` — expected: compile errors.

- [ ] **Step 3: Implement**

`block.rs`:
```rust
    /// The lean commitment the EVM header commits to (`prev_randao`), if any.
    /// Zero means "no lane" — Arc sets zero when the lane is off.
    pub fn header_lean_commitment(&self) -> Option<BlockHash> {
        let r = self.execution_payload.payload_inner.payload_inner.prev_randao;
        (r != BlockHash::ZERO).then_some(r)
    }

    /// True iff the EVM header and the carried lean payload agree: no lean
    /// payload, or `prev_randao == recomputed lean commitment`.
    pub fn lean_binding_ok(&self) -> bool {
        match &self.lean_payload {
            None => true,
            Some(l) => self.header_lean_commitment() == Some(l.commitment()),
        }
    }
```
`payload.rs`, first thing inside `if let Some(lane) = block.lean_payload.as_ref() {` (before the timestamp check):
```rust
        if !block.lean_binding_ok() {
            record_invalid_payload(
                block,
                &format!(
                    "lean lane: header/lean mismatch (prev_randao {:?} vs recomputed {})",
                    block.header_lean_commitment(), lane.commitment()
                ),
                store, metrics,
            ).await;
            return Ok(Validity::Invalid);
        }
```

- [ ] **Step 4: Run** both crates' tests — all ok.

- [ ] **Step 5: Commit** `git add crates && git commit -m "consensus: reject a proposal whose EVM header does not commit to its lean payload"`

---

### Task 7: Decide by commitment; remove the in-memory stash

**Files:**
- Modify: `crates/malachite-app/src/handlers/decided.rs` (`handle`, `decide`, `anchor_lean_lane`)
- Modify: `crates/malachite-app/src/state.rs` (remove `lean_undecided`)
- Modify: `crates/malachite-app/src/handlers/get_value.rs`, `received_proposal_part.rs`, `started_round.rs`, `process_synced_value.rs` (remove every `lean_undecided` use and parameter; remove the `Some(block) if lean_shim.is_none()` / rebuild arm in `get_value.rs` so upstream's reuse path is used; in `started_round.rs` restore upstream's `process_pending_proposal_parts() -> Result<()>` and `validate_undecided_blocks` without the `assembled` parameter and without the "skip store-loaded in lean mode" branch)
- Modify: `crates/malachite-app/src/app.rs` only if a signature changes (it should not).

**Interfaces:**
- Consumes: `LeanShim::new_block_by_commitment` (Task 4), `ConsensusBlock::header_lean_commitment` (Task 6).
- Produces: `decide(.., lean_shim: Option<&LeanShim>)` (the `lean_lane` parameter is gone); `anchor_lean_lane(shim, commitment, height)`.

- [ ] **Step 1: Failing test** — `decided.rs` tests currently pass `None, None` to `decide`; change every call to a single trailing `None` and add one compile-level test that the stash no longer exists:

```rust
    #[test]
    fn state_has_no_lean_stash() {
        // Guard against re-introducing CL-side byte custody: the node holds
        // the bytes (staged at validation); decide anchors by commitment.
        let _ = std::mem::size_of::<crate::state::State>();
        // (compile-time: `State { lean_undecided, .. }` must not exist)
    }
```
The real behavioural coverage is the local testnet in Task 9 (mid-run CL restart must recover without a stash); say so in the commit message.

- [ ] **Step 2: Run** `cargo check -p arc-node-consensus --tests` — expected: arity errors at the `decide` call sites.

- [ ] **Step 3: Implement**

`decided.rs`: `handle` no longer reads the stash; `decide` loses the `lean_lane` parameter; the anchor call becomes:
```rust
    if let Some(shim) = lean_shim {
        let Some(commitment) = block.header_lean_commitment() else {
            return Err(eyre!("lean lane: decided EVM header carries no lean commitment at height={height} (lane on, prev_randao zero)"));
        };
        anchor_lean_lane(shim, commitment, height)
            .await
            .wrap_err_with(|| format!("lean lane: decide anchor failed at height={height}"))?;
    }
```
Replace `anchor_lean_lane` entirely:
```rust
/// Anchor the decided lean block by commitment. The node promotes its staged
/// copy (staged at validation), applies a queued one, or fetches the bytes from
/// a peer node — all node-side. The CL only waits, transient-tolerant, up to
/// the deadline. Historic no-op: an already-canonical commitment answers VALID.
async fn anchor_lean_lane(
    shim: &arc_eth_engine::lean_shim::LeanShim,
    commitment: arc_consensus_types::BlockHash,
    height: Height,
) -> eyre::Result<()> {
    use arc_eth_engine::lean_shim::NewBlockStatus;
    use arc_eth_engine::transient::is_transient;
    const TOTAL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
    const POLL: std::time::Duration = std::time::Duration::from_millis(150);
    let start = std::time::Instant::now();
    let mut polls = 0u64;
    loop {
        polls += 1;
        if start.elapsed() > TOTAL_DEADLINE {
            return Err(eyre!("lean lane: could not anchor {commitment} at height={height} within {TOTAL_DEADLINE:?} ({polls} polls) — halting"));
        }
        match shim.new_block_by_commitment(commitment).await {
            Ok(NewBlockStatus::Valid(c)) if c == commitment => {
                if polls > 2 {
                    info!("🪶 Lean lane anchored at height {height} after {polls} polls in {:?}", start.elapsed());
                } else {
                    debug!("🪶 Lean lane anchored at decide in {:?} (height {height})", start.elapsed());
                }
                return Ok(());
            }
            Ok(NewBlockStatus::Valid(c)) => {
                return Err(eyre!("lean lane: node answered commitment {c} for anchor {commitment} at height={height} — halting"));
            }
            Ok(NewBlockStatus::Syncing) => tokio::time::sleep(POLL).await,
            Err(e) if is_transient(&e) => {
                warn!("lean lane: node unreachable at anchor ({e:#}); waiting");
                tokio::time::sleep(POLL).await;
            }
            Err(e) => return Err(e.wrap_err("lean lane: newBlock{commitment} failed")),
        }
    }
}
```
Then remove: `State::lean_undecided` and its init; every `lean_undecided` parameter and insert/remove in the four handlers; the `stash.remove/clear` block in `decided::handle`; `get_value.rs`'s `Some(_) => rebuild` arm (restore upstream's `Some(block) => { check_reused_block_binding(..); block }` and `None => build`); `started_round.rs`'s `assembled` collection and the "skip store-loaded" branch (validation of store-loaded rows without lean bytes runs `validate_consensus_block` which, with `lean_payload == None`, does only the EVM lane — exactly spec §5.8). Keep `lean_shim` threaded everywhere it is used for validation.

- [ ] **Step 4: Run** `cargo test -p arc-node-consensus 2>&1 | grep -E "test result|FAILED|error\["` — all ok; `grep -rn lean_undecided crates` returns nothing.

- [ ] **Step 5: Commit** `git add crates && git commit -m "consensus: anchor the lean lane by the header commitment; drop the CL-side byte stash"`

---

### Task 8: Vote on the EVM block hash; drop the two-lane value id and the sync scan

**Files:**
- Modify: `crates/types/src/block.rs` (remove `commit_lanes`, `value_id`, `lean_lane_commitment`, `DecidedBlock::new_with_lane_commitment`, `DecidedBlock::from_stored_evm_only`, and their tests; restore upstream `DecidedBlock::new`; `to_proposed_value_with_validity` and `LocallyProposedValue` use `self_reported_block_hash()` as upstream)
- Revert: `crates/consensus-db/` → `git checkout origin/main -- crates/consensus-db` then re-add `lean_payload: None` in `services/pruning.rs`'s test literal
- Modify: `crates/malachite-app/src/handlers/get_decided_values.rs` (serve by header commitment)
- Modify: `crates/malachite-app/src/handlers/process_synced_value.rs` (dedup by block hash as upstream; keep decode/validate/stage)

**Interfaces:**
- Consumes: `LeanShim::get_block_bytes_by_commitment` (Task 4), `header_lean_commitment` (Task 6).

- [ ] **Step 1: Failing test** (`get_decided_values.rs` tests; the helper `get_raw_decided_value` is private, so test the pure mapping you extract):

```rust
    #[test]
    fn served_lean_bytes_must_match_the_header_commitment() {
        let evm = crate::block::tests_payload_helper(0x11, vec![]);
        let bytes = lean_block_bytes(B256::repeat_byte(1), 5, evm.timestamp() * 1000);
        let c = arc_consensus_types::block::decode_lean_block(&bytes).unwrap().commitment;
        let mut bound = evm.clone();
        bound.payload_inner.payload_inner.prev_randao = c;
        assert!(lean_bytes_match_header(&bound, &bytes).is_ok());
        assert!(lean_bytes_match_header(&evm, &bytes).is_err(), "zero header must not accept lean bytes");
        let wrong = lean_block_bytes(B256::repeat_byte(2), 5, evm.timestamp() * 1000);
        assert!(lean_bytes_match_header(&bound, &wrong).is_err());
    }
```
(reuse the `lean_block_bytes` helper from Task 5 by moving it to `crate::block` under `#[cfg(test)]`).

- [ ] **Step 2: Run** — compile error `lean_bytes_match_header` not found.

- [ ] **Step 3: Implement**

`get_decided_values.rs`: delete the offset/scan block. New per-height logic:
```rust
/// The lean bytes a served height must carry: the block whose recomputed
/// commitment equals the EVM header's `prev_randao`.
fn lean_bytes_match_header(evm: &ExecutionPayloadV3, bytes: &[u8]) -> eyre::Result<LeanLanePayload> {
    let header = evm.payload_inner.payload_inner.prev_randao;
    if header == B256::ZERO {
        return Err(eyre!("EVM header carries no lean commitment"));
    }
    let lane = LeanLanePayload::new(bytes.to_vec()).wrap_err("lean bytes failed strict decode")?;
    if lane.commitment() != header {
        return Err(eyre!("lean bytes commitment {} != header {header}", lane.commitment()));
    }
    Ok(lane)
}
```
In `get_decided_values`, for each `(height, execution_payload)`: if `lean_shim` is `Some` and `prev_randao != ZERO`, call `shim.get_block_bytes_by_commitment(prev_randao)`; `None` → `warn!` and `continue` (skip the height; a peer serves it); `Some(bytes)` → `lean_bytes_match_header(&payload, &bytes)?` (on Err, warn and skip). Then `encode_value(&payload, lane.as_ref().map(|l| l.bytes.as_slice()), lean_shim.is_some())` and upstream's `DecidedBlock::new(payload, certificate)` + `ExtendedCommitCertificate` as before. `get_raw_decided_value(store, execution_payload, lean: Option<LeanLanePayload>, height, lean_lane_enabled)`.

`process_synced_value.rs`: dedup back to `get_by_round_and_hash(height, round, block_hash)` and upstream's `debug_assert_eq!` + `ProposedValue::from(&existing)`; keep `decode_value`, the `lean_payload` field, `establish_block_validity(.., lean_shim, ..)`; remove the value-id comments.

`block.rs`: remove the listed items; `to_proposed_value_with_validity` → `Value::new(self.self_reported_block_hash())`; `From<&ConsensusBlock> for LocallyProposedValue` likewise; delete `lane_tests` cases that assert `value_id` differences and the cross-lane commit tests; keep framing/decoder/binding tests. Revert consensus-db as stated.

- [ ] **Step 4: Run** `cargo test -p arc-node-consensus -p arc-consensus-types -p arc-consensus-db -p arc-eth-engine 2>&1 | grep -E "test result|FAILED|error\["` — all ok. `git diff --stat origin/main -- crates/consensus-db` must show only the one test-literal line.

- [ ] **Step 5: Commit** `git add -A crates && git commit -m "consensus: vote on the EVM block hash; serve sync lean bytes by the header commitment"`

---

### Task 9: Guide, local testnet, restart leg

**Files:**
- Modify: `docs/lean-lane-integration.md` (§1 series table, §3 per-phase table: propose = "lean first, commitment → prev_randao", decide = "arc_newBlock{commitment}", sync serve = "by header commitment"; §4 note that the header now binds the lane; §6 keep; add "prevrandao semantics" paragraph from spec §2)
- Modify: `scripts/lean-testnet.sh` — add a `restart <n>` subcommand: `docker restart validator<n>_cl`, then wait until that validator's lean head is within 3 of the max head.

- [ ] **Step 1: Docs** — edit as listed; `grep -n "value_id\|commit_lanes\|stash" docs/lean-lane-integration.md` must return nothing.

- [ ] **Step 2: Script** — append to `scripts/lean-testnet.sh`:
```bash
restart(){ # restart <validator-index>: CL container only; the lean node stays up
  local i=${2:?validator index}
  docker restart "validator${i}_cl" >/dev/null || die "no container validator${i}_cl"
  for _ in $(seq 1 60); do
    max=0; me=0
    for j in $(seq 1 "$N"); do h=$(rpc "$(lean_url "$j")" arc_getHead '{}' | jget result.number); [ "${h:-0}" -gt "$max" ] && max=$h; [ "$j" = "$i" ] && me=${h:-0}; done
    [ $((max - me)) -le 3 ] && { say "validator$i back within 3 heights of the tip ($me/$max)"; return 0; }
    sleep 2
  done
  die "validator$i did not catch up after restart"
}
```
and `restart) restart "$@" ;;` in the `case`.

- [ ] **Step 3: Rebuild and run** (from `/home/papaduck/arc-lean-v0.1`; lean node from `/home/papaduck/lean-lane` branch `v0.2`):
```bash
make build-docker && cargo build --release --bin quake
cargo build --release --manifest-path ../lean-lane/Cargo.toml -p lean-lane-node -p spammer
export PATH="$PATH:$HOME/.foundry/bin"
FUND_ACCOUNTS=800 QUAKE=target/release/quake ./scripts/lean-testnet.sh up
(FUND_ACCOUNTS=800 LOAD_SECS=240 LOAD_RATE=3000 FANOUT=50 POOL_TARGET=1500 ./scripts/lean-testnet.sh load > .quake/lean-nodes/load-v02.log 2>&1 &)
sleep 90; ./scripts/lean-testnet.sh restart 3
sleep 170; ./scripts/lean-testnet.sh status
```
Record: heights/min from two `status` calls 60 s apart, head-block txs (must be 369 at N=50/100 M), "agreement: all 5", restart count 0 for the four untouched CLs, and `docker logs validator3_cl | grep -c "Manual intervention"` = 0. Then `./scripts/lean-testnet.sh down`.

- [ ] **Step 4: Commit** (docs, script) and append a worklog entry in the campaign checkout (`/home/papaduck/arc-node-paymentlane/docs/worklog.md`, §8 format: Changed / Measured / Broke / Decided / Open) with the numbers.

---

### Task 10: Fleet runner under `~/arc-runs/<run-id>`

**Files:**
- Create: `scripts/fleet.env.example` (hosts, tailscale IPs, `REMOTE_USER=papaduck`, `FLEET_ROOT=/home/papaduck/arc-runs`)
- Create: `crates/quake/scenarios/fleet4-lean.toml` — copy of `localdev-lean.toml` with `validator1..4` only and per-node `ARC_PAYMENT_LEAN_RPC = "http://172.17.0.1:8560"` (the docker bridge gateway, as the campaign used), `ARC_PAYMENT_LEAN_PEER_RPCS` = the four tailscale IPs on 8560.
- Create: `scripts/fleet-split-compose.py` — port of `experiments/dual-el/fleet/gen-fleet.py` from the campaign checkout, parameterised: reads `.quake/fleet4-lean/compose.yaml`, rewrites CL persistent-peer multiaddrs `/ip4/172.21.1.{n-1}/tcp/27000` → `/ip4/<ip_n>/tcp/2700{n-1}`, EL enode `@172.21.2.{n-1}:30303` → `@<ip_n>:3030{n}`, publishes `3030{n}:30303`, replaces the local `.quake/fleet4-lean` path in volumes with `<FLEET_ROOT>/<run-id>/quake`, and writes `compose-val{n}.yaml` per host plus a local `compose.yaml` with only validator1.
- Create: `scripts/fleet-lean.sh up|load|status|down|restart-cl <n>|kill-lean <n> <secs>` — for `<run-id>` (default `v02-$(date +%m%d-%H%M)`):
  - `up`: build images + lean node; for each remote: `mkdir -p $FLEET_ROOT/<run-id>/{quake,lean,logs}`; ship `arc_consensus:latest`/`arc_execution:latest` with `docker save | gzip -1 | tailscale ssh .. 'gunzip | docker load'` and verify `docker images -q <image>` matches the local image id; ship the lean binary, spammer, fund file (sha-verified as `deploy-lean.sh` does: compare `sha256sum | cut -c1-16` before/after); run `quake -f crates/quake/scenarios/fleet4-lean.toml start --force` locally only to *generate* files, then `quake stop`; run `fleet-split-compose.py`; ship `compose-val{n}.yaml` + the `quake/assets` dir + `validator{n}/` config dirs; start lean nodes (host processes, `--datadir $FLEET_ROOT/<run-id>/lean`, `--bind 0.0.0.0`, peers = other IPs, logs in `$FLEET_ROOT/<run-id>/logs`); `docker compose -f $FLEET_ROOT/<run-id>/compose-val{n}.yaml up -d` on each remote, local `docker compose up -d` for validator1; census + health gate as in `lane-bench.sh` (4/4 containers up, 0 "Manual intervention", heads advancing).
  - `load`: local spammer at `LOAD_RATE`, `FANOUT`, `POOL_TARGET` against all four lean nodes over tailscale; sample every 60 s: heights/min, head txs, pool depth, restart counts; write `$FLEET_ROOT/<run-id>/logs/run.jsonl` locally under `.quake/fleet-runs/<run-id>/`.
  - `status`: EL/lean heights per host, head txs, byte-identical check at the min common height.
  - `restart-cl <n>` / `kill-lean <n> <secs>`: the Track A legs.
  - `down`: compose down on every host, kill lean nodes; data stays under `$FLEET_ROOT/<run-id>` (a run is never deleted by the script; `rm -rf` is the operator's).

- [ ] **Step 1**: write `fleet.env.example`, the scenario, and the splitter; `python3 scripts/fleet-split-compose.py --dry-run` prints the four multiaddr and enode substitutions it will make.
- [ ] **Step 2**: write `fleet-lean.sh`; `bash -n` clean; `./scripts/fleet-lean.sh up` on the four machines; verify with `status` that all four EL and lean heights advance and agree.
- [ ] **Step 3**: 10-minute run at `LOAD_RATE=3000 FANOUT=50 POOL_TARGET=1500` (150 M budget in the scenario); then `kill-lean 3 60` and `restart-cl 3` legs; `status` after each; `down`.
- [ ] **Step 4**: commit scripts + scenario; record the run in the guide §5 (fleet, v0.2 on main 97f8da0) and in the worklog with cadence, fullness, agreement, restarts, and what the legs did. Any number without fullness is not recorded.

---

## Self-review

- Spec coverage: §2 → Task 5 (prev_randao) + §9 note in Task 9; §4 node verbs → Tasks 1–3; §5.1 → Tasks 6, 8; §5.2 → Task 8; §5.3 → Tasks 4, 5; §5.4 → Task 5; §5.5 → Task 6; §5.6 → Task 7; §5.7 → Task 8; §5.8 → Task 7; §5.9 unchanged; §8.1–8.5 → Tasks 1–2 (node tests), 3 (smoke), 9 (local + restart leg), 10 (fleet + folder rule); §9 rollout → Task 9 docs.
- Placeholders: none; each code step has code. Two spots delegate to existing files by name (`tests/sync.rs` harness is not used; `gen-fleet.py` is ported with its substitutions spelled out).
- Type consistency: `LeanBuilt {commitment, bytes}`, `LeanBuild {builder, head, budget_gas}`, `generate_block(parent, timestamp, fee_recipient, prev_randao)`, `new_block_by_commitment`, `get_block_bytes_by_commitment`, `shim_new_by_commitment`, `block_wire_bytes_by_commitment`, `header_lean_commitment`, `lean_binding_ok`, `lean_bytes_match_header` are used with the same names and shapes throughout.
