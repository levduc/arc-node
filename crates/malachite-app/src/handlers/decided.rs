// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use eyre::{eyre, Context};
use tracing::{debug, error, info, warn};

use malachitebft_app_channel::Reply;
use malachitebft_core_types::{CommitCertificate, Context as _, Round};

use arc_consensus_types::{ArcContext, Height};
use arc_eth_engine::engine::Engine;
use arc_eth_engine::json_structures::ExecutionBlock;

use crate::block::ConsensusBlock;
use crate::payload::PaymentExecMode;
use crate::finalize::{BlockFinalizer, EngineBlockFinalizer};
use crate::metrics::AppMetrics;
use crate::state::{Decision, NextHeightInfo, State};
use crate::stats::Stats;
use crate::store::repositories::{DecidedBlocksRepository, UndecidedBlocksRepository};
use crate::store::services::{ProdPruningService, PruningService};
use crate::utils::sync_state::{sync_state, SyncState};

/// Handles the `Decided` message from the consensus engine.
///
/// This is called when the consensus engine has decided on a value for a given height and round.
/// The application processes the decided value, executes the decided block and, based on the
/// output of those steps, stores the decision result as either `Success` or `Failure`.
///
/// The `Finalized` message that will follow this message sends the appropriate `Next` message to
/// consensus to start the next height, or in case of failure, restart the current height.
///
/// The `commit_ack` channel is consumed once the certificate is durably stored, so the sync actor
/// can advertise the new tip height. If the commit fails before the store completes (block lookup
/// or storage error), the channel is dropped and no acknowledgement is sent.
#[tracing::instrument(
    name = "decided",
    skip_all,
    fields(
        height = %certificate.height,
        round = %certificate.round,
    )
)]
pub async fn handle(
    state: &mut State,
    engine: &Engine,
    payment_engine: Option<&Engine>,
    payment_builder_engine: Option<&Engine>,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    certificate: CommitCertificate<ArcContext>,
    commit_ack: Reply<()>,
) -> eyre::Result<()> {
    let decided_height = certificate.height;
    let decided_value_id = certificate.value_id;

    store_proposal_monitor_on_decision(state, decided_height, &decided_value_id).await;

    let (store, metrics, stats) = (state.store(), state.metrics(), state.stats());

    let block_finalizer = EngineBlockFinalizer::new(engine, stats, metrics);
    let pruning_service = ProdPruningService::new(store, &state.config().prune);

    let payment_exec_mode = if state.env_config().payment_deferred_exec {
        PaymentExecMode::Deferred
    } else {
        PaymentExecMode::Gated
    };
    // LEAN lane: the decide anchor needs the lean bytes, which the SSZ store
    // dropped — read them from the in-memory stash (written at get_value /
    // proposal assembly / sync). Missing after a mid-height restart -> decide
    // fails loudly and the height recovers via sync.
    let decided_vid = certificate.value_id.block_hash();
    let lean_lane = state
        .lean_undecided
        .lock()
        .expect("lean_undecided mutex poisoned")
        .get(&decided_vid)
        .cloned();
    let block = decide(
        block_finalizer,
        payment_engine,
        store, // undecided blocks repository
        store, // decided blocks repository
        pruning_service,
        certificate,
        stats,
        metrics,
        commit_ack,
        payment_exec_mode,
        lean_shim,
        lean_lane,
    )
    .await;

    match block {
        Ok((block, payment_payload)) => {
            info!("🟢 Successfully committed the decided value");
            {
                // Remove only the decided entry. A full clear() here wiped
                // already-validated NEXT-height stashes (validation of H+1
                // overlaps decide(H) at sub-second heights), forcing every
                // decide into the peer-fetch race — measured 2026-08-22:
                // 24-133 anchor failures/10min per validator, each a
                // "restarting height" retry whose delay burned ~7s round
                // timeouts on the late validator's next proposer turn
                // (cadence 0.58 blk/s vs 0.64s median height). Same bug
                // class as fc10d3c (mem::take drained future candidates).
                let mut stash = state
                    .lean_undecided
                    .lock()
                    .expect("lean_undecided mutex poisoned");
                stash.remove(&decided_vid);
                // Losing candidates (other rounds' values) accumulate; bound
                // the map with a safety valve far above any live window.
                if stash.len() > 256 {
                    warn!(
                        "lean stash grew to {} entries — clearing (losing \
                         candidates leak?)",
                        stash.len()
                    );
                    stash.clear();
                }
            }

            let catch_up_threshold = state.env_config().sync_catch_up_threshold;
            let new_sync_state = sync_state(block.timestamp, catch_up_threshold);

            if SyncState::fell_behind(state.sync_state, new_sync_state) {
                debug!("Node fell behind: transitioned from InSync to CatchingUp");
                state.metrics().inc_sync_fell_behind_count();
            }

            state.sync_state = new_sync_state;

            let next_height_info =
                prepare_next_height(decided_height, block, new_sync_state, engine).await?;

            // Phase-1 builder separation: keep the remote builder following the canonical
            // payment head (newPayload + forkchoice), and — when WE are the next proposer
            // (RoundRobin is deterministic) — chain a speculative build of our next payment
            // payload behind the feed so get_value can serve it from the stash. Everything
            // here is fire-and-forget: failures only mean get_value falls back to a local
            // build. A miss can slow a height, never break one.
            if let (Some(be), Some(pp)) = (payment_builder_engine, payment_payload) {
                let im_next = state
                    .ctx
                    .select_proposer(
                        &next_height_info.validator_set,
                        next_height_info.next_height,
                        Round::new(0),
                    )
                    .address
                    == state.address();
                let trigger = state.builder_refresher.clone();
                let payment_ts = pp.payload_inner.payload_inner.timestamp;
                let payment_number = pp.payload_inner.payload_inner.block_number;
                let be = be.clone();
                tokio::spawn(async move {
                    // SINGLE-FEEDER: only the NEXT proposer feeds the builder.
                    // All four validators feeding the same ~6MB payload each
                    // height (24MB/height inbound) saturated a wifi builder —
                    // measured: hit rate 62% wired -> 27% wifi. One feed
                    // carries all the information; a lost feed is only a
                    // stash miss (local-build fallback), never a wrong block.
                    if !im_next {
                        return;
                    }
                    let hash = pp.payload_inner.payload_inner.block_hash;
                    if let Err(e) = be.notify_new_block(&pp, Vec::new()).await {
                        debug!("builder follow: newPayload({hash}) failed: {e:#}");
                        return;
                    }
                    if let Err(e) = be.set_latest_forkchoice_state(hash).await {
                        debug!("builder follow: forkchoice({hash}) failed: {e:#}");
                        return;
                    }
                    // Hand the refresher the exact head we just fed and wake it
                    // NOW — the decide->get_value gap is too short for polling.
                    *trigger.expected.lock().await = Some((
                        arc_consensus_types::BlockHash::from(hash),
                        payment_ts,
                        payment_number,
                    ));
                    trigger.notify.notify_one();
                });
            }

            state.decision = Some(Decision::Success(Box::new(next_height_info)));
        }
        Err(e) => {
            error!("🔴 Failed to process decided value: {e:#}");

            state.decision = Some(Decision::Failure(e));
        }
    }

    Ok(())
}

