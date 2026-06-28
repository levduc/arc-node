# Dual-EL payment lane — implementation plan (branch `dual-el-payment-lane`)

Goal: each node runs **1 CL (Malachite) + 2 ELs** (EVM-EL + a lean payment-EL, both reth on
payment-only vs general chains); every block carries **two roots** (evmStateRoot + paymentRoot),
both built by the proposer and re-executed by every validator before voting; a spammer drives
both lanes. **No simulation** — the payment lane really executes and commits.

This is the paper's full design (App.~E / §4). It is a **multi-day, consensus-critical** change.
Below is the concrete file-by-file plan and the order to do it in, lowest-risk first.

## Code map (verified)
- **EL client:** `crates/eth-engine/src/engine.rs` — trait `EngineAPI` (l.54), struct `Engine`
  (l.112) holding `api: Box<dyn EngineAPI>` (l.273); built by `Engine::new_ipc` (l.116) /
  `new_rpc` (l.147). Build call: `generate_block()` (l.360: forkchoice_updated l.409 + get_payload
  l.434). Validate call: `notify_new_block()` (l.457: new_payload l.474).
- **Config:** `crates/malachite-app/src/config.rs` — `EngineConfig` (l.37), `connect()` (l.43)
  makes ONE Engine. CLI: `crates/malachite-cli/src/cmd/start.rs` `StartCmd` (eth_socket/
  execution_socket for IPC; *_endpoint for RPC).
- **Block type:** `crates/types/src/block.rs` — `ConsensusBlock` (l.35) has ONE
  `execution_payload: ExecutionPayloadV3` (l.41); `block_hash()` = EVM payload block hash (l.47),
  which is the consensus **Value** (l.73). SSZ via `block_as_ssz_data()` (l.90). `DecidedBlock`
  (l.106). (SSZ tuple type in `crates/types/src/ssz.rs`.)
- **Proposer build:** `crates/malachite-app/src/handlers/get_value.rs` — `build_block()` (l.264,
  `EnginePayloadGenerator { engine }`), then `build_and_validate_block()` (l.214).
- **Validator re-execute:** `crates/malachite-app/src/payload.rs` — `validate_consensus_block()`
  (l.308) → `validate_payload()` (l.212) → `engine.notify_new_block()`. Entry points:
  received_proposal_part.rs:218, process_synced_value.rs:102, started_round.rs:232.
- **quake:** `crates/quake/src/node.rs` — `NodeMetadata` (l.398) has ONE
  `execution: ExecutionContainer` (l.407); ports in `new_local()` (l.425). Compose/container gen in
  setup.rs / manifest.rs.

## Plan (order = lowest risk first)
1. **Second Engine, additive (eth-engine + config).** Add a constructor + an `EngineConfig` variant
   that yields a *second* `Engine` from a second endpoint pair. No behavior change yet. Compiles in
   isolation. *(low risk)*
2. **CLI + quake plumb a second EL endpoint.** `StartCmd` gains `execution2_socket` /
   `execution2_endpoint`; `NodeMetadata` gains a second `ExecutionContainer` + ports; compose runs a
   2nd reth with its own datadir + a 2nd (payment-only) genesis. Boot a node with 2 ELs (EL2 idle
   until step 4). *(infra, medium risk)*
3. **`ConsensusBlock` carries the payment payload.** Add `payment_payload: ExecutionPayloadV3` to
   `ConsensusBlock` + `DecidedBlock`; extend `block_as_ssz_data` + the SSZ tuple + proposal-part
   stream/reassembly + all constructors + the store. **Riskiest** (SSZ + proposal streaming +
   wide ripple). Consensus Value stays the EVM block hash for v0 (payment payload is co-streamed +
   co-validated); folding `paymentRoot` into the Value is a v1 refinement. *(HIGH risk)*
4. **Proposer builds both.** In `build_block`, after the EVM payload, drive engine2
   (forkchoice_updated + get_payload) to build the payment payload from the payment mempool; attach
   to `ConsensusBlock`. *(medium)*
5. **Validators re-execute both.** In `validate_consensus_block`, also `engine2.notify_new_block`
   on `payment_payload`; block invalid if either lane fails. Persist/sync carry the payment payload
   automatically (it's a block field). *(medium)*
6. **Dual-lane spammer.** Drive EVM txs → EL1 RPC, payment txs → EL2 RPC; measure both roots
   advancing + per-lane tx/s. *(low)*

## Honest status
Branch created. Scoping done (this doc). Step 1 is the safe start. Steps 3–5 are consensus-critical
and must be validated by differential replay (all validators compute identical paymentRoot) before
this is trustworthy — that gate is the real cost, not the typing.
