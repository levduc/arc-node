# Dual-EL payment lane — design notes & learnings

A single place to understand **what was built, how it works in the code, how to run it, what was
validated, and the open questions** — so it can be read, replicated, and extended on another machine.

Companion docs:
- [`dual-el-runbook.md`](dual-el-runbook.md) — step-by-step to run the testnet.
- [`dual-el-payment-lane.md`](dual-el-payment-lane.md) — the file-by-file implementation plan / ripple.

---

## 1. Why this exists

Arc's execution bottleneck is the **shared, general-purpose, disk-bound Merkle-Patricia trie**: on a
real 169 GB testnet snapshot, a 2.7M-gas block imports in ~27.5 ms, of which **EVM execution is only
~4%** — the rest is **state-root/merklization (~58%) and disk persistence (~38%)**, dominated by cold
disk I/O on a trie that exceeds RAM.

The thesis: **isolate payments into a lean lane** whose state grows with *users* (not tx volume or
contract activity), so it stays small, fits in RAM, and escapes the disk-bound merklization. The
**dual-EL** prototype realizes that: every node runs a second execution layer dedicated to a payment
chain, and consensus commits to both lanes each block.

---

## 2. Architecture

Each node runs **1 consensus layer (Malachite BFT) + 2 execution layers (reth 2.3)**: a general
**EVM lane** and a lean **payment lane**. Every consensus block carries **two execution payloads →
two state roots**. The consensus layer builds, validates, and finalizes *both* ELs every block;
every validator re-executes both lanes and must agree on both roots.

```
   Malachite BFT (4 validators): ordered heights, one commit certificate (2/3+ votes) per height
            │                         │
   ConsensusBlock(N)          ConsensusBlock(N+1)
   ├ height / round / proposer / validity
   ├ execution_payload ─parentHash→ execution_payload      ← EVM lane     (its own hash chain)
   ├ payment_payload   ─parentHash→ payment_payload        ← payment lane (its own hash chain)
   └ proposer signature (over the whole streamed proposal)
```

- **Two independent hash chains.** Each lane is a normal blockchain — block `N+1.parentHash ==
  block N.hash` on each lane (verified live). Arc's convention `parentBeaconBlockRoot == parent hash`
  holds on both.
- **Consensus orders heights** via BFT commit certificates. The *certified value* each height is the
  **EVM block hash** (see §5 for the nuance).

---

## 3. How it works in the code

All changes are on top of the **reth-2.3** port (Malachite is unchanged — CL↔EL is just the Engine API).

### 3.1 The block carries a second payload (the data model)
- `crates/types/src/block.rs` — `ConsensusBlock` gains `payment_payload: Option<ExecutionPayloadV3>`.
  `block_hash()` still returns the **EVM** payload's hash (the consensus value).
- `crates/types/src/ssz/v1/block.rs` — `SszBlock` goes from a 7- to an **8-tuple** (adds the optional
  payment payload). `ethereum_ssz` supports tuples up to Tuple9, so no struct rewrite was needed.
- `crates/types/src/block.rs::block_as_ssz_data` encodes it; the store
  (`crates/consensus-db/src/{encoder,decoder}.rs`) carries it.
- **Proposal streaming** (`crates/malachite-app/src/proposal_parts.rs`): the proposal is split into
  parts (gossiped, hashed, signed). The payload bytes are now **length-framed**:
  `[u64 len(evm)][evm ssz][payment ssz?]` in `make_proposal_parts`, split back in
  `assemble_block_from_parts`. `payment_payload = None` keeps the single-EL path byte-identical.

### 3.2 The CL drives a second EL
- `crates/malachite-app/src/config.rs` — `StartConfig` gains `payment_*` endpoint fields +
  `payment_engine_config()`; `crates/malachite-cli/src/cmd/start.rs` gains the `--payment-*` flags.
- `crates/malachite-app/src/node.rs` — `connect_to_payment_engine()` builds an optional 2nd `Engine`;
  it gets the **same Osaka / Engine-API V4-vs-V5 config** as the main engine
  (`set_osaka_from_genesis_file`) — without this, `get_payload` on the 2nd EL fails with
  `-38005 Unsupported fork`. The optional engine is passed into `app::run`.