/// Update proposal monitor data upon decision.
async fn store_proposal_monitor_on_decision(
    state: &mut State,
    decided_height: Height,
    decided_value_id: &arc_consensus_types::ValueId,
) {
    let Some(mut monitor) = state.proposal_monitor.take() else {
        warn!(%decided_height, "No proposal monitor found for decided height");
        return;
    };
    assert!(monitor.height == decided_height);

    monitor.mark_decided(decided_value_id);

    if let Err(e) = state.store().store_proposal_monitor_data(monitor).await {
        error!(
            %decided_height,
            "Failed to store proposal monitor data: {e}"
        );
    }
}

/// Commits a value with the given certificate, finalizes the block,
/// updates internal state and moves to the next height.
///
/// The `commit_ack` channel is consumed inside `commit` once the certificate has been durably
/// stored. If we error out before reaching the store (block lookup), the channel is dropped here
/// without firing.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
async fn decide(
    block_finalizer: impl BlockFinalizer,
    payment_engine: Option<&Engine>,
    undecided_blocks: impl UndecidedBlocksRepository,
    decided_blocks: impl DecidedBlocksRepository,
    pruning_service: impl PruningService,
    certificate: CommitCertificate<ArcContext>,
    stats: &Stats,
    metrics: &AppMetrics,
    commit_ack: Reply<()>,
    payment_exec_mode: PaymentExecMode,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    lean_lane: Option<arc_consensus_types::block::LeanLanePayload>,
) -> eyre::Result<(ExecutionBlock, Option<alloy_rpc_types_engine::ExecutionPayloadV3>)> {
    let height = certificate.height;
    let round = certificate.round;
    let value_id = certificate.value_id;

    // NOTE: here the node searches for the block with maching value_id from any round
    // It needs to read the complete undecided blocks table, but the expectation is it should be small.
    let block = match undecided_blocks
        .get_by_hash(height, value_id.block_hash())
        .await
    {
        Ok(Some(block)) => block,
        Ok(None) => {
            return Err(eyre!(
                "Cannot find undecided block for certificate with height={height}, round={round}, value_id={value_id}"
            ));
        }
        Err(e) => {
            return Err(eyre!(
                "Failed to retrieve undecided block for certificate with height={height}, round={round}, value_id={value_id}: {e}"
            ));
        }
    };

    debug!(
        "🎁 Block size: {:?}, payload size: {:?}",
        block.size_bytes(),
        block.payload_size()
    );

    // Deferred payment execution (ARC_PAYMENT_DEFERRED_EXEC=1): under Deferred
    // mode validators voted on structural validity only, so the decided payment
    // payload may not have been executed on EL2 yet. Anchor execution HERE,
    // BEFORE commit(): the CL decided store persists EVM-only payloads and
    // commit prunes the undecided copy, so executing after commit would open a
    // crash window where a decided height's payment body exists nowhere locally.
    // newPayload is idempotent (VALID for already-known blocks), so this is a
    // no-op when a background/vote-gap execution already completed.
    // An INVALID verdict here fails the height loudly (Decision::Failure ->
    // restart_height) — deterministic on every honest node, so no fork.
    if payment_exec_mode == PaymentExecMode::Deferred {
        if let (Some(pe), Some(payment_payload)) = (payment_engine, block.payment_payload.as_ref())
        {
            let wait_start = std::time::Instant::now();
            let payment_hash = payment_payload.payload_inner.payload_inner.block_hash;
            let status = pe
                .notify_new_block(payment_payload, Vec::new())
                .await
                .wrap_err_with(|| {
                    format!(
                        "payment lane (deferred): newPayload({payment_hash}) failed at height={height}"
                    )
                })?;
            match status.status {
                alloy_rpc_types_engine::PayloadStatusEnum::Valid => {}
                alloy_rpc_types_engine::PayloadStatusEnum::Invalid { validation_error } => {
                    return Err(eyre!(
                        "payment lane (deferred): decided payment block {payment_hash} at height={height} \
                         REJECTED by EL2: {validation_error} — halting height (no commit, no FCU)"
                    ));
                }
                other => {
                    return Err(eyre!(
                        "payment lane (deferred): unexpected status {other:?} for decided payment \
                         block {payment_hash} at height={height}"
                    ));
                }
            }
            debug!(
                "🪙 Deferred payment execution anchored at decide in {:?} (height {height})",
                wait_start.elapsed()
            );
        }
    }

    // LEAN lane anchor: execute + append the decided lean block on the lean
    // node BEFORE commit — the one and only feed (validation never appends;
    // arc_newBlock is permanent and idempotent by commitment). The certificate
    // binds commit_lanes(evm, lean_commitment) == value_id; verify BOTH that
    // binding and that the node's answer reproduces the commitment. Failure =
    // loud height failure (Decision::Failure -> restart/sync), never a fork.
    if let Some(shim) = lean_shim {
        let evm_hash = block.block_hash();
        let cert_bound = value_id.block_hash();
        anchor_lean_lane(shim, evm_hash, cert_bound, lean_lane.as_ref(), height)
            .await
            .wrap_err_with(|| format!("lean lane: decide anchor failed at height={height}"))?;
    }

    // Commit the decision to the store before finalizing the block.
    // This way we ensure that latest decided height >= latest finalized block.
    let new_latest_block = commit(
        block_finalizer,
        decided_blocks,
        pruning_service,
        certificate,
        &block,
        commit_ack,
    )
    .await
    .wrap_err_with(|| {
        format!("Failed to commit block at height={height}, round={round}, value_id={value_id}")
    })?;

    // Payment lane (second EL): make the decided payment block canonical on EL2,
    // mirroring the EVM finalize above. The payment payload was already validated
    // (newPayload) by every validator, so forkchoice to its hash advances EL2's head.
    if let (Some(pe), Some(payment_payload)) = (payment_engine, block.payment_payload.as_ref()) {
        let payment_hash = payment_payload.payload_inner.payload_inner.block_hash;
        pe.set_latest_forkchoice_state(payment_hash)
            .await
            .wrap_err_with(|| {
                format!("payment lane: failed to advance EL2 head to {payment_hash} at height={height}")
            })?;
        debug!("🪙 Payment lane forkchoice updated to {payment_hash} at height {height}");
    }

    // Update the latest block
    info!(
        "🔍 Updating latest block with timestamp: {:?}",
        new_latest_block.timestamp
    );

    // Update block finalize time metric
    metrics.observe_block_finalize_time(stats.height_started().elapsed().as_secs_f64());

    Ok((new_latest_block, block.payment_payload.clone()))
}

