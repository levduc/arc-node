# CLAUDE.md — arc-node working notes

Arc is an EVM-compatible L1: a **customized Reth** execution layer (built on the Reth SDK, git tag `v1.11.3`) driven by the **Malachite** BFT consensus layer over the standard Ethereum **Engine API** (IPC/RPC). See `README.md`, `crates/node/README.md`, `crates/malachite-app/README.md`.

## Toolchain (and gotchas that cost real time)

- **Node.js ≥ 20.19 / 22.x (even versions).** Node 18 breaks the Hardhat genesis step with a misleading `HH19` error — the real cause is `ERR_REQUIRE_ESM` on the ESM-only `@noble/ed25519`. CI uses Node 24. Install via nvm; do not use system Node 18.
- **Rust** per `rust-toolchain.toml` (currently `1.91.1`), via rustup.
- **clang + libclang-dev** are required on the host to build `quake`/the node — `reth-mdbx-sys`'s bindgen needs clang's builtin headers (`stdarg.h`). Missing them gives `failed to run custom build command for reth-mdbx-sys`.
- **Docker** for `make testnet` (builds CL/EL images, runs containers via `quake`).
- **Foundry** pinned in `.foundry-version`.

## Common commands

```bash
make build                 # cargo build the node
make genesis               # generate assets/localdev/genesis.json (idempotent)
make testnet               # genesis + build-docker + quake start (5 validators + 5 CL + monitoring)
make testnet-down          # stop testnet
make testnet-clean         # remove testnet artifacts
make testnet-load RATE=1000 TIME=60   # drive load (wraps quake load)
make test-unit             # rust unit tests + lint
```

`quake` is the in-repo testnet manager (`crates/quake`), run as `cargo run --bin quake -- <args>` (the Makefile wraps this). It is NOT a global binary. Node RPC endpoints for a running local testnet are in `.quake/localdev/nodes.json` (validator1 EL: http `8545`, ws `8546`).

## Execution-throughput benchmarking

Two tools measure execution-layer performance:

- **`arc-engine-bench`** (`crates/engine-bench`) — replays real Arc blocks into a fresh node via `engine_newPayloadV4` + `engine_forkchoiceUpdatedV3` and reports per-block + aggregate import throughput. This is the apples-to-apples way to compare execution clients (no consensus gating).
- **`spammer`** (`crates/spammer`) / `quake load` — generates transaction load. ERC20 transfer load: `spammer ws --targets ws://127.0.0.1:8546 -r 4000 -g 4 -t 60 --mix erc20=100`.

A end-to-end baseline harness lives at **`scripts/bench/engine-baseline.sh`**. Workflow: drive load into a running testnet → `prepare-payload --from 1 --to <head>` → replay into a fresh target node booted with the **testnet's own genesis** (`.quake/localdev/assets/genesis.json`) → read results.

Benchmarking gotchas:
- The replay source must be **real consensus blocks**, not a reth `--dev` node — engine-bench assumes Arc's convention `parent_beacon_block_root = parent execution block hash` (`crates/eth-engine/src/engine.rs`), which dev blocks violate (→ "block hash mismatch").
- The target node's genesis must exactly match the source testnet's, or you get an "expected parent block 0" mismatch.
- Run bench nodes on non-testnet ports (e.g. http `7545`, authrpc `7552`, p2p `30402`); never `pkill -f "arc-node-execution node"` (it matches the testnet containers).
- `summary.csv` averages are diluted by empty blocks — filter `combined_latency.csv` to `gas_used > 0` for the real number.

### Key finding (ERC20 workload)

For ERC20-transfer block import, the per-stage cost split (from Reth's `reth_tree_root_*` / `reth_sync_execution_*` / persistence metrics) is roughly: **EVM execution ~4%, trie/state-root ~58%, disk persistence ~38%.** EVM execution is not the bottleneck — state-root/merklization dominates. Implication for alternative execution clients (e.g. Galxe's gravity-reth): a faster *parallel EVM* (grevm) targets the ~4% slice; *parallel merklization* targets the ~58% slice.

Validated on **real testnet state** (169 GB snapshot, block 48.75M): a 2.7M-gas block imports in **27.5 ms (97 Mgas/s)** vs ~1,900 Mgas/s on shallow state — **~20× slower**, with merkle/state-root **27.8 ms** and persistence **24.5 ms** dominating, EVM execution only **4 ms**. A warm re-replay (trie pages cached) halves it to 11.3 ms (235 Mgas/s), proving ~60% of merkle is cold disk-I/O. This is exactly what **reth 2.0's Sparse Trie Cache + Partial Proofs** target (published ~2 ms state-root), making a reth-2.0 upgrade the highest-value execution lever for Arc.

## reth 2.0 upgrade — attempt log

Goal: port Arc's execution layer from reth 1.11.3 → reth 2.0 (the upstream, maintained fix for the state-root/I-O bottleneck above). This is a multi-week port, NOT a dependency bump. Status: **LAYER 1 COMPLETE — `cargo generate-lockfile` resolves clean (exit 0, 0 errors, 103 reth crates at v2.0.0). Compilation not yet attempted (blocked on layer 2).** Branch: `reth-2.0-layer1`.

