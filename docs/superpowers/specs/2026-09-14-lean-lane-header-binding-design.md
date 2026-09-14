# Lean lane v0.2 — bind the lean block into the EVM header (prevRandao)

Date: 2026-09-14. Branches: `lean-lane-v0.2` (arc-node, off `lean-lane-v0.1`,
base `main@97f8da0`, reth v2.2.0) and `v0.2` (lean-lane, off `main` at
`3963a92`). Status: approved design, implementation not started.

## 1. Goal

Reduce the consensus-layer (CL) delta needed for the lean payment lane by
binding the lean block into the EVM block instead of beside it. Consensus votes
on the plain EVM block hash, exactly as upstream, and the EVM header itself
commits to the lean block through `prev_randao`.

Non-goals for this step: erasure-coded or compact proposal dissemination; a
lean state root in the certificate; header version, proposer or fee changes in
the lean block; transaction propagation between lean nodes; removing the
validation-time peer catch-up loop. Each is a separate decision.

## 2. Why prevRandao

Arc's CL sets `prev_randao` to `B256::ZERO` in every payload attribute and
documents that contracts must not rely on it (`crates/eth-engine/src/engine.rs`).
Reth accepts any 32-byte value there and includes it in the header hash. So the
field is a free 32-byte slot in the header that consensus already signs
indirectly. Placing the lean commitment there means:

- `value_id` is the EVM block hash again. The two-lane commitment
  (`commit_lanes`, `LeanLanePayload`-carrying `DecidedBlock`, store keyed by
  value id) is no longer needed.
- Any party holding the EVM header knows which lean block belongs to it, so a
  syncing node can look lean bytes up by commitment and verify them.
- The lean node can be the sole holder of lean bytes at decide time; the CL no
  longer has to carry them from validation to decide in memory.

Consequence to communicate: `block.prevrandao` observed by contracts changes
from a constant zero to a proposer-chosen keccak. It is not randomness (it was
not before), and anything asserting zero would break. Contracts that read it
as randomness were already told not to.

## 3. Lean block commitment (unchanged)

```
[parent 32B][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE] ( [len u32 LE][tx bytes] )*
commitment = keccak( parent ‖ number LE ‖ timestamp_ms LE ‖ keccak(bytes[48..]) )
```

The CL recomputes the commitment from bytes; it never trusts a claimed value.

## 4. Lean node contract v0.2 (lean-lane repo)

Backward compatible: every v0.1 form keeps working.

| verb | v0.1 | v0.2 addition |
|---|---|---|
| `arc_buildBlock{parentCommitment,number,timestampMs,budgetGas}` | `{commitment, blockBytes}` | unchanged |
| `arc_stageBlock{blockBytes}` | `STAGED` / `SYNCING` | staged entries are kept while their parent is the head **or an ancestor of the head within the last 64 blocks**; they are no longer cleared on every head move. Bounded by the existing queue cap |
| `arc_newBlock{blockBytes}` | `VALID` / `SYNCING` | unchanged |
| `arc_newBlock{commitment}` | — | **new**: promote the staged block with that commitment; if none, look in the sync queue; if none, fetch by commitment from `--peers` (`arc_getBlockBytes{commitment}`), stage and promote; if no peer has it, `{"status":"SYNCING", number}`. Idempotent: already-canonical commitment answers `VALID` |
| `arc_getBlockBytes{number}` | `{blockBytes|null}` | unchanged |
| `arc_getBlockBytes{commitment}` | — | **new**: canonical block with that commitment, or a staged/queued one, else `null` |
| `arc_getHead` | `{commitment, number, timestampMs}` | unchanged |

Node internals: a `commitment → number` index over the append-only log, built
during recovery (replay already decodes every block) and maintained on append;
staged and sync-queue maps already key by commitment.

## 5. CL changes (arc-node)

Every arm remains behind `ARC_PAYMENT_LEAN_LANE`; flag off is upstream
behaviour and upstream bytes on the wire.

### 5.1 Types (`crates/types/src/block.rs`)

Keep: `lean_payload: Option<LeanLanePayload>` on `ConsensusBlock`,
`LeanBlockRef`, `decode_lean_block`, `frame_lanes`/`unframe_lanes`,
`encode_value`/`decode_value` (wire mode by flag).
Remove: `commit_lanes`, `value_id()`, `lean_lane_commitment()`,
`DecidedBlock::new_with_lane_commitment` / `from_stored_evm_only` (upstream
`DecidedBlock::new` returns).
Add: `ConsensusBlock::lean_binding_ok() -> bool` = `lean_payload` is `None`, or
`execution_payload.prev_randao == lean_payload.commitment()`; and
`ConsensusBlock::header_lean_commitment() -> Option<B256>` = `prev_randao`
when non-zero.

Votes use `self_reported_block_hash()` as upstream.

### 5.2 consensus-db

Revert to upstream. No lane changes remain in this crate.

### 5.3 eth-engine

`lean_shim.rs`: add `new_block_by_commitment(commitment)` and
`get_block_bytes_by_commitment(commitment)`; keep everything else. `engine.rs`
/ `PayloadGenerator::generate_block` gain a `prev_randao: B256` argument;
upstream callers pass `B256::ZERO` (behaviour unchanged when the lane is off).
`transient.rs` unchanged.

### 5.4 Proposer (`handlers/get_value.rs`, `payload.rs::generate_payload_with_retry`)