- `payment_engine: Option<&Engine>` is then threaded `run → go →` the handlers that build/validate/
  finalize: `started_round`, `get_value`, `received_proposal_part`, `process_synced_value`, `decided`.

### 3.3 Build both, validate both, finalize both
- **Build** (`handlers/get_value.rs::build_block`): after building the EVM payload, if a payment
  engine is present it builds the payment payload on **EL2's own head**
  (`engine.eth.get_block_by_number("latest")`) aligned to the EVM block's timestamp, and attaches it.
- **Validate** (`payload.rs::validate_consensus_block`): re-executes the payment payload on EL2
  (`newPayload`). A block is **valid only if BOTH lanes validate** → every validator computes
  identical roots per lane.
- **Finalize** (`handlers/decided.rs::decide`): after the EVM finalize, it forkchoices EL2 to the
  decided payment block (`set_latest_forkchoice_state`) so EL2's canonical head advances in lockstep.

### 3.4 Infra
- `crates/quake/templates/local/compose.yaml.hbs` — each CL's command gets the `--payment-*` flags
  pointing at `http://NODE_el_pay:8551`.
- `experiments/dual-el/launch-payment-els.sh` — launches a 2nd reth EL per validator (`NODE_el_pay`)
  on the testnet network, with builder flags (`--arc.builder.deadline=2000 --arc.builder.wait-for-payload`)
  + large txpool caps so it fills blocks under load. Genesis = 100M block-gas limit.

---

## 4. Is it really a blockchain? (yes)

Verified live: on **both** lanes, `block N+1.parentHash == block N.hash` for consecutive blocks. So
there are two genuine hash chains. The Malachite `ConsensusBlock` is the per-height envelope that
carries both lane-blocks and orders/finalizes them by BFT. The cryptographic linkage between heights
is the **EVM lane's parentHash chain** (which is the certified value); the payment lane is a parallel
hash chain stapled to each height and agreed via the proposer's signature.

---

## 5. Important nuances / honest limitations (v0)

- **"Two roots" = two payloads, not a new header field.** The EVM execution header is unchanged; the
  second root is the payment payload's own `stateRoot`. The paper's "header gains a `paymentRoot`
  commitment" form was **not** implemented.
- **The certified consensus value is the EVM block hash only.** The payment payload is co-streamed,
  co-validated, and co-stored, and all validators agree on it (via the signed proposal) — but it is
  **not folded into the certified value/identifier**. Hardening (v1): make
  `Value = hash(evmHash ‖ paymentRoot)` (or put `paymentRoot` in the EVM header) so one certified hash
  commits to both lanes.
- **Value-sync carries only the EVM payload.** A node that joins late / falls behind catches up the
  EVM lane via sync but **not** the payment lane (only the live proposal path carries both). Practical
  consequence: **start all 4 validators together from genesis**; a node that falls behind can't recover
  the payment lane. (Seen once when a payment EL was restarted mid-run — a fresh start fixed it.)
- **Receipts/tx roots are equal only for empty blocks** (`0x56e81f17…` = the empty-trie root); with
  txs they diverge — normal.

---

## 6. Build & run (and the gotchas that bite)

Full steps: [`dual-el-runbook.md`](dual-el-runbook.md). The traps actually hit:

- **`bindgen` can't find `stdarg.h`/`stdbool.h`** building `reth-mdbx-sys`/`librocksdb-sys`: clang's
  **builtin headers** are missing. Install the matching clang + `libclang-common-<N>-dev`
  (e.g. `sudo apt-get install -y clang libclang-dev libclang-common-18-dev`), then `cargo clean` and
  rebuild. Verify with `ls "$(clang -print-resource-dir)/include/stdarg.h"`.
- **OOM / machine freeze during the build.** The reth tree wants ~2 GB RAM per parallel job;
  `make build-docker` builds *both* images at once. Build images **separately**
  (`docker compose -f deployments/arc_execution.yaml build …` then the consensus one), use `-j 4` on
  host builds, and add swap.
- **A stray `anvil` on host `:8545`** shadows validator1's EVM host port (it looks "stuck"). The node
  is fine — query it over the docker network. On a clean box this doesn't happen.
- **Leftover blockscout containers** from a monitored run bind-mount `.quake/<name>/blockscout/*` and a
  crash-looping `db` recreates the testnet dir as **root**, blocking `quake start`. Use a fresh
  scenario name + `--monitoring false`; stop those containers if hit.