How layer 1 was resolved: repointed 42 `reth-*` deps to `tag = "v2.0.0"`; aliased the removed crate via `reth-primitives-traits = { ..., package = "reth-ethereum-primitives" }` (placeholder so the lockfile resolves — the real per-type relocation is a layer-2 task); bumped `rust-toolchain.toml` 1.91.1 → 1.93.0. **The resolve immediately surfaced the layer-2 wall: the lockfile now contains BOTH revm 34 (Arc's own EVM crates) AND revm 36 (reth 2.0) — they don't unify.** Arc's revm sub-crate pins are revm-34-era (`revm-interpreter = 32`, `revm-primitives = 22`, `revm-context-interface = 14`); reth 2.0 needs the revm-36-era set. So step 1 of layer 2 is bumping ALL `revm-*` workspace pins to reth-2.0's exact versions, then porting Arc's EVM code to the new revm API.

The 5 layers of the port (in order):
1. **Crate-source reconciliation** — reth crates are git-only (NOT on crates.io; verified). Of Arc's 42 `reth-*` deps, **40/42 exist at git tag `v2.0.0`**; only **`reth-primitives-traits` is removed** (primitives reorganized under `crates/ethereum/primitives`, `crates/payload/primitives`, `crates/engine/primitives`; its traits likely moved to alloy / `reth-ethereum-primitives` — TODO: find new import paths). Earlier mechanical `sed v1.11.3→v2.0.0` failed *only* on that one dangling crate. Scope of the relocation is small: Arc imports just **~8 types** from `reth_primitives_traits` across **~16 sites** (`SignerRecoverable`, `RecoveredBlock`, `SealedHeader`, `NodePrimitives`, `SealedBlock`, `Recovered`, `Block`, `InvalidTransactionError`) — `grep -rn reth_primitives_traits:: crates/` to find them; repoint to their reth-2.0 home.
2. **revm 34 → 36** — Arc's `crates/evm` (handler.rs, evm.rs, opcode.rs, log.rs, precompiles) is written against revm 34's `Handler`/`Context`/`Evm` API; revm 36 has breaking changes. Bulk of the work #1.
3. **reth SDK 1.11 → 2.0** — `ConfigureEvm`, `BlockExecutorFactory` (alloy-evm), node builder, payload primitives, and **Storage V2** APIs changed; port `crates/node`, `crates/evm-node`, `crates/execution-config`. Bulk of the work #2.
4. **Toolchain** — rust 1.91.1 → 1.93 (reth 2.0 `rust-version`), bump alloy 1.6.3 → ~1.8.2 (resolves up automatically as same-major).
5. **Datadir migration** — existing nodes' MDBX data is Storage-V1; reth 2.0 defaults to Storage V2 (`storage_v2: true`), so a re-sync/migration from snapshot is required.

Reference clone at `/home/papaduck/reth-2.0-ref` (git tag v2.0.0). Crate name list: `/tmp/reth20_names.txt`.

### PROGRESS LOG (most recent first)
- **🎯 LIVE 4-NODE reth-2.3 + MALACHITE CONSENSUS TESTNET — WORKS END-TO-END (2026-06-26).** Both binaries compile on reth-2.3 (EL arc-node-execution + CL arc-node-consensus; CL needed only PayloadAttributes.slot_number since CL↔EL is just the Engine API). Built reth-2.3 Docker images (Dockerfile base rust 1.91→1.93). Also bumped workspace reqwest 0.12→0.13 (alloy-2.0 pulls 0.13) + renamed reqwest TLS features (rustls-tls→rustls, rustls-tls-native-roots→rustls, add json feature) so quake/arc-checks build. Ran `quake -f crates/quake/scenarios/localdev4.toml start` (new 4-validator manifest). RESULT: 4 validators (CL+EL) produce blocks via Malachite BFT (~2 blocks/s), reth-2.3 EL executes with storage_v2=true on FRESH chain (no migration). ERC20 spam (45s): 73,595 txs ALL landed in 88 blocks, many FULL at 30M gas / 868 ERC20 transfers (0xa9059cbb), state-root ~277µs shallow. → reth-2.3+Malachite operates as a normal consensus node. GOTCHA: `cast block` in a tight loop times out on a busy node — use direct eth_getBlockByNumber RPC. TODO: "join public Arc testnet" (needs public CL bootnodes/chainspec + V1→V2 migration or genesis sync + live P2P).
- **reth 2.3 PORT COMPLETE — full binary typechecks (2026-06-26), branch `reth-2.3-upgrade`.** `cargo check -p arc-node-execution` = 0 errors against reth 2.3 / revm 40 / alloy 2.0.5 / ssz 0.10. Deltas beyond reth-2.0: arc-precompiles (revm-40 redesign: PrecompileError=Fatal-only, failures→PrecompileOutput.status Revert/Halt, new(g,b,0)/revert()/halt(PrecompileHalt::OutOfGas)); arc-evm (BlockExecutor new Result/TxResult, commit_transaction→GasOutput, set_state_hook REMOVED→reth installs at DB layer via State::set_state_hook so Arc's on_state forwarders were redundant, SELFDESTRUCT opcode→revm-40 Err-return contract, known_bytecode now (B256,Bytecode) executed directly, Send bounds, create_executor sig); arc-consensus-types (ssz 0.9→0.10 to match alloy 2.0 ExecutionPayloadV3 Encode); arc-execution-validation (ConsensusError::Other→msg(), validate_block_post_execution 5-arg+whole-result); arc-execution-payload (parent_block_info, slot_number=None, mark_invalid by-value, gas_used.tx_gas_used(), EthBuiltPayload::new 4-arg=Arc::new(block)+None); arc-evm-node (RpcAddOns::new 6-arg, slot_number). Building /tmp/arc-reth23. NOT VALIDATED: consensus-correctness (esp. SELFDESTRUCT opcode + known_bytecode) needs differential replay. Ref: /home/papaduck/reth-2.3-ref, cargo checkout 9384bc5.
- **🎯 STORAGE-V2 WIN MEASURED ON REAL ARC STATE (2026-06-26).** Migrated the 169GB V1 snapshot → V2 via `arc-node-execution db migrate-v2` (44min: changesets→static files, history/tx-hash→RocksDB, MDBX 142→58GB, flip storage_v2=true), then merkle rebuild on boot (~12min, MerkleExecute re-merklizes from hashed state). GOTCHAS (pruned snapshot): migrate-v2 reset SenderRecovery to 0 but txs only exist from 48.5M (pruned) — set checkpoint via `db stage-checkpoints set --stage sender-recovery`; the TransactionSenders static file is empty/buggy (reth-2.3 migrate-v2 + pruned bug: "append as block #0 but expected 48500000") so LIVE persistence of new blocks crashes — WORKAROUND: SenderRecovery=tip + boot with `--engine.persistence-threshold 20000 --engine.memory-block-buffer-target 20000` so blocks import IN-MEMORY (state-root computed pre-persistence). RESULT replaying 300 real blocks (48835032+) into reth-2.3 V2: **state-root 2.44ms avg (vs V1 cold 27.8ms = 11.4x, vs V1 warm 11.3ms = 4.6x)**; block import newPayload 4.89ms (vs V1 cold 27.5ms = 5.6x, vs legacy-V1 warm 9.7ms = 2x); 373 Mgas/s. Matches reth's published ~2ms state-root target ON REAL ARC STATE. Replay imported all 300 with correct hashes → also consensus-validates the reth-2.3 port + the migration. CAVEAT: V2 ran in-memory (persistence buffered due to senders bug) — newPayload/state-root are the synchronous compute (persistence is async anyway) so comparison is sound for execution+state-root; full on-disk persistence in V2 blocked by the senders static-file bug on this pruned snapshot.
- **reth-2.0 port VALIDATED + head-to-head MEASURED (2026-06-26).** Release binaries: reth-1.11 `/tmp/arc-reth1`, reth-2.0 `/tmp/arc-reth2`, engine-bench `/tmp/bench-reth1`. 176-block loaded-ERC20 fixture `/tmp/exp-fixture` (genesis `/tmp/exp-genesis.json`, block0 0xc7e3c695...). Replayed SAME fixture into both (isolated). reth-2.0 accepted all 176 blocks with IDENTICAL hashes/state-roots → **differential replay validated the port is consensus-correct** (storage_v2=true). Throughput IDENTICAL on shallow state: reth-1.11 2154 Mgas/s, reth-2.0 2129 Mgas/s. EXPECTED — reth-2.x's Sparse-Trie-Cache win only appears when state EXCEEDS RAM (62GB); a fresh chain can't reach it. Win already measured via warm/cold on real 169GB: 27.8ms cold vs 11.3ms warm = 2.5x.
- **reth 2.3 bump (branch `reth-2.3-upgrade`): deps resolve, arc-evm has 27 revm-40 errors.** 2.3 = revm 36→40 + alloy 1.6→2.0.5 + reth-primitives-traits 0.4.1. GOTCHA: pin alloy `~2.0.5` (2.1.0 adds EthPayloadAttributes.target_gas_limit, breaks reth's reth-engine-local); revm-statetest-types=19.0.3. arc-evm errors are revm-40 precompile API: `PrecompileOutput::new_reverted`/`reverted` gone, `PrecompileError::{OutOfGas,Other}` renamed — find new names in revm-precompile 36.x. Grindable batch, plus alloy-2.0 changes still to surface. NOTE: 2.3 also won't change the shallow benchmark.
- **STRATEGIC: to see the win on Arc needs LARGE (RAM-exceeding >62GB) state.** Rebuild-from-genesis = TOO MUCH (~48M public-testnet blocks, days). BEST PATH (confirmed feasible-ish): reth 2.x AUTO-DETECTS storage version from existing datadir metadata (reth source `crates/storage/db-common/src/init.rs:170` reads stored settings, defaults v1; `EnvironmentArgs::storage_settings` honors `--storage.v2` for NEW dbs only) and supports `StorageSettings::v1()` legacy mode. So the reth-2.0 binary can likely OPEN the reth-1.11 V1 169GB snapshot in legacy mode — gets the in-memory Sparse-Trie-Cache win (~2.5x, the main one) but NOT the on-disk Storage-V2 win (needs migration). Unknown: reth-2.x legacy on-disk table compat with reth-1.11 exact format. TEST: `arc-snapshots download --chain arc-testnet` (~1-2h, EL 169GB), then `/tmp/arc-reth2 node --chain arc-testnet --datadir <snapshot> [--storage.v2 false]`, then engine-bench replay real blocks 48.75M+ → measure vs reth-1.11 baseline (27.8ms cold). The reth-2.0 binary is consensus-correct so it CAN execute Arc blocks.
- **🎯 FULL `arc-node-execution` BINARY BUILDS + RUNS ON reth 2.0** (`cargo build --bin arc-node-execution` → 2.4GB binary; `--version`/`node --help` work). All crates ported: arc-evm, arc-execution-payload, arc-evm-node, and the rest (execution-config/txpool/validation, types, node) compiled with the recurring fixes. Recurring patterns: (1) `EthPayloadBuilderAttributes`→`EthPayloadAttributes` + drop `PayloadTypes::PayloadBuilderAttributes` assoc type; payload_id moved attributes→PayloadConfig; BuildArguments gained execution_cache/trie_handle (sparse-trie cache, passed None); `BlockBuilder::finish(sp, None)`; `EthBuiltPayload::new` dropped id; attributes method→field access. (2) `TaskExecutor` is now a type alias for `reth_tasks::Runtime` (drop Box). (3) `builder.extra_data` is `Bytes` → use `default_extra_data_bytes()`.
  **NOT YET VALIDATED:** compiles + runs ≠ consensus-correct. Unverified: differential block/state-root output; the `set_state_clear_flag`-removal and sparse-trie-cache-as-None choices need Arc's differential/e2e suite. Also: reth 2.0 uses **Storage V2** so it CANNOT read the old reth-1.11 169GB Arc snapshot — running it needs a FRESH genesis/chain. So a real reth-2.0-on-Arc number is now *reachable* (fresh chain + load + engine-bench), but NOT on the existing snapshot, and only trustworthy after differential validation.
- **`arc-evm` COMPILES against reth 2.0 / revm 36 / alloy-evm 0.30 (10 errors → 0).** This is the core/hardest crate (all EVM customizations). Fixes: `TxResult::into_result` added; revm-36 `Bytecode` struct (`eip7702_address()`/`new_eip7702()`); state-clear now unconditional; and the big one — alloy-evm 0.30 **DB model**: executor DB is bounded by `StateDB` (Database+DatabaseCommit) directly, not `&mut State<DB>`. Updated `create_executor` (`Evm<DB,I>` / `DB: StateDB` / `Inspector<Context<DB>>`), both `ArcBlockExecutor` impl blocks (`E: Evm<DB: StateDB>`), and `builder_for_next_block`'s `BlockExecutorFor` DB arg → `&'a mut State<DB>`. Commits `87e643c` and earlier. (2 cosmetic warnings left: a test-only `State` import, deprecated `map_frame`→`map_item`.)
- **Next crate: `arc-execution-payload` (9 errors)** — reth 2.0 payload-builder API: `EthPayloadBuilderAttributes`/`PayloadBuilderAttributes` moved, `PayloadTypes::PayloadBuilderAttributes` assoc-type change, changed fn arities, and structs that gained fields (`execution_cache`, `trie_handle`, `payload_id`) needing pattern updates. After that: `execution-config/txpool/validation`, `node`, `malachite-app`. Same pattern, different API surfaces. The compile front is moving ~1 crate at a time; consensus-correctness validation (differential/e2e) is still the separate multi-week cost.
- **arc-evm: 10 → 8 errors.** Fixed 3 revm-36 API breaks (verified against revm source in `~/.cargo/registry/src/.../revm-*-{9,12,16,17}.0.0`): (a) `Bytecode` is now an opaque struct — use `eip7702_address()`/`new_eip7702()`; (b) `set_state_clear_flag` removed, EIP-161 clearing is unconditional in revm 36 (`revm-database` CacheState `// EIP-161 state clear`), so dropping the `(true)` call is behavior-preserving. Remaining 8 are STRUCTURAL reth-2.0-SDK (alloy-evm 0.30): `BlockExecutor::into_result` (must ADD per-tx `ResultAndState` accumulation to `ArcBlockExecutor` — it only tracks receipts/gas_used; ref impl `EthBlockExecutor::into_result` returns `self.result`), `create_executor` signature change, and `Evm`/`Inspector<Context<...>>`/`DatabaseCommit` generic re-wiring (evm.rs:1787/1827/367). These touch block-execution result/state → consensus-critical; need Arc's differential test suite to validate, NOT just compilation. Do not blind-edit.
- **Layer 1 + 2a DONE & COMMITTED** (`c80c70d`, `2799354`, plus reth-primitives-traits/DownloadDefaults fix): deps resolve, revm unified to single 36. Key fixes: `reth-primitives-traits` moved to crates.io (use `version = "0.1"` not git — independently versioned 0.1.x, NOT removed); reth 2.0's `DownloadDefaults` (in `reth-cli-commands`) gained required `snapshot_api_url: Cow<'static,str>` field.
- **`cargo check -p arc-evm` now compiles the entire reth-2.0 + revm-36 tree and gets down to 10 real API errors** (all in `crates/evm/src/evm.rs` + `executor.rs`) — the revm-34→36 EVM-code port. These are SECURITY-SENSITIVE (EIP-7702 delegation, state-clear, block execution) — port carefully with revm 36 docs, do NOT guess. The 10:
  - `executor.rs:360` `set_state_clear_flag` — removed/renamed on revm 36 `State` (find successor in revm-database 12).
  - `evm.rs:986,4047-4049` `Bytecode::Eip7702(..)` — revm 36 made `Bytecode` a struct (was enum); use the new accessor/constructor in revm-bytecode 9.
  - `executor.rs:80` `create_executor` incompatible trait signature (alloy-evm 0.30 `BlockExecutorFactory`).
  - `executor.rs:355` missing `into_result` — `BlockExecutor` trait gained a method to implement.
  - `evm.rs:1786,1788,1826,1827` `Inspector<revm::Context<...>>` / `DB: DatabaseCommit` bound failures + `BlockExecutor::Evm == ArcEvm` type mismatches — revm 36 Inspector/Evm generics changed.
- After arc-evm: expect similar in `crates/evm-node`, `crates/execution-*`, then layer 3 (reth SDK node-builder/payload/Storage-V2), then datadir migration.

HONEST STATUS: dependency + reconciliation layers are DONE (big milestone — from "won't resolve" to "10 specific core-EVM API errors"). Remaining = deep semantic port of Arc's EVM executor to revm 36 + reth-2.0 SDK; multi-day, human-reviewed (consensus-critical). No working build yet, so **no real "reth 2.0 on Arc delivery number" exists** — do not fabricate one. Closest real proxies: warm-cache 235 Mgas/s (Arc reth 1.11) and reth 2.0's published ~2 ms state-root / 1.7 Ggas/s on large blocks.

## Payment-lane / UTXO experiment (branch `utxo-state-experiment`)

Goal: test the paper's payment-sector thesis empirically — can a UTXO payment state (a) stay in RAM
as tx volume grows, (b) be committed cheaply, and (c) beat reth + simple native (account) transfers?
Standalone crate **`experiments/utxo-state/`** (own `[workspace]`, detached from the reth fork; deps
cached: alloy-primitives, alloy-trie, k256/ecdsa, curve25519-dalek-ng, rayon). Bins
(`cargo run --release --bin <name>`):

- `utxo-bench` — RAM UTXO set + 3 commitment schemes. **25M UTXOs = 3.4 GB RAM; ECMH commit flat
  ~259 ms/block across a 250× state increase; MPT/Merkle full roots are O(n).**
- `merkle_acc` — hash-only incremental Merkle accumulator (no DLog). **Flat ~325 ms/block to 25M,
  5.5 GB RAM, ~11 µs/update** (constant depth). Hash-only commitment is forced if you drop DLog (the
  additive multiset hash is broken by Wagner's k-sum; ECMH needs a group).
- `two_el` — two-EL/two-root prototype: a mock CL fans out to an MPT-rooted EVM-EL and an ECMH-rooted
  UTXO Payment-EL **in parallel** and merges both roots into one header. Shows **block time =
  max(T_evm, T_pay)**; lane ~free while T_pay < T_evm (crossover ~10k payments/block at T_evm≈140 ms).
- `utxo_tx` — real secp256k1-signed UTXO payments end-to-end. **~8.5k tx/s single-thread, verify =
  75% of cost (~82 µs k256 ECDSA); state+Merkle ~3× cheaper.**
- `compare` — UTXO vs account-native, single-thread, same secp256k1+Merkle. **~equal (account
  marginally faster: fewer state writes).**
- `parallel_cmp` — parallel throughput. ~104k tx/s both, ~9× scaling. **(Had a bug — see below.)**
- `contention` — the corrected contention test (real balance RMW).

### KEY FINDINGS (honest — all measured on i7-11700F 8C/16T, 62 GB, testnet stopped for parallel runs)
- **For SIMPLE payments, UTXO is NOT faster than reth-style native transfers.** Single-thread and
  parallel they are ~equal (~105k tx/s on 16 threads), both **signature-verification-bound** (~9 µs/tx
  parallel k256 ECDSA; libsecp256k1 ~2× faster). Verification is contention-free and parallelizes
  identically for both models.
- **Account contention is real but invisible for simple transfers.** `contention` models REAL balance
  read-modify-write: hot recipients give 97% conflict and a hot account touched 1309× (non-commutative).
  Throughput is unchanged because the contended work (a balance add) is ~0 µs vs 9 µs/tx verify.
  Contention only bites when the contended *work* is expensive (contracts).
- **BUG I made and fixed (do not repeat):** the first `parallel_cmp` "account" path wrote a CONSTANT
  leaf and batched changes like UTXO — it never modeled RMW or hot-account serialization, so its
  "account == UTXO" was an artifact. `contention` fixes it. Also fixed a parallel-Merkle early-`break`
  that skipped levels to the root when changes collapsed into one subtree.
- **Where UTXO actually wins (architecture, not per-tx speed):** (1) a small **RAM-resident** payment
  lane avoids reth's disk-bound 169 GB shared MPT (App C: 0.1–0.37 Ggas/s on real state vs ~105k tx/s
  in RAM); (2) it **avoids the Block-STM contention machinery** (deterministic parallel, zero aborts —
  robust under adversarial contention); (3) conflict-free parallelism for EXPENSIVE/contract execution.
  The win is **isolation + small RAM state**, not "UTXO > account".
- **Commitment choice:** Merkle is fine. ECMH (32 B, O(1)) needs DLog → rejected. Hash-only ⇒ incremental
  Merkle (additive multiset hash is k-sum-broken). Mature alternatives: Bitcoin Core **MuHash**
  (incremental multiset hash, group-based, what `coinstatsindex` ships) or **libbitcoinkernel** (C++/FFI,
  full Bitcoin tx/Script semantics; needs cmake — NOT installed here). No reth-style EL uses Utreexo;
  Utreexo lives in Bitcoin nodes (utreexod, Floresta/`rustreexo`).
- **How Bitcoin scales sig verify (no batching):** libsecp256k1 + parallel `CCheckQueue` + **signature
  cache** (verify-once at mempool, skip at block connect). reth caches recovered senders similarly.

- `locality` — locality-keyed vs hash-keyed state+commitment (same Merkle shape, only the node
  store differs: contiguous Vec vs HashMap by node index = MPT-style). **Dense beats sparse
  1.1×→1.4× and the gap WIDENS with size** (dense −12% vs sparse −30% as it spills cache, to 12M
  entries). Honest scope: 1.4× is RAM-only (cache effect) and understates it — a real MPT keys by
  `keccak(addr)` (fully random) and pays on DISK (the 27.8 ms cold). So the data-structure win is
  modest in RAM (~1.4×), large on disk.

### REFINED THESIS (after the account-vs-UTXO + locality work — supersedes "UTXO is better")
- The 169 GB / 27.8 ms problem is the **shared general-purpose hash-keyed MPT**, not payment data.
- **First-order fix = ISOLATION:** a dedicated payment lane whose state grows with USERS (not tx
  volume or contract activity) → small → fits in RAM → no disk-bound merklization. Works for an
  account lane too; it is NOT a UTXO-specific win.
- **Pragmatic implementation = reuse reth** (EVM account + Block-STM + mempool + sender cache):
  payment-only account state is compact (≤ UTXO, ~1 entry/user), throughput = UTXO (~105k tx/s,
  signature-bound), and reth's MPT on a SMALL RAM state costs only ~1.4× vs an ideal structure —
  acceptable. This is the low-risk, high-reuse design.
- **Secondary refinement = a locality-keyed, unified state+commitment structure** (dense Merkle /
  Jellyfish-MT / Verkle): ~1.4× in RAM, more on disk; worth it at scale, not required.
- **UTXO is an ALTERNATIVE lane**, justified only by deterministic conflict-free parallelism (no
  Block-STM machinery/aborts) + statelessness — NOT speed or state size (both ≈ or worse than a
  minimal account lane). Do not pitch UTXO as faster/smaller.
- Architecture unchanged: second minimal EL + one extra header root; CL fans out + combines;
  block time = max(T_evm, T_pay), payment lane ~free while T_pay < T_evm.

DO NOT fabricate numbers. k256 (pure Rust) is ~2× slower than libsecp256k1.

### PAPER WRITING STATE (paper repo: `2026.arc.payment.highway.claude/main.tex`, Overleaf-connected)
The paper is being rewritten to a HONEST, NON-UTXO-LEANING framing the user approved:
**(1) the bottleneck is the shared general-purpose disk-bound MPT; (2) first-order fix = ISOLATE
payments into a lean lane (grows with users, fits in RAM); (3) pragmatic implementation = REUSE
RETH (account + Block-STM, Approach 2); (4) UTXO is an HONEST ALTERNATIVE (deterministic
conflict-free parallelism + statelessness), NOT faster/smaller; (5) the RESEARCH FRONT = the
COMMITMENT for the lean EVM state (unify store+commitment, locality-key it, cheap as it scales
past RAM, cheap proofs).** Push to Overleaf after each compile-clean change.

Done so far (all pushed, compiles clean ~25 pp): App C (reth-2.3 measurements), App D (reality
check), **App E "A payment lane in RAM"** (the experiments/utxo-state results — measured, honest
ranges, variance noted, reproducible via the crate README); **§4 pivot** (recommend Approach 2 /
reuse-reth, UTXO as honest alternative, tied to App E); **Research Questions** now lead with
"the commitment for the lean EVM state" (\S sec:rq-commitment) as the central front.

VERIFICATION (user demanded no fake numbers / reproducible): re-ran all experiments; logical
results are deterministic and reproduce exactly (utxo-bench 25M = **3.42 GB**, merkle_acc 25M =
**5.52 GB** both reconfirmed to the decimal). Timings vary ±~15% run-to-run; parallel throughput
is load-sensitive (92k contended vs 105k idle) — paper now states ranges, not point values.

DONE (end-to-end consistency pass, all pushed, compiles clean 25 pp): read the whole paper;
removed every UTXO-lean spot (abstract, §2 RQ list, §3 forward-ref, §4.3 "State and commitment"
now account-first, §4 takeaway now RECOMMENDS Approach 2, open-questions list). Moved the
memory/storage **schematic into the body §3** (replaces the stage-bar as `fig:exec-anatomy`,
"why I/O is bad"); App C keeps the cold/warm/V2 **bar chart**. Consolidated **§4.4** into a
"three measurements, one conclusion" synthesis table (reth shallow/big + Block-STM + UTXO-vs-account
→ isolate + reuse-reth + commitment-is-the-lever); moved the detailed Block-STM per-thread table to
**App E item 7**. Verified numbers reproduce (3.42 / 5.52 GB exact; timings stated as ranges).
Paper now consistent end-to-end: isolation + reuse-reth thesis; UTXO an alternative; the commitment
for the lean EVM state is the central research front. Avoid re-introducing UTXO-leaning or unmeasured
numbers in future edits.

### Dual-EL payment-lane implementation (branch `dual-el-payment-lane`)
Goal: each node = 1 CL (Malachite) + 2 ELs (EVM-EL + lean payment-EL, both reth); every block carries
TWO roots (evmStateRoot + paymentRoot), both built by the proposer and re-executed by every validator;
a dual-lane spammer stress-tests both. This is the paper's full design — **multi-day, consensus-critical**
(NOT one-night). Full file-by-file plan in **`docs/dual-el-payment-lane.md`** (commit e32d77c). Status:
branch + blueprint done; **no consensus code written yet** (deliberately — the riskiest piece, step 3, is
an SSZ + proposal-streaming change to `ConsensusBlock` that breaks consensus if rushed). Step order
(low-risk first): (1) 2nd Engine additive in eth-engine/config; (2) CLI+quake 2nd EL endpoint + 2nd
genesis; (3) `ConsensusBlock.payment_payload` field [HIGH risk]; (4) proposer builds both; (5) validators
re-execute both; (6) dual-lane spammer. Each consensus change must pass differential replay (all
validators compute identical paymentRoot) before trusting it.

**STEP 2 DONE + VERIFIED (2026-06-28):** `experiments/dual-el/launch-payment-els.sh` launches a 2nd
reth EL ("payment lane") per validator on the running localdev4 testnet (real `arc_execution` image,
own datadir `reth-pay`, genesis, ports http 19545/19645/19745/19845, discovery off, on
`arc_testnet_host-access` — NOT the `internal:true` `arc_testnet_default`, or published ports don't
route). Verified: 4 payment ELs up, RPC chainId=1337 block=0 (idle), while EVM ELs advance (blk 213+).
Gotcha: localdev EL uses IPC auth (no `jwtsecret` file) → don't pass `--authrpc.jwtsecret`; let reth
auto-gen `<datadir>/jwt.hex`. **STEP 1 NOT done** (CL driving EL2): coupled to step 4 and blocked by
the consensus Docker image rebuild (Cargo.toml reth `file://` fork unreachable in Docker build) —
do steps 1+3+4 together next. The base testnet here was started with `-e 50000 --monitoring false`
(blockscout fails otherwise); validator1 sometimes doesn't boot (3/4 is enough for BFT).

**STEPS 3+4 DONE — consensus code for two-root blocks compiles + tested (2026-06-28, branch `dual-el-payment-lane`).**
- STEP 3 (commit ad95465): `ConsensusBlock.payment_payload: Option<ExecutionPayloadV3>`; `SszBlock`
  7→8 tuple (ethereum_ssz supports Tuple9, verified); proposal streaming length-frames the two lanes
  (`[u64 len(evm)][evm ssz][payment ssz?]`); store encode/decode carry it. Round-trip test +82 db tests pass.
- STEP 4 (commit 0ef9046): CL builds+validates+finalizes BOTH ELs. config/CLI gained `payment_*`
  endpoint flags + `payment_engine_config()`; node.rs connects an optional 2nd `Engine`; threaded
  `payment_engine: Option<&Engine>` through run→go→handlers (started_round/get_value/
  received_proposal_part/process_synced_value/decided). `build_block` builds the payment payload on
  EL2's own head (`get_block_by_number("latest")`) aligned to the EVM timestamp; `validate_consensus_block`
  re-executes it (newPayload) — block valid only if BOTH lanes validate; `decide` forkchoices EL2 to the
  decided payment block. Single-EL path unchanged (None default). lib+bin+tests compile; 261 lib tests pass.
- **v0 limit:** value-sync carries only the EVM payload (synced blocks have no payment lane); the live
  proposal-streaming path carries both. NOT yet differential-validated on a live testnet.
- **TO RUN (remaining):** (a) payment genesis with blockGasLimit=100M; (b) revert the 41 reth `file://`
  deps in Cargo.toml → upstream `tag=v2.3.0` so the CL Docker image rebuilds (backup
  /tmp/Cargo.toml.preforkpatch); rebuild CL+EL images; (c) quake passes `--payment-execution-endpoint`
  (+ws/jwt) to each CL pointing at its payment EL; (d) boot, drive both lanes (dual spammer), confirm
  all validators compute identical paymentRoot. The Docker rebuild is the long pole.

**🎯 value_id NOW COMMITS TO BOTH LANES + 24h LIVE SOAK PASS (2026-07-02, commit `4c86b4f`).**
Closes the equivocation hole where the certified consensus value bound only the EVM lane (a proposer
could stream two payment payloads under one EVM hash → fork the payment lane beneath a valid EVM
commit certificate). Now `value_id = keccak(evm_block_hash ‖ payment_block_hash)` when a payment lane
is present, else exactly the EVM hash (single-EL blocks byte-for-byte unchanged, no migration).
- **types/block.rs:** `commit_lanes(evm, Option<pay>)` = single source of truth; `ConsensusBlock::value_id()`
  / `payment_block_hash()` (block_hash() stays the real EVM hash for EL ops); the two `From` impls
  (`ProposedValue`/`LocallyProposedValue`) vote on `value_id()`; `DecidedBlock` carries `payment_payload`,
  `new()` re-checks the commitment vs the certificate, `from_stored_evm_only()` for DB reconstruction;
  extracted `frame_lanes`/`unframe_lanes` (shared by streaming + sync).
- **consensus-db/store.rs:** undecided blocks keyed by `value_id` (not EVM hash) so the decide-path
  lookup by `certificate.value_id` resolves; DB reconstruction uses `from_stored_evm_only`.
- **sync now carries BOTH lanes (closes the v0 gap above):** `process_synced_value` unframes both,
  re-validates the payment lane, dedups by value_id; `get_decided_values`+`app.rs` thread
  `payment_engine`, fetch EL2 payloads per height, verify the fetched lanes reproduce the certificate
  value_id before shipping, frame both lanes. Decided store still persists EVM-only (no schema
  migration); the authoritative both-lane commitment lives in `certificate.value_id`, payment block
  is canonical in EL2.
- **Tests:** 6 new `block::tests` incl. the equivocation guard (`value_id_changes_when_only_payment_lane_changes`),
  order-sensitivity, frame/unframe round-trips. types 167 + consensus-db 82 + consensus lib 261 pass.
- **24h SOAK PASS:** rebuilt the CL image from this commit, ran `soak4` (4× CL+EVM-EL+payment-EL) with
  continuous dual-lane spam (300 tx/s/lane). **1422/1422 60s-checks, 0 failures over 86,400s**; both
  lanes lockstep to ~329k blocks; per-lane block hash identical across all 4 validators at every settled
  height (→ all validators computed an identical value_id every height); 12/12 containers, disk fine.
  A nondeterministic/mismatched commitment would have halted or forked within minutes — it did not.
  Harness: `experiments/dual-el/soak.sh` (this run used a path-adapted copy for the `arc-node-paymentlane`
  clone). NOTE: two clones of this branch exist — `/home/papaduck/arc-node` (older, prior soak) and
  `/home/papaduck/arc-node-paymentlane` (this commit); they have diverged.

**DUAL-EL DEMO TOOLING + STATE-BLOAT/PRESEED RUNS (2026-07-10..13, commits e91ff14..8a36e23+).**
Tooling (experiments/dual-el/): `demo.sh` (start/stop/status: quake soak4 + payment ELs + dual load +
dashboard; stop DELETES datadirs), `demo-bloat.sh` (state-experiment variant: preseeds 10M payment
accounts in genesis at start — regenerated EVERY run — guzzler bloat on EVM lane, pool-update load on
payment lane, memory caps applied at start), `demo-empty.sh` (clean chain, no preseed/load),
`dashboard.py` (localhost:8080: per-lane block/root/tx cards, value_id callout via cast keccak,
state-vs-history split [state=db/ = what a pruned snapshot ships; history=static_files+rocksdb minus
RocksDB WAL churn], touched-accounts counter, side-by-side headers, per-lane exec_ms+root_ms from reth
prometheus + disk I/O + RAM from cgroups, block-production rate; settled height anchors on MAX head so
an offline validator can't pin the display; "agree"=no-fork at settled height, NOT all-in-sync).
Spammer additions: `--fresh-recipients` (every transfer to a new address = new account/tx; wall-time-
seeded counter), `--recipient-pool base:size` (seq walk w/ wrap: first pass creates pool, then pure
balance updates — separates onboarding from steady-state payments). Payment ELs gossip via static
admin_addPeer mesh (launch-payment-els.sh; un-fed validator otherwise proposes empty payment blocks on
its round-robin turn). PAYMENT_GENESIS env selects payment-lane genesis.

KEY FINDINGS (all measured; trend log ~/dualel-bloat-trend.log):
1. **Genesis-resident alloc RAM**: reth keeps the entire genesis alloc in process RSS forever
   (~250B/account): 10M preseed = ~2.7G/EL, 30M = ~8G/EL, on top of disk state (10M=0.66GB, 30M=2.1GB
   MDBX) and working caches. 100M preseed = 7.2GB genesis JSON, ~30-60min parse+root per EL BOOT (OOM
   cascade if boots overlap; must boot sequentially, uncapped, then clamp). Production: don't preseed
   huge allocs; organic/tx-created accounts live only in DB.
2. **Proposer build-failure crash-exit (FIXED 509f404)**: payment getPayload timeout during get_value
   crashed the CL → restart-loop on same height → quorum loss + stall. Fix: log + skip round (same as
   existing None path). Confined to our get_value.rs handler, no consensus semantics.
3. **Value-sync batch livelock (OPEN)**: both-lane sync batches (10 heights × ~500tx payloads ≈ 2.2MiB)
   exceed client request timeout under load → lagging validator loops request/timeout forever and its
   request storm taxes healthy peers. Fix pending: value_sync.batch_size 10→~3 in scenario config.
   (Sync itself validated live: val2 caught up 2000 heights with both lanes re-validated — 4c86b4f.)
4. **Shared-box ceiling + caps**: 12 nodes on 62G thrash-froze uncapped under heavy state loads (swap
   sawtooth; sheds every ~45min; one hard machine freeze, no oom-kill logged — hardware suspected).
   Per-container caps (docker update, live) fix isolation: one container OOMs alone (137, ~3min
   recovery) instead of box death. Budget caps for END-state (EVM ELs pinned at 97% of 2.5G once the
   guzzler trie grew → 0.16blk/s; 5G fixed it). Current: pay 10G / evm 5G (bump demo-bloat from 2.5G!)
   / cl 0.75G. After ANY EL pool wipe (restart) spammers MUST be bounced (-l re-syncs nonces) or all
   their txs queue forever nonce-gapped.
5. **Latency coupling measured**: one certificate/height couples cadence = max(T_evm,T_pay)+overhead.
   Guzzler-full EVM blocks (50-90M gas keccak+cold-SSTORE, all txs serial on ONE contract+counter — the
   hot-account worst case; executed TWICE: build+re-execute) drag the pair to 0.1-0.7 blk/s while
   payment exec stays ~5ms. Light lanes hit ~3.8-3.9 blk/s (24h soak) ≈ the 250ms target. Builder
   deadline bounds block FULLNESS, not cadence (2000→250ms changed nothing under saturated mempool).
6. **RUN7 3h averages (10M preseed, cumulative histograms)**: EVM(guzzler) root 10.79ms/exec 57.25ms
   per block (~18tx) vs PAY root 2.59ms/exec 5.20ms per block (273-4761tx) → per-tx gap 2-3 orders of
   magnitude; payment transfers ≈ 33µs/tx exec on 10M accounts; payment proposer builds full blocks
   ~40x faster (exec proxy). Zero forks/divergence across ALL runs incl. OOM kills, CL restarts,
   460-height sync catch-up. Payment state root flat 1-11ms on 10M throughout.
7. Ops runbook: CL "Manual intervention required" park after EL races → docker restart CL once ELs up;
   exit-137 pay EL → boot uncapped (60g) then clamp; /tmp wiped on reboot → trend log in home dir.
RECOMMENDED DEMO CONFIG: 10M preseed + fixed CL (509f404) + caps pay10/evm5/cl0.75 + batch_size fix
before 4-validator demos. demo-empty.sh for clean-slate demos. Exec benchmark (empty vs 10M preseed,
identical 1500tx/s transfers both lanes, 1h each): results land in /tmp/execcmp2-results.txt (2026-07-13).

**MULTI-MACHINE FLEET (2026-07-14..16, branch `fleet-multi-machine`).** 4 validators on 4 tailscale
machines, driven entirely from ginny-alienware: val1 here (62G, wifi) + val2 ginnythui
(100.85.150.119, 62G, wired) + val3 papaduck (100.70.62.92, 78G, wired) + val4 papaduck-alien2
(100.86.97.40, 15G, wifi). tailscale ssh enabled on remotes; arc images shipped via docker save|gzip|ssh
docker load. LAN 2-8ms.

Topology mechanics (gen-fleet.py rewrites quake's compose): CL P2P = persistent-peers multiaddrs
/ip4/<host-ts-ip>/tcp/2700(N-1) (host-published); EVM EL P2P = trusted-peers enodes @<ts-ip>:3030N +
published 3030N:30303; payment ELs publish 3041N:30303 + admin_addPeer ts-enodes. Remote composes:
per-host compose-valN.yaml, volumes under /home/papaduck/arc-fleet (val3 fast variants:
/mnt/blockchain.ssd/arc-fleet), networks recreated per host with DYNAMIC subnets and NO static
container IPs (remote docker pools collide, e.g. ginnythui wiki owns 172.21/16); pay ELs MUST run
host-access as PRIMARY network + connect internal after (internal-primary silently breaks port
publishing AND the CL never reaches them). CL<->EL stays machine-local (IPC).

Machine gotchas (all cost real time): papaduck docker couldn't publish ports (iptables DNAT missing
kernel module after kernel upgrade) -> REBOOT fixes; alien2 wifi truncates GB-scale ssh/tar pipes ->
chunked base64 (8MB pieces, per-chunk sha+retry) or taildrop (needs operator); docker auto-creates
root-owned mount dirs -> every later user-extract silently fails (chown -R in clean step now);
alien2 wifi power-save = 101ms idle RTT -> keepalive ping (nohup ping -i 0.3 gw) -> 7ms; remote
cleanup MUST use explicit container names (three different silent failures with $(docker ps) subshells
inside tailscale ssh strings).

CL changes: value-sync env overrides ARC_VALUE_SYNC_BATCH_SIZE / ARC_VALUE_SYNC_TIMEOUT_SECS
(hardcoded 10 blocks x 1s timeout livelocked lagging validators; fleet runs 3/15s via gen-fleet env
injection; validated: 1151-block wifi catch-up in 5min). ARC_PAYMENT_GENESIS_FILE_PATH (per-lane
V4/V5 fork detection) lives on the gravity branch build. FINDING #7 (OPEN): pay-EL OOM that loses
unpersisted blocks while CL tip is ahead -> value-sync can't backfill (<tip) and ProcessSyncedValue
treats newPayload=Syncing as FATAL -> CL crash-loop. OPS HEAL (scripted in revive-val.sh): manual
engine_forkchoiceUpdatedV3(canonical head) -> reth p2p backfills from gossip peers (2148 blks/2.5min)
-> restart CL. CODE FIX PENDING: treat Syncing as retryable in process_synced_value.

Builder deadline: quake scenario had 2000ms per lane; CL gives proposer 3s for BOTH lanes -> 2+2>3
guarantees timeout on a contended host (papaduck-as-desktop: every-4th-height 4s stall, measured
gaps [4,0,1,0...]; round-robin means a slow proposer taxes EVERYONE ~3.5x - tolerated != unaffected).
Now 500ms everywhere (soak4.toml el.config + all launchers); deadline bounds packing, NOT cadence.

DISK PERSISTENCE IS FIRST-CLASS: reth_consensus_engine_persistence_save_blocks_duration histogram;
persistence is async but caps SUSTAINABLE cadence <= 1/persist-per-block (buffer bounded, then
backpressure). fsync(4k dsync x100) per machine: alien2 0.67ms, ginny 1.38, ginnythui 1.44,
papaduck-home 30.9ms(!) -> papaduck /mnt/blockchain.ssd 1.5ms. v3 persist 3704ms/blk on home disk ->
67ms on NVMe (55x). Full 4761-tx pay blocks persist ~212ms/blk. Dashboard shows persist per validator.

Spam delivery: each ws send awaits ack -> remote/wifi targets gate the loop: 4-target 1.5k tx/s vs
LOCAL-only 5-8k+ (gossip fans out; spam-fleet.sh defaults local now; spam-fleet-fanout.sh keeps the
old behavior). Full blocks need delivered >= 4761 x cadence; a HEALTHY chain (3-4 blk/s) needs 15-19k
tx/s to fill — full blocks are a symptom of slowness (or the coupling demo via guzzler).

MEASURE-2H (2026-07-15 20:35-22:35, all-fixes fleet, 10M preseed, pay blocks FULL 4761tx the whole
window, 6009 blocks): per-blk PAY exec 64.4 / root 12.0 / persist 211.8 ms; EVM(~5tx guzzler) exec
16.0 / root 13.4 / persist 121 ms. Per-tx: PAY 13.5us exec / 2.5us root / 44us persist vs EVM ~3.2ms /
~2.7ms / ~24ms (240x / ~1000x / ~540x). val4 partial (mid-window OOM+revive). Results:
/tmp/fleet2h-results.txt; harness fleet/measure-2h.sh.

Fleet scripts (experiments/dual-el/fleet/): demo-fleet.sh (4-machine 10M ~40min), demo-fleet-fast.sh
(10M + val3 on /mnt/blockchain.ssd), demo-fleet-empty.sh (~5min clean), demo-fleet-empty-fastssd.sh,
demo-fleet2.sh (2-machine), spam-fleet.sh / spam-fleet-fanout.sh (start|stop|status; env rates),
revive-val.sh [N] (single-validator revival incl. finding-#7 heal), measure-2h.sh, gen-fleet.py /
gen-remote-val4.py (compose surgery), pilot.sh. All starts self-verify + print fallback; loads always
separate. **`clean-fleet.sh`** (also a `clean` subcommand on demo-fleet-metamask/-fast/-empty-fastssd)
= deep-wipe ALL machines: force-rm every `validator*` container + every datadir (local `.quake/*` except
monitoring, each remote's `arc-fleet` on home + NVMe) + buildx prune. Use it — start/stop only touch a
tracked scenario, so ORPHANED datadirs from past runs pile up (measured: ~615 GB across the 4 machines
in one session; the docker "build cache" number in `system df` is a PHANTOM over-count — the real hog is
always the `.quake`/`arc-fleet` chain datadirs, 50-200 GB/machine/run). `clean` is explicit-only (never on
start/stop); datadirs are root-owned so removal needs a root container (`docker run --user root alpine rm`). Dashboard fleet mode: DUALEL_FLEET=<json {valN: ts-ip}> (unset = single-machine); rows:
per-val root/exec/PERSIST now+avg-run, landed TPS, block rate; exec-now had been lifetime-avg since
inception (fixed), stale values expire after 45s idle.

**PRODUCT-LEGIBLE DEMO: "state grows with activity, not payments" + "EVM congests, payment lane doesn't"
(2026-07-18..20, branch `fleet-multi-machine`, commits `3390c40` + `525cff1`).** For product-audience
demos (MetaMask walkthrough on single-machine or fleet, EVM lane chainId 1337 / payment lane 1338).
Two theses, each measured live (honest, real EIP-1559 — not mocked):
- **State metric = `reth_db_table_size{table=...}` prometheus gauge** (live, no DB lock), summed over
  STATE_TABLES {HashedAccounts, HashedStorages, AccountsTrie, StoragesTrie, PlainAccountState,
  PlainStorageState, Bytecodes} = exactly what a pruned snapshot ships. `du` on the datadir is USELESS
  (MDBX pre-allocates a flat ~4GB file). Payment state grows in HashedAccounts (users); EVM state grows
  in HashedStorages (contract activity). Measured live: pay ~10MB (transfers between existing accts add
  ~0) vs EVM ~58MB and climbing (storage writes + fresh recipients).
- **Congestion**: oversubscribe the 30M EVM block (~200 tx/s of ~1.6M-gas guzzlers = ~320M gas/s demand
  vs ~120M/s capacity) → block pins ~95% full, EIP-1559 baseFee ratchets +12.5%/full block (compounds
  ~2× every ~6 blocks; measured 12k→29M wei in 2min, exponential, no plateau in that range; seen to reach
  ~1e12 wei / 1000 gwei at steady state), mempool holds ~740 stuck txs. Meanwhile the 200M payment lane
  at ~300 tx/s transfers (~6M gas/s) stays <1% full, baseFee flat at the **49-wei floor**, backlog drains
  each block. NOT a protocol-fixed fee — it's capacity≫demand keeping the fee at the floor (state honest).
  **SUPERSEDED (2026-08-04): the payment fee IS now protocol-fixed.** Branch `payment-lane-gas` (other
  session) built `fleet/set-lane-economics.sh`: `updateFeeParams` with **minBaseFee==maxBaseFee**
  (default 1 gwei) pins the payment lane's baseFee EXACTLY (verified: 1,000,000,000 wei constant), while
  the EVM lane stays dynamic. Ported to `fleet-multi-machine`; demo-metamask starts auto-apply it
  (steps 4c/8c, 5× retry — fresh-chain quorum races silently drop controller txs). Cost per transfer =
  21000×1e9/1e18 = **$0.000021 flat** (native gas token is USDC, 1e18 wei = $1); dashboard shows $ per
  transfer on both lanes. The EVM lane gets **Arc MAINNET's real fee band** (min 20 gwei / max 20,000
  gwei from assets/mainnet/genesis.json) — an idle devnet otherwise decays to the 1-wei dev floor and
  reads CHEAPER than the payment lane, inverting the story. Payment fixed fee = 20 gwei too
  (matches the EVM floor): BOTH lanes idle at the SAME $0.00042/transfer — apples-to-apples — and under
  congestion only the EVM lane climbs (→$0.42, 1,000×); spammer-safe (signs max_fee 40k gwei). NOTE: `payment-lane-gas` also reorganized the demo surface (testnet.sh/spam.sh,
  attic/) — the two branches have overlapping parallel work (EXTRA_ACCOUNTS, --account-offset); reconcile
  before merging either.
- **Tooling (experiments/dual-el/):** `congest-demo.sh` {start|watch|status|stop} (auto-detects each
  lane's chainId from RPC; env EVM_RATE/PAY_RATE/GUZZLER/EVM_WS/PAY_WS; `watch` = live fullness-bar/
  baseFee/backlog table). `congestion.py` + `state-growth.py` = reproducible generators → CSV + self-
  contained presentation SVGs. `dashboard.py` `/product` = 3 hero tiles + congestion strip (fullness bar,
  fee-vs-floor multiple, mempool backlog via txpool_status) + state-grows chart; engineering view stays
  at `/`. `demo-metamask.sh` / `fleet/demo-fleet-metamask.sh` = MetaMask-tuned starts.
- **BLOCK CADENCE = 2 blocks/s (2026-08-04, commit 05317bd, product feedback: match Arc's real block
  time).** The CL paces heights via malachite "stable block times": each height it eth_calls
  `consensusParams()` on ProtocolConfig (0x3600...0001, PRIMARY/EVM lane only) and waits
  `targetBlockTimeMs` (0=unpaced; storage slot 0x668f...38520**5**, bits 112-127 of the packed
  8×uint16 ConsensusParams; localdev genesis ships **250ms**). **`set-block-time.sh <ms>`** flips it at
  RUNTIME via `updateConsensusParams` — onlyController = **hardhat dev account #8**
  (0x23618e81..., key at m/44'/60'/0'/0/8 of the junk mnemonic); effect within ~2 heights, no
  restart. demo-metamask.sh + fleet/demo-fleet-metamask.sh apply `BLOCK_TIME_MS` (default **500**)
  post-boot. Validated: both lanes lock to exactly 2.00 blk/s; dashboard Settlement reads ~0.5s.
- **SINGLE-MACHINE PORTABILITY (2026-08-06, commits 3d2c9ae + 4e01573; USER-CONFIRMED WORKING
  2026-08-07):** the whole demo (4 validators = 12 containers + dashboard) runs on ONE machine via
  `demo-metamask.sh`; validated cold-start end-to-end. `check` treats ports held by our OWN running
  demo as info ("stop first"), only fails when something ELSE holds them.
  `./demo-metamask.sh check` = preflight for a NEW device (binaries, images, node>=20-even,
  node_modules, ports, RAM). To port: clone repo + `cargo build --release -p quake -p spammer` (or copy
  binaries) + ship images (`docker save arc_execution arc_consensus | gzip` → `docker load`) + foundry +
  nvm node 22 + `npm install`. Footprint measured: **1.7 GiB idle / 5.5 GiB under full congestion** —
  16 GB device comfortable (lower the caps in start() below that). Dashboard + RPCs bind 0.0.0.0, so
  other devices reach them via the host's IP. Bench button caveat: run-bench.sh drives the FLEET
  distributed spam (tailscale) — on a lone device it degrades to val1-local spam only.
- **GOTCHAS:** (1) MetaMask reserves chainId 1337 for its built-in "Localhost 8545" — but the demo KEEPS
  1337(EVM)/1338(payment); do NOT change chainIds (breaks the working MetaMask demo — firm user
  constraint). The CL only validates the PRIMARY (EVM) engine's chainId against its whitelist {MAINNET
  5042, TESTNET 5042002, DEVNET 5042001, LOCALDEV 1337}; the payment EL's chainId is NOT validated, so
  1338 is fine there. (2) Guzzler intensity: `storage-write=100@8`≈182k gas/tx, `@80`≈1.6M gas/tx (use
  @80 — ~19 fill a 30M block AND land); `@600`/`@40` at high rate → per-tx gasLimit estimate exceeds the
  30M block → "-32003: gas limit too high", ALL accounts skipped, spammer EXITS. `-a N` must be divisible
  by `-g` (generators). (3) baseFee is PERSISTENT chain state — for the live fee-CLIMB visual, `stop` and
  let the EVM lane idle ~1-2min first (empty blocks decay it 12.5%/block back to the floor) THEN `start`;
  otherwise it opens already-maxed (still shows the divergence, just no ramp). (4) Payment-lane genesis:
  chainId + 200M gas set in BOTH the header gasLimit AND the ProtocolConfig slot (Arc reads gas limit
  from contract 0x3600...0001 slot 0x668f09ce... at runtime, not just the header).

**🎯 1 Ggas PAYMENT-LANE THROUGHPUT EXPERIMENT (2026-07-21, branch `fleet-multi-machine`).** Question:
how high can payment-lane tps go, and is "bigger blocks" the lever? Answer: **1 Ggas blocks WORK**
(chainId 1338 bounds are [1M,1B] in `crates/execution-config/src/chainspec.rs:293-297`, NO code change
— set via payment genesis header gasLimit + ProtocolConfig slot). **Block size is NOT the bottleneck.**
- **Tooling:** `PAY_GAS`/`EXTRA_ACCOUNTS` now env-overridable in `demo-metamask.sh` + `fleet/demo-fleet-metamask.sh`
  (defaults unchanged). Delivery: **`fleet/spam-fleet-distributed.sh {start|stop|status}`** — the TRUE
  distributed spammer (didn't exist before; fleet spam was always local-from-ginny + gossip): runs S
  spammers ON EACH machine against that machine's LOCAL payment EL (ws 19546/19646/19746/19846),
  globally-disjoint account ranges (`--account-offset`), `-l` nonce-resync for re-runs. `start` folds in
  an idempotent `ship` (sha-check, base64 over `tailscale ssh`, binary is only 5.1MB). Measure:
  **`pay-throughput-bench.sh {run|measure|stop}`** (`measure` = sample-only; reports avg + PEAK tx/s via
  best rolling ≥10s window, block rate, txs/block, fullness, exec/root/persist from prometheus).
- **Runbook:** `PAY_GAS=1000000000 EXTRA_ACCOUNTS=16000 fleet/demo-fleet-metamask.sh start` → `S=4 ACCTS=1000 fleet/spam-fleet-distributed.sh start` → `WINDOW=300 pay-throughput-bench.sh measure` → `spam-fleet-distributed.sh stop`. (S=4,ACCTS=1000 needs 16000 prefunded accounts = 4·S·ACCTS.)
- **RESULTS** (i7-11700F 16-thread local val1 + 3 tailscale remotes, **alien2 on WIFI**):
  - single-machine (all 12 containers + spam on ONE box): **7.4k tps, blocks 19% full** — CPU-contention +
    ack-gated delivery bound; MORE spammers made it WORSE (16 spammers = 6.5k). NOT block-size bound.
  - fleet + distributed spam: blocks **FILL to 100% = 47,618 txs** (1e9/21000). **5-min window: avg 9.5k /
    PEAK 15k tps** (snapshots to ~19k), block rate 0.28/s, 34,363 txs/blk (72% full), **exec 379ms,
    state-root 0.8ms, persist 169ms.**
- **KEY FINDINGS (this is the slide material):**
  1. **STATE-ROOT IS FREE ON A LEAN LANE: 0.8ms for a 34k-tx block.** Because 16k accounts send AMONG
     THEMSELVES → tiny ~16k-leaf RAM-resident trie, and NO new accounts → trie structure static, only
     values change. Contrast 27.8ms for a 2.7M-gas block on the 169GB disk-bound mainnet trie. The MPT
     "bottleneck" is about SHARED DISK-BOUND state, NOT a lean payment lane. **THESIS VALIDATED.**
  2. **EXECUTION IS THE SOLE BOTTLENECK:** exec 379ms/pass, **replayed ~5×/height** (proposer build + 4
     validators re-execute). Block time 3.6s (0.28/s) but measured work only ~0.55s → **~3s is CONSENSUS
     COORDINATION** (re-exec passes + voting + the **alien2 WIFI node** dragging rounds).
  3. **CONTENTION is the parallel-execution research problem:** transfers among a hot closed account set
     create read-write deps on balances → naive optimistic parallel (Block-STM) aborts. Deterministic/
     partitioned parallel (or UTXO conflict-free) is the angle.
  4. **STATE-ROOT GROWS WITH NEW ACCOUNTS:** cheap root holds ONLY for a closed set; fresh-recipients grow
     the trie → root climbs toward mainnet. "State grows with USERS, not payment volume."
- **PATH TO 20-50k:** parallel execution (cut the 379ms, replayed 5×) + drop the wifi node / reduce
  coordination. If cadence were exec-bound only (~0.93s/blk) the same full blocks = **~37k tps**. Block
  size and commitment are NOT the levers. TODO: isolate wifi-node drag (run 3 validators); try parallel exec.
- **GOTCHA:** after a spam run's timer expires, accounts are left nonce-gapped → mempool shows big
  `queued` (NOT `pending`) that never executes (waiting for nonces that won't come); drains/evicts on its
  own. Re-runs MUST use `-l` (in the script) or start at nonce 0 → "nonce too low". The product dashboard
  summed pending+queued, so it showed a phantom backlog — payment-lane pending tile removed for this reason.

**BRANCH `blockstm-native-transfers` (2026-08-07):** next experiment — Block-STM parallel execution
for native transfers on the payment lane (the "parallel execution" lever from the 1 Ggas finding:
exec 379ms/pass replayed ~5×/height is THE bottleneck; state-root 0.8ms is free). PRIOR WORK to build
on: `payment-lane-gas` commit `bd943f0` = standalone grevm-style Block-STM bench
(`experiments/utxo-state/src/bin/blockstm.rs`, 172 lines: hint DAG + level-parallel + deferred fee) +
`docs/block-stm-for-arc.md`. MEASURED there: **8.5× pooled transfers / 8.9× disjoint at realistic
per-tx cost, 1.0× hot-recipient (serial chain, expected), net-overhead when transfers are free**;
verdict "real execute-phase win but persistence>root>execute is the lane's priority order" — NOTE that
verdict predates the 1 Ggas fleet finding where exec DOMINATES (379ms vs persist 169ms vs root 0.8ms)
at full blocks, so the win case is stronger than the old verdict suggests. Cherry-picked onto this
branch as the baseline. NEXT: wire parallel execution into the real payment-EL path (grevm/reth or
Arc's executor) and re-measure fleet tps at 1 Ggas.
**MEASURED (2026-08-07, commit 55d3a60): the EVM/executor layer is NOT the bottleneck.**
`crates/evm/examples/transfer_bench.rs` runs the REAL ArcBlockExecutor on a full 1-Ggas block
(47,618×21k transfers, 16k closed accounts, bundle tracking on): **102.5ms = 2.15 µs/tx** (465k tx/s
serial) vs the live node's ~11 µs/tx (379ms/34k block) → **~80% of live exec cost is AROUND the
executor**: reth's engine-tree per-tx loop (payload_validator.rs:1298 drives per-tx, streams receipts
to the root task, coordinates prewarm) + state-provider/cache lookups. IMPLICATION: fast-pathing Arc's
executor crates can't fix it; the modified reth must attack the engine-tree loop/provider layer —
i.e. a vendored reth fork with a batched/parallel transfer path in the payload validator (Arc handler
detail: base fee NOT burned, beneficiary credited EVERY tx → parallel scheme must defer coinbase).
Arc's per-transfer customizations (for any fast/parallel path): blocklist SLOAD check on
sender+recipient (unmetered) + NATIVE_COIN_CONTROL load in pre_execution; full fee (base+tip) to
beneficiary in reward_beneficiary. Engine hot path: newPayload → payload_validator per-tx loop —
`execute_block` override would NOT be hit (only BasicBlockExecutor uses it). GOTCHA: arc-evm
cfg(test) has 91 pre-existing compile errors (revm-40 bump never fixed tests) — benches must be
example targets. perf is locked on this box (perf_event_paranoid=4, no sudo).
**RETH FORK STOOD UP (2026-08-07):** `~/reth-fork` (cp of `~/reth-2.3-ref`, v2.3.0). `experiments/reth-fork/apply-fork.sh`
appends a `[patch."https://github.com/paradigmxyz/reth"]` section (41 crates → absolute local paths) to
Cargo.toml — DEV-BOX ONLY, never commit (breaks Docker/CI); `apply-fork.sh revert` is a verified clean
round-trip (Cargo.toml==HEAD). Full `arc-node-execution` binary builds against the fork (10m30s).
TWO LEVERS, both located: (1) **state-root machinery = stock CLI flag `--engine.state-root-fallback`**
→ `StateRootStrategy::Synchronous` → `spawn_cache_exclusive` (NO multiproof task / prewarm / receipt-
stream; only `StateRootTask` strategy spawns those, gated in payload_validator.rs `spawn_payload_processor`).
NO FORK NEEDED for this half — test it on the payment EL first. (2) **parallel exec** = fork edit to
`reth-fork/.../engine/tree/src/tree/payload_validator.rs` `execute_transactions` (~L1265, the
`executor.execute_transaction(tx)` loop). Parallel scheme MUST: defer coinbase (Arc credits beneficiary
FULL fee every tx, base not burned — else all-tx conflict), cache blocklist bitmap (2 SLOADs/transfer on
NATIVE_COIN_CONTROL_ADDRESS), handle hot-recipient conflicts. Full plan + status: experiments/reth-fork/README.md.
NEXT (in order): measure state-root-fallback (no fork) → write parallel path → differential replay →
docker-from-fork (host-build+COPY; docker can't reach abs host paths) → fleet re-measure at 1 Ggas.
**FLEET A/B AT FULL 1-Ggas BLOCKS (2026-08-08):** val1 recreated live with the flag (node-local →
no chain restart; re-mesh val1 via admin_addPeer after any pay-EL recreate — runtime peer list is
lost). 47,618-tx blocks, 4 physical machines: val1 SYNC exec 304.7 + root 32.2 = 337ms vs val1's own
prior stock fleet baseline 379ms (~-20%, hw-consistent); cross-validator comparisons on the fleet are
CONFOUNDED BY HARDWARE (stock val4/alien2 280ms < stock val3/papaduck 623ms on identical blocks!) —
only same-machine A/Bs are valid. KEY HONEST POINT: fleet tps did NOT move (~0.35 blk/s, 2.9s/block vs
~0.5s of work) because cadence is CONSENSUS-COORDINATION-BOUND — root-machinery removal and even
future parallel exec show up in per-block exec_ms, not fleet tps, until the ~2.4s/height coordination
overhead is attacked. USER-RUN GOTCHAS (2026-08-08): plain `demo-fleet-metamask.sh start` = NO flag
(must pass PAY_EL_EXTRA_ARGS) and EXTRA_ACCOUNTS defaults to 1000 (S=1 ACCTS=250 for distributed spam);
"root field still changes" is EXPECTED — the flag changes HOW the root is computed, not WHETHER
(no-root = pending fork edit).
**LEVER 1 MEASURED LIVE (2026-08-07, commit 34d9fec):** single-machine 1 Ggas demo, val1 pay-EL with
`--engine.state-root-fallback` vs val2-4 stock, 333 IDENTICAL blocks (~3.4k tx each): val1 exec
**67.4ms/blk + root 12.3 = 79.7 total** vs stock **104.5 + 2.2 = 106.7** → async root machinery taxes
exec by **~35%**; net **-25% total work**, zero code changes, mixed config kept consensus (A/B via new
`PAY_EL<i>_EXTRA_ARGS` env in launch-payment-els.sh). Execution now dominates (67 vs 12) → parallel
exec in the fork is the remaining lever. Parallel execution NOT YET IMPLEMENTED (fork builds, hook
located, no parallel code written). Note: sync root 12.3ms > the 0.8ms fleet number because this box
ran 8 spammers + 12 containers concurrently (contention), and async "root 2.2ms" hides its real cost
inside the exec window (workers overlap) — the honest comparison is the TOTAL column.

Other branches: `gravity-payment-lane` PARKED (gravity-reth = O(total-state) per block on standard
Engine API paths, perf requires their consensus; FINDINGS.md there); erigon probe passed the engine
smoke (needs Prague system-contract predeploys in genesis; chainId 1337 collides with its named
bor-devnet chain — don't pass --networkid) but was abandoned after a box crash. TODO: cherry-pick
demo-bloat fail-loud preseed (09d02dd) to dual-el-payment-lane; presentation deck (paper repo
presentation/slides.tex) is demo-driven and carries all these numbers.