/// Anchors the decided lean block, catching the local lane up from PEER lean
/// nodes when it is behind the certificate (missed round-1 proposals, node
/// restarts, mid-height stash loss). Termination is exact and trustless: keep
/// feeding until `commit_lanes(evm_hash, local_head) == certificate value_id`
/// — every ingested block's commitment is recomputed by our own node, and the
/// final head must reproduce the certified binding, so peers cannot forge.
async fn anchor_lean_lane(
    shim: &arc_eth_engine::lean_shim::LeanShim,
    evm_hash: arc_consensus_types::BlockHash,
    cert_bound: arc_consensus_types::BlockHash,
    stashed: Option<&arc_consensus_types::block::LeanLanePayload>,
    height: Height,
) -> eyre::Result<()> {
    use arc_consensus_types::block::commit_lanes;
    use arc_eth_engine::lean_shim::NewBlockStatus;
    // Engine-API-shaped anchoring: feed what we have; a SYNCING answer (or a
    // behind head with nothing to feed) means the NODE is backfilling itself
    // from its peers — WAIT and re-poll instead of failing the height. The
    // old fail-fast turned every transient lag into a "Decision failure,
    // restarting height" loop whose delay made the validator late for its
    // next proposer turn; the laggard role then migrated around the network
    // (24-174 anchor failures/10min/validator, cadence 0.60 blk/s measured).
    // CL-side peer-fetch stays only as a fallback for nodes without --peers.
    const TOTAL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
    const NODE_HEAL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
    const POLL: std::time::Duration = std::time::Duration::from_millis(150);
    let wait_start = std::time::Instant::now();
    let mut fed_from_peers = 0u64;
    let mut iterations = 0u64;
    loop {
        iterations += 1;
        if wait_start.elapsed() > TOTAL_DEADLINE {
            let head = shim.get_head().await.map(|h| h.number).unwrap_or(0);
            return Err(eyre!(
                "lean lane: could not anchor height={height} within {TOTAL_DEADLINE:?} \
                 (local lean head {head}, {fed_from_peers} peer blocks fed) — halting"
            ));
        }
        // Node briefly AWAY (restart ~10 s): wait it out under the same
        // deadline instead of failing the height on the first miss. A
        // Decision::Failure here restarts the height every ~15 s for as long
        // as the node is down, and made this validator late for its next
        // proposer turn.
        let head = match shim.get_head().await {
            Ok(h) => h,
            Err(e) if arc_eth_engine::transient::is_transient(&e) => {
                warn!("lean lane: node unreachable at anchor ({e:#}); waiting");
                tokio::time::sleep(POLL).await;
                continue;
            }
            Err(e) => return Err(e.wrap_err("lean lane: get_head failed")),
        };
        // ALREADY-CANONICAL (sync replay of a historic height): when the lean
        // node ran ahead of this validator's consensus (v1.1 gossip), the
        // decided lean block is in our past — the certificate binds THAT
        // block's commitment, which will never equal the moving head. Anchor
        // is a no-op iff our canonical block at that number is byte-identical.
        if let Some(lane) = stashed {
            if lane.decoded.number <= head.number
                && commit_lanes(evm_hash, Some(lane.commitment())) == cert_bound
            {
                match shim.get_block_bytes(lane.decoded.number).await {
                    Ok(Some(ours)) if ours == lane.bytes => {
                        debug!(
                            "🪶 Lean lane anchored (historic no-op, block {}) at height {height}",
                            lane.decoded.number
                        );
                        return Ok(());
                    }
                    Ok(_) => {
                        return Err(eyre!(
                            "lean lane: certified historic block {} conflicts with our \
                             canonical chain at height={height} — halting",
                            lane.decoded.number
                        ));
                    }
                    Err(e) if arc_eth_engine::transient::is_transient(&e) => {
                        warn!("lean lane: node unreachable at historic anchor ({e:#}); waiting");
                        tokio::time::sleep(POLL).await;
                        continue;
                    }
                    Err(e) => return Err(e.wrap_err("lean lane: historic anchor read failed")),
                }
            }
        }
        if commit_lanes(evm_hash, Some(head.commitment)) == cert_bound {
            if fed_from_peers > 0 || iterations > 2 {
                info!(
                    "🪶 Lean lane anchored at height {height} after catch-up \
                     ({fed_from_peers} peer blocks, {iterations} polls) in {:?}",
                    wait_start.elapsed()
                );
            } else {
                debug!(
                    "🪶 Lean lane anchored at decide in {:?} (height {height})",
                    wait_start.elapsed()
                );
            }
            return Ok(());
        }
        // Fast path: the stashed decided payload extends the current head.
        if let Some(lane) = stashed {
            if lane.decoded.parent == head.commitment {
                match shim.new_block(&lane.bytes).await {
                    Ok(NewBlockStatus::Valid(_)) => continue,
                    Ok(NewBlockStatus::Syncing) => {
                        tokio::time::sleep(POLL).await;
                        continue;
                    }
                    Err(e) => {
                        warn!("lean lane: stashed anchor failed ({e:#}); trying peer catch-up");
                    }
                }
            }
        }
        // Behind. Give the node its self-backfill grace window first; only
        // then fall back to CL-side peer fetching (nodes without --peers).
        if wait_start.elapsed() < NODE_HEAL_GRACE {
            tokio::time::sleep(POLL).await;
            continue;
        }
        let next = head.number + 1;
        let mut fed = false;
        for peer in shim.peers() {
            match peer.get_block_bytes(next).await {
                Ok(Some(bytes)) => match shim.new_block(&bytes).await {
                    Ok(NewBlockStatus::Valid(_)) => {
                        fed = true;
                        fed_from_peers += 1;
                        break;
                    }
                    Ok(NewBlockStatus::Syncing) => {
                        fed = true; // node took it as a sync hint; re-poll
                        break;
                    }
                    Err(e) => {
                        warn!("lean lane: peer-fed block {next} rejected by local node: {e:#}");
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    debug!("lean lane: peer {} has no block {next}: {e:#}", peer.url());
                }
            }
        }
        if !fed {
            // Nobody has it YET (peers' own decides may still be in flight,
            // the exact race measured tonight) — wait out the deadline
            // instead of failing the height immediately.
            tokio::time::sleep(POLL).await;
        }
    }
}

/// Commits a value with the given certificate, cleanup stale consensus data and prune historical data.
///
/// The `commit_ack` channel is consumed immediately after the certificate is durably stored —
/// before any post-store work (cleanup, finalize, pruning) — so the sync actor learns the new tip
/// even if a later step fails. If the store itself fails, the channel is dropped without firing.
async fn commit(
    block_finalizer: impl BlockFinalizer,
    decided_blocks: impl DecidedBlocksRepository,
    pruning_service: impl PruningService,
    certificate: CommitCertificate<ArcContext>,
    block: &ConsensusBlock,
    commit_ack: Reply<()>,
) -> eyre::Result<ExecutionBlock> {
    let certificate_height = certificate.height;
    let certificate_round = certificate.round;
    let value_id = certificate.value_id;

    decided_blocks
        .store(certificate, block.execution_payload.clone(), block.proposer)
        .await
        .wrap_err_with(|| {
            format!("Failed to store decided block at height={certificate_height}, round={certificate_round}, value_id={value_id}")
        })?;

    if commit_ack.send(()).is_err() {
        error!(
            %certificate_height,
            "Decided: Failed to send commit acknowledgement (sync actor may not be notified)"
        );
    }

    // Clean up stale consensus data (undecided blocks and pending proposals up to the certificate height)
    if let Err(e) = pruning_service
        .clean_stale_consensus_data(certificate_height)
        .await
    {
        error!("Failed to clean stale consensus data: {e}");
    }

    // Finalize the decided payload
    let (new_latest_block, _latest_valid_hash) =
        block_finalizer.finalize_decided_block(certificate_height, &block.execution_payload)
        .await
        .wrap_err_with(|| {
            format!("Failed to finalize block at height={certificate_height}, round={certificate_round}, value_id={value_id}")
        })?;

    // Prune historical decided certificates if pruning is enabled
    if let Err(e) = pruning_service
        .prune_historical_certs(certificate_height)
        .await
    {
        error!("Failed to prune historical data: {e}");
    }

    // Prune decided blocks
    // NOTE: Always performed, even if pruning is disabled, as CL does not store
    // historical blocks anymore, besides the few needed to recover from EL amnesia.
    if let Err(e) = pruning_service.prune_decided_blocks().await {
        error!("Failed to prune decided blocks: {e}");
    }

    Ok(new_latest_block)
}

/// Prepares the state for the next height by incrementing the height,
/// fetching the new validator set and consensus params,
/// and determining the target block time based on the sync state.
///
/// ## Arguments
/// * `decided_height`: The height that was just decided.
/// * `decided_block`: The block that was just decided.
/// * `engine`: The Ethereum engine to fetch validator sets and consensus params.
async fn prepare_next_height(
    decided_height: Height,
    decided_block: ExecutionBlock,
    sync_state: SyncState,
    engine: &Engine,
) -> eyre::Result<NextHeightInfo> {
    let next_height = decided_height.increment();

    // Fetch the validator set for the next height
    // NOTE: Validator set is fetched at the decided height for the next height
    let validator_set = engine
        .eth
        .get_active_validator_set(decided_height.as_u64())
        .await
        .wrap_err_with(|| {
            format!("Failed to fetch validator set at height {decided_height} for next height {next_height}")
        })?;

    // Fetch the consensus params for the next height
    // NOTE: Consensus params are fetched at the decided height for the next height
    let consensus_params = engine
        .eth
        .get_consensus_params(decided_height.as_u64())
        .await
        .inspect_err(|e| {
            let next_height = decided_height.increment();
            error!(%decided_height, %next_height, "Failed to fetch consensus params for next height: {e}");
            error!(%decided_height, %next_height, "Using default consensus params as a fallback");
        })
        .unwrap_or_default();

    // If we are catching up, we skip the stable block times logic and start the next height right away.
    let target_time = match sync_state {
        SyncState::InSync => consensus_params.target_block_time(),
        SyncState::CatchingUp => {
            debug!("Node is catching up: no target duration for the next height");
            None
        }
    };

    Ok(NextHeightInfo {
        next_height,
        validator_set,
        consensus_params,
        decided_block,
        target_time,
    })
}

#[cfg(test)]
mod tests {

    use eyre::eyre;
    use mockall::predicate::*;

    use alloy_primitives::{Address as AlloyAddress, Bloom, Bytes as AlloyBytes, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use arc_consensus_types::signing::Signature;
    use arc_consensus_types::{Address, Height, Round, ValueId, B256};
    use malachitebft_app_channel::app::types::core::Validity;
    use malachitebft_core_types::{CommitCertificate, CommitSignature};

    use crate::finalize::MockBlockFinalizer;
    use crate::metrics::AppMetrics;
    use crate::stats::Stats;
    use crate::store::repositories::mocks::{
        MockDecidedBlocksRepository, MockUndecidedBlocksRepository,
    };
    use crate::store::services::mocks::MockPruningService;

    use arc_eth_engine::engine::{MockEngineAPI, MockEthereumAPI};

    use super::*;

    // Helper functions for creating test fixtures
    fn test_execution_block(height: u64, timestamp: u64) -> ExecutionBlock {
        ExecutionBlock {
            block_hash: B256::repeat_byte((height % 256) as u8),
            block_number: height,
            parent_hash: if height > 0 {
                B256::repeat_byte(((height - 1) % 256) as u8)
            } else {
                B256::ZERO
            },
            timestamp,
        }
    }

    fn test_execution_payload(height: u64, timestamp: u64) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: if height > 0 {
                        B256::repeat_byte(((height - 1) % 256) as u8)
                    } else {
                        B256::ZERO
                    },
                    fee_recipient: AlloyAddress::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: height,
                    gas_limit: 30000000,
                    gas_used: 0,
                    timestamp,
                    extra_data: AlloyBytes::default(),
                    base_fee_per_gas: U256::from(1u64),
                    block_hash: B256::repeat_byte((height % 256) as u8),
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    fn test_consensus_block(height: u64, round: u32, timestamp: u64) -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(height),
            round: Round::new(round),
            valid_round: Round::new(0),
            proposer: Address::default(),
            validity: Validity::Valid,
            execution_payload: test_execution_payload(height, timestamp),
            signature: Some(Signature::test()),
            payment_payload: None,
            lean_payload: None,
        }
    }

    fn test_commit_certificate(
        height: u64,
        round: u32,
        block_hash: B256,
    ) -> CommitCertificate<ArcContext> {
        CommitCertificate {
            height: Height::new(height),
            round: Round::new(round),
            value_id: ValueId::new(block_hash),
            commit_signatures: vec![CommitSignature::new(Address::default(), Signature::test())],
        }
    }

    fn test_metrics() -> AppMetrics {
        AppMetrics::default()
    }

    fn test_stats() -> Stats {
        Stats::default()
    }

    /// A dummy commit-ack channel for tests that don't assert ack delivery.
    /// The receiver is dropped, so a successful `commit_ack.send(())` will
    /// return `Err`, which `commit` logs and ignores.
    fn dummy_commit_ack() -> Reply<()> {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        tx
    }

    // Tests for decide() function

    /// Deferred mode: a decided payment block that EL2 REJECTS must fail the
    /// height BEFORE anything is committed (the payment body's only local copy
    /// is the undecided store, which commit prunes).
    #[tokio::test]
    async fn test_decide_deferred_reject_fails_before_commit() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;

        let mut consensus_block = test_consensus_block(height, round, timestamp);
        consensus_block.payment_payload = Some(test_execution_payload(height, timestamp));
        // certificate must commit to BOTH lanes now that a payment payload exists
        let certificate =
            test_commit_certificate_for_block(&consensus_block, height, round);

        let cb = consensus_block.clone();
        let mut undecided_blocks = MockUndecidedBlocksRepository::new();
        undecided_blocks
            .expect_get_by_hash()
            .return_once(move |_, _| Ok(Some(cb)));

        // EL2 rejects the deferred newPayload
        let mut mock_engine = MockEngineAPI::new();
        mock_engine.expect_new_payload().return_once(|_, _, _| {
            Ok(alloy_rpc_types_engine::PayloadStatus {
                status: alloy_rpc_types_engine::PayloadStatusEnum::Invalid {
                    validation_error: "bad state root".to_string(),
                },
                latest_valid_hash: None,
            })
        });
        let payment_engine = Engine::new(Box::new(mock_engine), Box::new(MockEthereumAPI::new()));

        // NOTHING may be committed/finalized/pruned: times(0) on all of them.
        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks.expect_store().times(0);
        let mut block_finalizer = MockBlockFinalizer::new();
        block_finalizer.expect_finalize_decided_block().times(0);
        let mut pruning_service = MockPruningService::new();
        pruning_service.expect_clean_stale_consensus_data().times(0);
        pruning_service.expect_prune_historical_certs().times(0);
        pruning_service.expect_prune_decided_blocks().times(0);

        let metrics = test_metrics();
        let stats = test_stats();

        let result = decide(
            block_finalizer,
            Some(&payment_engine),
            undecided_blocks,
            decided_blocks,
            pruning_service,
            certificate,
            &stats,
            &metrics,
            dummy_commit_ack(),
            PaymentExecMode::Deferred,
            None,
            None,
        )
        .await;

        let err = result.expect_err("EL2 rejection must fail the height");
        assert!(err.to_string().contains("REJECTED"), "unexpected error: {err:#}");
    }

    /// Certificate whose value_id commits to the block's actual lanes.
    fn test_commit_certificate_for_block(
        block: &ConsensusBlock,
        height: u64,
        round: u32,
    ) -> CommitCertificate<ArcContext> {
        CommitCertificate {
            height: Height::new(height),
            round: Round::new(round),
            value_id: ValueId::new(block.value_id()),
            commit_signatures: vec![CommitSignature::new(Address::default(), Signature::test())],
        }
    }


    // Successful decision with valid block found
    #[tokio::test]
    async fn test_decide_success() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);
        let expected_execution_block = test_execution_block(height, timestamp);

        let mut undecided_blocks = MockUndecidedBlocksRepository::new();
        undecided_blocks
            .expect_get_by_hash()
            .with(eq(Height::new(height)), eq(block_hash))
            .return_once(move |_, _| Ok(Some(consensus_block.clone())));

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks
            .expect_store()
            .return_once(move |cert, payload, proposer| {
                assert_eq!(cert.height, Height::new(height));
                assert_eq!(cert.round, Round::new(round));
                assert_eq!(payload.payload_inner.payload_inner.block_number, height);
                assert_eq!(proposer, Address::default());
                Ok(())
            });

        let mut block_finalizer = MockBlockFinalizer::new();
        block_finalizer
            .expect_finalize_decided_block()
            .return_once(move |h, _| {
                assert_eq!(h, Height::new(height));
                Ok((expected_execution_block, block_hash))
            });

        let mut pruning_service = MockPruningService::new();
        pruning_service
            .expect_clean_stale_consensus_data()
            .return_once(|_| Ok(()));
        pruning_service
            .expect_prune_historical_certs()
            .return_once(|_| Ok(vec![]));
        pruning_service
            .expect_prune_decided_blocks()
            .return_once(|| Ok(vec![]));

        let metrics = test_metrics();
        let stats = test_stats();

        let result = decide(
            block_finalizer,
            None,
            undecided_blocks,
            decided_blocks,
            pruning_service,
            certificate,
            &stats,
            &metrics,
            dummy_commit_ack(),
            PaymentExecMode::Gated,
            None,
            None,
        )
        .await;

        let (block, _payment) = result.unwrap();
        assert_eq!(block.block_number, height);
        assert_eq!(block.timestamp, timestamp);
    }

    // Block not found in undecided blocks
    #[tokio::test]
    async fn test_decide_block_not_found() {
        let height = 5u64;
        let round = 2u32;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);

        let mut undecided_blocks = MockUndecidedBlocksRepository::new();
        undecided_blocks
            .expect_get_by_hash()
            .with(eq(Height::new(height)), eq(block_hash))
            .return_once(|_, _| Ok(None));

        let decided_blocks = MockDecidedBlocksRepository::new();
        let block_finalizer = MockBlockFinalizer::new();
        let pruning_service = MockPruningService::new();
        let metrics = test_metrics();
        let stats = test_stats();

        let result = decide(
            block_finalizer,
            None,
            undecided_blocks,
            decided_blocks,
            pruning_service,
            certificate,
            &stats,
            &metrics,
            dummy_commit_ack(),
            PaymentExecMode::Gated,
            None,
            None,
        )
        .await;

        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Cannot find undecided block"));
    }

    // Repository error when fetching undecided block
    #[tokio::test]
    async fn test_decide_undecided_blocks_fetch_error() {
        let height = 5u64;
        let round = 2u32;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);

        let mut undecided_blocks = MockUndecidedBlocksRepository::new();
        undecided_blocks
            .expect_get_by_hash()
            .with(eq(Height::new(height)), eq(block_hash))
            .return_once(|_, _| Err(std::io::Error::other("Database error")));

        let decided_blocks = MockDecidedBlocksRepository::new();
        let block_finalizer = MockBlockFinalizer::new();
        let pruning_service = MockPruningService::new();
        let metrics = test_metrics();
        let stats = test_stats();

        let result = decide(
            block_finalizer,
            None,
            undecided_blocks,
            decided_blocks,
            pruning_service,
            certificate,
            &stats,
            &metrics,
            dummy_commit_ack(),
            PaymentExecMode::Gated,
            None,
            None,
        )
        .await;

        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to retrieve undecided block"));
    }

    // Commit failure propagates error
    #[tokio::test]
    async fn test_decide_commit_failure() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);

        let mut undecided_blocks = MockUndecidedBlocksRepository::new();
        undecided_blocks
            .expect_get_by_hash()
            .with(eq(Height::new(height)), eq(block_hash))
            .return_once(move |_, _| Ok(Some(consensus_block.clone())));

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks
            .expect_store()
            .return_once(|_, _, _| Err(std::io::Error::other("Store failed")));

        let block_finalizer = MockBlockFinalizer::new();
        let pruning_service = MockPruningService::new();
        let metrics = test_metrics();
        let stats = test_stats();

        let result = decide(
            block_finalizer,
            None,
            undecided_blocks,
            decided_blocks,
            pruning_service,
            certificate,
            &stats,
            &metrics,
            dummy_commit_ack(),
            PaymentExecMode::Gated,
            None,
            None,
        )
        .await;

        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to commit block"));
    }

    // Tests for commit() function

    // Successful commit flow
    #[tokio::test]
    async fn test_commit_success() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);
        let expected_execution_block = test_execution_block(height, timestamp);

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks
            .expect_store()
            .return_once(move |cert, payload, proposer| {
                assert_eq!(cert.height, Height::new(height));
                assert_eq!(cert.round, Round::new(round));
                assert_eq!(payload.payload_inner.payload_inner.block_number, height);
                assert_eq!(proposer, Address::default());
                Ok(())
            });

        let mut block_finalizer = MockBlockFinalizer::new();
        block_finalizer
            .expect_finalize_decided_block()
            .return_once(move |h, _| {
                assert_eq!(h, Height::new(height));
                Ok((expected_execution_block, block_hash))
            });

        let mut pruning_service = MockPruningService::new();
        pruning_service
            .expect_clean_stale_consensus_data()
            .with(eq(Height::new(height)))
            .return_once(|_| Ok(()));
        pruning_service
            .expect_prune_historical_certs()
            .with(eq(Height::new(height)))
            .return_once(|_| Ok(vec![Height::new(1), Height::new(2)]));
        pruning_service
            .expect_prune_decided_blocks()
            .return_once(|| Ok(vec![Height::new(0)]));

        let result = commit(
            block_finalizer,
            decided_blocks,
            pruning_service,
            certificate,
            &consensus_block,
            dummy_commit_ack(),
        )
        .await;

        let block = result.unwrap();
        assert_eq!(block.block_number, height);
        assert_eq!(block.timestamp, timestamp);
    }

    // DecidedBlocksRepository store failure
    #[tokio::test]
    async fn test_commit_store_failure() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks
            .expect_store()
            .return_once(|_, _, _| Err(std::io::Error::other("Store failed")));

        let block_finalizer = MockBlockFinalizer::new();
        let pruning_service = MockPruningService::new();

        let result = commit(
            block_finalizer,
            decided_blocks,
            pruning_service,
            certificate,
            &consensus_block,
            dummy_commit_ack(),
        )
        .await;

        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to store decided block"));
    }

    // BlockFinalizer finalization failure
    #[tokio::test]
    async fn test_commit_finalization_failure() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks.expect_store().return_once(|_, _, _| Ok(()));

        let mut block_finalizer = MockBlockFinalizer::new();
        block_finalizer
            .expect_finalize_decided_block()
            .return_once(|_, _| Err(eyre!("Finalization failed")));

        let mut pruning_service = MockPruningService::new();
        pruning_service
            .expect_clean_stale_consensus_data()
            .return_once(|_| Ok(()));

        let result = commit(
            block_finalizer,
            decided_blocks,
            pruning_service,
            certificate,
            &consensus_block,
            dummy_commit_ack(),
        )
        .await;

        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to finalize block"));
    }

    // Pruning errors are logged but don't fail the operation
    #[tokio::test]
    async fn test_commit_pruning_errors_logged_not_fatal() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);
        let expected_execution_block = test_execution_block(height, timestamp);

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks.expect_store().return_once(|_, _, _| Ok(()));

        let mut block_finalizer = MockBlockFinalizer::new();
        block_finalizer
            .expect_finalize_decided_block()
            .return_once(move |_, _| Ok((expected_execution_block, block_hash)));

        let mut pruning_service = MockPruningService::new();

        // Stale data cleanup fails - but is logged, not fatal
        pruning_service
            .expect_clean_stale_consensus_data()
            .return_once(|_| Err(std::io::Error::other("Cleanup failed")));

        // Historical cert pruning fails - but is logged, not fatal
        pruning_service
            .expect_prune_historical_certs()
            .return_once(|_| Err(std::io::Error::other("Historical prune failed")));

        // Decided blocks pruning must succeed
        pruning_service
            .expect_prune_decided_blocks()
            .return_once(|| Ok(vec![]));

        let result = commit(
            block_finalizer,
            decided_blocks,
            pruning_service,
            certificate,
            &consensus_block,
            dummy_commit_ack(),
        )
        .await;

        // Despite pruning errors, the commit should succeed
        let block = result.unwrap();
        assert_eq!(block.block_number, height);
    }

    /// commit_ack must fire after the certificate is durably stored, even if a later
    /// post-store step (finalize) fails — sync should still learn the new tip.
    #[tokio::test]
    async fn test_commit_acks_when_store_succeeds_but_finalize_fails() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks.expect_store().return_once(|_, _, _| Ok(()));

        let mut block_finalizer = MockBlockFinalizer::new();
        block_finalizer
            .expect_finalize_decided_block()
            .return_once(|_, _| Err(eyre!("Finalization failed")));

        let mut pruning_service = MockPruningService::new();
        pruning_service
            .expect_clean_stale_consensus_data()
            .return_once(|_| Ok(()));

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();

        let result = commit(
            block_finalizer,
            decided_blocks,
            pruning_service,
            certificate,
            &consensus_block,
            ack_tx,
        )
        .await;

        assert!(result.is_err(), "expected finalize failure");
        assert_eq!(
            ack_rx.await,
            Ok(()),
            "ack must be sent when store succeeds, even if finalize later fails"
        );
    }

    /// commit_ack must NOT fire if the durable store fails — otherwise sync
    /// would advertise a height we cannot serve.
    #[tokio::test]
    async fn test_commit_does_not_ack_when_store_fails() {
        let height = 5u64;
        let round = 2u32;
        let timestamp = 1000u64;
        let block_hash = B256::repeat_byte((height % 256) as u8);
        let certificate = test_commit_certificate(height, round, block_hash);
        let consensus_block = test_consensus_block(height, round, timestamp);

        let mut decided_blocks = MockDecidedBlocksRepository::new();
        decided_blocks
            .expect_store()
            .return_once(|_, _, _| Err(std::io::Error::other("Store failed")));

        let block_finalizer = MockBlockFinalizer::new();
        let pruning_service = MockPruningService::new();

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();

        let result = commit(
            block_finalizer,
            decided_blocks,
            pruning_service,
            certificate,
            &consensus_block,
            ack_tx,
        )
        .await;

        assert!(result.is_err(), "expected store failure");
        // Sender dropped without sending → recv resolves to Err(RecvError)
        assert!(
            ack_rx.await.is_err(),
            "ack channel must be dropped (not sent) when store fails"
        );
    }
}