- **EL datadirs are root-owned** (container writes); clear via a throwaway container:
  `docker run --rm -v "$PWD/.quake":/q alpine rm -rf /q/<name>`.
- **Node 18 breaks genesis** (`HH19`/`ERR_REQUIRE_ESM`) — use Node 22.
- reth deps must use the **upstream `v2.3.0` tag** (not a local `file://` fork) so Docker/other machines
  can fetch them. (Already set on the branches here.)

---

## 7. What was validated

- **Differential agreement:** under load, the block hash at a settled height is **byte-identical across
  all 4 validators** on both lanes — every validator independently computes the same roots.
- **Two distinct roots under load:** spamming only the payment lane gives a block where
  `evmStateRoot ≠ paymentRoot` (the lanes are independent chains).
- **Fills the gas limit:** with high-gas txs the payment lane reaches **~100 M gas/block** (its 100M
  limit). With plain transfers it's build-time-bound (~600–1000 tx/block) since the proposer must
  finish within the consensus propose-timeout.
- **24-hour soak: PASS.** 4 nodes × (CL + 2 EL), continuous dual-lane spam, **1,425/1,425 checks clean,
  0 liveness/agreement/health/disk failures**, ~333k blocks/lane, all 4 in lockstep the entire time.
  (Harness: `experiments/dual-el/soak.sh`, `DUR=86400 RATE=250`.)

---

## 8. State growth & the "heat map" (the research direction)

Experiment: `experiments/utxo-state/src/bin/state_growth.rs` (`cargo run --release --bin state_growth`).

- **Live state grows with USERS, not transactions.** A transfer between *existing* accounts only mutates
  two balances — no new trie entries — so state size is **flat** as txs climb (measured: 1M accounts,
  5M transfers → 100 MB constant). It grows 1:1 only when **new** accounts appear. (On the live testnet,
  the 6.1 GB payment-lane datadir is mostly *prunable history*; the live account state is tiny.)
- **When users exceed RAM → a hot/cold "heat map".** Payment traffic is skewed (a small active set gets
  most txs). Simulated 1B users (~100 GB, won't fit RAM) with 90% of txs to a ~1M active set: an LRU
  cache of the **active working set (~200 MB)** captures ~88% of accesses; the ~10% cold tail is the
  irreducible disk hits. So **total users can exceed RAM as long as the working set fits** — the lever
  is keeping hot accounts resident, not all accounts.
- **The catch (the open problem):** the heat map solves *execution* (balance reads/writes become cache
  hits), but the **commitment** (state root) still needs the trie. The full answer pairs a hot/cold
  cache for execution with a **cache-friendly commitment** (sparse/cached trie à la reth 2.x, or a
  locality-keyed structure). That commitment-for-the-lean-state is the central research question.

---

## 9. Branches & repos

- **`reth-2.3-upgrade`** — the clean execution-layer port reth 1.11 → 2.3 (revm 40, alloy 2.0,
  Storage V2), Malachite unchanged. A **normal single-EL Arc node**. Run with `make testnet`.
- **`dual-el-payment-lane`** — the two-lane work, on top of reth 2.3. Run via the runbook.
- A clean copy of both branches (with run instructions at the top of each README, commits attributed to
  Duc Le) was prepared in `~/arc-node-paymentlane`, remote `levduc/arc-node`. To override an already-
  pushed branch after the history rewrite: `git push --force-with-lease origin <branch>`.

---

## 10. Suggested reading order to learn the code
1. `crates/types/src/block.rs` (the block + `payment_payload`) and `ssz/v1/block.rs`.
2. `crates/malachite-app/src/proposal_parts.rs` (how a block is streamed/reassembled).
3. `crates/malachite-app/src/node.rs` (engine construction + Osaka config) and `app.rs` (the `go` loop
   that threads `payment_engine`).
4. `handlers/get_value.rs::build_block`, `payload.rs::validate_consensus_block`,
   `handlers/decided.rs::decide` (build / validate / finalize both lanes).
5. `crates/eth-engine/src/engine.rs` (the Engine API wrapper: `generate_block`, `notify_new_block`,
   `set_latest_forkchoice_state`).