Order per attempt: choose `timestamp` (upstream logic) → if lane on,
`arc_buildBlock(head.commitment, head.number+1, timestamp*1000, budget)`,
recompute commitment, cross-check the claimed one → `generate_block(parent,
timestamp, fee_recipient, prev_randao = commitment)` → attach the lean bytes
as `lean_payload`. A retry with a new timestamp rebuilds the lean block. The
proposer stages its own build (fire-and-forget) as today. No stash.

### 5.5 Validation (`payload.rs::validate_consensus_block`)

After the EVM engine verdict, when `lean_payload` is present:
1. `lean_binding_ok()` — else Invalid (reason: header/lean mismatch).
2. timestamp lockstep `lean.timestamp_ms == evm.timestamp * 1000` — else Invalid.
3. parent linkage against the local lean head, with the existing historic
   (`number <= head`, byte-equal) and behind (`number > head+1`, peer
   catch-up) branches — unchanged in this step.
4. `arc_stageBlock`, fire-and-forget.

A lean-mode block that arrives **without** lean bytes but whose header carries
a non-zero `prev_randao` (a store-loaded undecided row after restart) is
validated on the EVM lane only and left Valid if it was Valid; decide needs no
bytes from the CL (5.6).

### 5.6 Decide (`handlers/decided.rs`)

Anchor = `arc_newBlock{commitment = header_lean_commitment()}`, with the same
30 s wait-and-poll, SYNCING-aware, transient-tolerant loop as today. Historic
no-op when the certificate's block is already canonical (compare by
commitment). On `VALID`, the returned commitment must equal the header's. The
CL-side peer-fetch fallback is removed: fetching by commitment is the node's
job (4). `state.lean_undecided` and every stash insert/remove are removed.

### 5.7 Sync

Serve (`handlers/get_decided_values.rs`): for each height, the EVM payload from
reth as upstream; if its `prev_randao` is non-zero and the lane is on, lean
bytes from the local node by commitment; verify the recomputed commitment
equals the header field before sending; frame with `encode_value`. No offset
guess, no scan. If the local node lacks the block, skip the height with a
warning (a peer will serve it).
Receive (`handlers/process_synced_value.rs`): `decode_value`, validate (5.5),
store as upstream, stage. Transient errors → `LocalTransientError` as today.

### 5.8 Round start (`handlers/started_round.rs`)

Upstream flow. The "skip store-loaded undecided blocks in lean mode" branch and
the assembled-block stash loop are removed; re-validation of store-loaded rows
runs the EVM lane and skips the lean structural check when no bytes are
present.

### 5.9 Wire

Proposal parts and sync values: `encode_value(evm, lean_bytes, lean_lane)`
unchanged from v0.1. Flag off ⇒ upstream SSZ bytes.

## 6. What is removed from v0.1, by file

`types/block.rs` −~200 lines (commit_lanes, value_id, DecidedBlock variants,
their tests); consensus-db reverted (−27); `state.rs` stash (−14);
`decided.rs` stash handling and peer-fetch loop (−~120); `get_value.rs`
stash + "ignore previously built block" arm (−~40); `received_proposal_part.rs`
stash (−~15); `started_round.rs` assembled/skip logic (−~60);
`process_synced_value.rs` stash and value-id dedup (−~30);
`get_decided_values.rs` offset scan (−~90). Expected CL delta after this step:
about +1.4k / −140 lines versus upstream, down from +2.2k.

## 7. Failure handling

Unchanged: shim transport retry ~15 s then `TransientDependencyError`; no
verdict rather than Invalid; anchor polls to its deadline; sync answers
`LocalTransientError`. New failure: node cannot produce a block for a
commitment within the anchor deadline ⇒ height failure and restart, as today
for a missing stash.

## 8. Testing

1. Unit (arc-node): binding check true/false/no-lane; proposer builds lean
   before EVM and passes the commitment (mock generator asserts `prev_randao`);
   validation rejects header/lean mismatch; decide calls
   `new_block_by_commitment` with the header value (mock shim); sync serving
   uses the header commitment. Upstream suites stay green.
2. Unit (lean-lane): commitment index survives recovery; `arc_newBlock
   {commitment}` promotes staged, fetches from a peer, answers SYNCING;
   staged retention across a head move when the parent is an ancestor.
3. `scripts/lean-smoke.sh` in lean-lane (must still pass).
4. Local: `make testnet-lean`, 4-minute load at N=50 / 3,000 tx/s offered /
   pool-target 1,500; every block full; CL restart mid-run (one validator)
   must recover without the stash.
5. Fleet (4 machines): same protocol as `lane-bench.sh` (§6 of CLAUDE.md), at
   N=50 / 150 M, 10 minutes, fullness on every number; plus the two Track A
   outage legs. Everything a run writes on a remote lives under
   `~/arc-runs/<run-id>/` (binaries, data dirs, logs, fund file); `fleet.env`
   and `deploy-lean.sh` are re-pointed there. Nothing else in `$HOME` is
   created or removed.

## 9. Rollout

Coordinated: all validators together from height 1 of a fresh chain (wire
framing and header semantics both change). Lean node v0.2 must be deployed
before CL v0.2 (the CL calls the new verbs); v0.2 nodes serve v0.1 CLs.

## 10. Risks

- Reth could someday validate `prev_randao` against a beacon value; it does
  not today and Arc has no beacon chain. Re-check on each reth bump.
- A proposer that builds a lean block and then fails the EVM build leaves a
  staged block on its node; staged entries are bounded and reaped, no harm.
- Retaining staged entries across head moves grows memory; the queue cap
  (512) and the 64-ancestor rule bound it.
