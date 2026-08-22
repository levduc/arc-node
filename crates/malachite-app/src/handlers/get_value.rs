// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
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

use std::time::{Duration, Instant};

use eyre::{eyre, Context};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use malachitebft_app_channel::app::streaming::StreamId;
use malachitebft_app_channel::app::types::core::Round;
use malachitebft_app_channel::app::types::LocallyProposedValue;
use malachitebft_app_channel::{NetworkMsg, Reply};
use malachitebft_core_types::Validity;

use arc_consensus_types::{Address, ArcContext, Height};
use arc_eth_engine::engine::Engine;
use arc_eth_engine::json_structures::ExecutionBlock;
use arc_signer::ArcSigningProvider;

use crate::block::ConsensusBlock;
use crate::metrics::AppMetrics;
use crate::payload::{
    generate_payload_with_retry, validate_consensus_block, EnginePayloadGenerator,
    EnginePayloadValidator, PaymentExecMode};
use crate::proposal_parts::{prepare_stream, stream_proposal};
use crate::state::State;
use crate::store::repositories::UndecidedBlocksRepository;
use crate::store::Store;
use crate::utils::pretty::PrettyPayload;

type NetworkHandle = mpsc::Sender<NetworkMsg<ArcContext>>;

/// Handles the `GetValue` message from the consensus engine.
///
/// This is called when the consensus engine requests a value to propose for a specific height and round.
///
/// - The application first checks if there are any previously built blocks for the given height and round.
/// - If such blocks exist, it selects the first one to propose.
/// - If no previously built blocks are found, the application builds a new block using the execution engine,
///   validates it, and prepares it for proposal.
/// - Finally, it sends the proposed value back to the consensus engine and streams the proposal parts over the network.
pub async fn handle(
    state: &mut State,
    network: NetworkHandle,
    engine: &Engine,
    payment_engine: Option<&Engine>,
    payment_builder_engine: Option<&Engine>,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    height: Height,
    round: Round,
    timeout: Duration,
    reply: Reply<LocallyProposedValue<ArcContext>>,
) -> eyre::Result<()> {
    let metrics = state.metrics().clone();
    let store = state.store().clone();

    let address = state.address();
    let fee_recipient = state.fee_recipient();
    let stream_id = state.next_stream_id(height, round);
    let previous_block = state.previous_block.as_ref();
    let signing_provider = state.signing_provider();

    let prebuilt_slot = state.builder_prebuilt.clone();
    let payment_exec_mode = if state.env_config().payment_deferred_exec {
        PaymentExecMode::Deferred
    } else {
        PaymentExecMode::Gated
    };
    let compact_payment = state.env_config().compact_payment_proposals;
    let lean_budget_gas = state.env_config().payment_lean_budget_gas;
    let proposed_value = on_get_value(
        network,
        engine,
        payment_engine,
        payment_builder_engine,
        lean_shim,
        lean_budget_gas,
        prebuilt_slot,
        payment_exec_mode,
        compact_payment,
        metrics,
        store,
        height,
        round,
        address,
        previous_block,
        fee_recipient,
        signing_provider,
        stream_id,
        timeout,
    )
    .await;

    // A proposer that fails to BUILD (e.g. a lane's engine getPayload times out under load)
    // must not crash the node: skip the reply so this round simply times out and consensus
    // moves on (identical to the existing None path). The next round/proposer recovers.
    let proposed_value = match proposed_value {
        Ok(v) => v,
        Err(e) => {
            error!(%height, %round, "GetValue: failed to build proposal; skipping round: {e:#}");
            return Ok(());
        }
    };

    if let Some(proposed_value) = proposed_value {
        if round.as_i64() == 0 {
            if let Some(monitor) = &mut state.proposal_monitor {
                debug_assert_eq!(monitor.height, height, "proposal monitor height mismatch");
                monitor.record_proposal(proposed_value.value.id());
            } else {
                warn!(%height, %round, "No proposal monitor present");
            }
        }

        if let Err(e) = reply.send(proposed_value) {
            error!("🔴 GetValue: Failed to send reply: {e:?}");
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn on_get_value(
    network: NetworkHandle,
    engine: &Engine,
    payment_engine: Option<&Engine>,
    payment_builder_engine: Option<&Engine>,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    lean_budget_gas: u64,
    prebuilt_slot: crate::builder_prebuild::PrebuiltSlot,
    payment_exec_mode: PaymentExecMode,
    compact_payment: bool,
    metrics: AppMetrics,
    store: Store,
    height: Height,
    round: Round,
    address: Address,
    previous_block: Option<&ExecutionBlock>,
    fee_recipient: Address,
    signing_provider: &ArcSigningProvider,
    stream_id: StreamId,
    timeout: Duration,
) -> eyre::Result<Option<LocallyProposedValue<ArcContext>>> {
    let block = get_previously_built_block(&store, address, height, round)
        .await
        .wrap_err_with(|| {
            format!(
                "Proposer failed to get previously built blocks (if any) for height {} and round {}",
                height, round,
            )
        })?;

    let mut block = match block {
        Some(block) => {
            info!(block_hash = %block.block_hash(), "✅ Using previously built block");
            block
        }
        None => {
            info!(%height, %round, "🌈 Building new block");

            let previous_block = previous_block.ok_or_else(|| {
                eyre!("No previous block available to build new block at height={height} and round={round}")
            })?;

            let task = build_and_validate_block(
                engine,
                payment_engine,
                payment_builder_engine,
                lean_shim,
                lean_budget_gas,
                prebuilt_slot,
                payment_exec_mode,
                &metrics,
                &store,
                height,
                round,
                address,
                previous_block,
                &fee_recipient,
            );

            let result = match tokio::time::timeout(timeout, task).await {
                Ok(result) => result,
                Err(_) => {
                    error!(%height, %round, "⏰ Proposer timed out while building block after {timeout:?}");
                    return Ok(None);
                }
            };

            result.wrap_err_with(|| {
                format!("Proposer failed to build new block at height={height} and round={round}")
            })?
        }
    };

    let proposed_value = LocallyProposedValue::from(&block);

    debug!(
        %height, %round,
        block_size = %block.size_bytes(),
        payload_size = %block.payload_size(),
        "🎁 Sending proposal: {proposed_value:?}",
    );

    let block_hash = block.block_hash();

    let (stream_messages, signature) =
        prepare_stream(stream_id, signing_provider, &block, compact_payment)
        .await
        .wrap_err_with(|| {
            format!(
                "Proposer failed to prepare stream for block {block_hash} \
                it wants to propose at height={height}, round={round}",
            )
        })?;

    // Store the block with its signature
    block.signature = Some(signature);
    store
        .store_undecided_block(block)
        .await
        .wrap_err_with(|| format!("Proposer failed to store block {block_hash}"))?;

    tokio::spawn(async move {
        if let Err(e) = stream_proposal(network, height, round, stream_messages).await {
            error!(%height, %round, "🔴 Failed to stream proposal parts: {e:#}");
        }
    });

    debug!(%height, %round, "✅ Proposal sent");

    Ok(Some(proposed_value))
}

/// Builds a new execution payload and validates it via the Engine API.
///
/// If the engine rejects the payload, an [`InvalidPayload`] record is
/// persisted (handled by [`validate_consensus_block`]) and the function
/// returns an error since a self-built block should never be invalid.
#[allow(clippy::too_many_arguments)]
async fn build_and_validate_block(
    engine: &Engine,
    payment_engine: Option<&Engine>,
    payment_builder_engine: Option<&Engine>,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    lean_budget_gas: u64,
    prebuilt_slot: crate::builder_prebuild::PrebuiltSlot,
    payment_exec_mode: PaymentExecMode,
    metrics: &AppMetrics,
    store: &Store,
    height: Height,
    round: Round,
    proposer: Address,
    previous_block: &ExecutionBlock,
    fee_recipient: &Address,
) -> eyre::Result<ConsensusBlock> {
    let start = Instant::now();

    let block = build_block(
        engine,
        payment_engine,
        payment_builder_engine,
        lean_shim,
        lean_budget_gas,
        prebuilt_slot,
        metrics,
        height,
        round,
        proposer,
        previous_block,
        fee_recipient,
    )
    .await?;

    let validator = EnginePayloadValidator::new(engine, metrics);
    let validity = validate_consensus_block(
        &validator,
        payment_engine,
        None,
        &block,
        store,
        metrics,
        payment_exec_mode,
        None,
    )
        .await
        .wrap_err_with(|| {
            format!(
                "Payload validation failed on self-built block at height={height}, round={round}: {}",
                block.block_hash()
            )
        })?;

    if !validity.is_valid() {
        return Err(eyre!("Self-built block {} is invalid", block.block_hash()));
    }

    // Deferred mode: in Gated mode the validate above already newPayload'd the
    // payment payload into EL2's tree; in Deferred mode it did not, and a
    // getPayload-built payload is not yet an inserted block — feed it now,
    // concurrently with streaming/voting, so decide's anchor finds it warm.
    if payment_exec_mode == PaymentExecMode::Deferred {
        if let (Some(pe), Some(pp)) = (payment_engine, block.payment_payload.as_ref()) {
            let pe = pe.clone();
            let pp = pp.clone();
            tokio::spawn(async move {
                let hash = pp.payload_inner.payload_inner.block_hash;
                if let Err(e) = pe.notify_new_block(&pp, Vec::new()).await {
                    debug!("deferred proposer-side execution of {hash} failed (decide will retry): {e:#}");
                }
            });
        }
    }

    debug!(
        "✅ Proposer validated self-built block {}",
        block.block_hash()
    );

    metrics.observe_block_build_time(start.elapsed().as_secs_f64());

    Ok(block)
}

/// Build a new block, validate it, and store it alongside its corresponding proposal.
///
/// Includes timing delay enforcement to ensure proper block intervals
pub async fn build_block(
    engine: &Engine,
    payment_engine: Option<&Engine>,
    payment_builder_engine: Option<&Engine>,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    lean_budget_gas: u64,
    prebuilt_slot: crate::builder_prebuild::PrebuiltSlot,
    metrics: &AppMetrics,
    height: Height,
    round: Round,
    proposer: Address,
    previous_block: &ExecutionBlock,
    fee_recipient: &Address,
) -> eyre::Result<ConsensusBlock> {
    let generator = EnginePayloadGenerator { engine }; // TODO: make this configurable

    let execution_payload =
        generate_payload_with_retry(previous_block, fee_recipient, &generator, metrics).await?;

    debug!(
        "🌈 Got execution payload: {:?}",
        PrettyPayload(&execution_payload)
    );

    // LEAN payment lane (ARC_PAYMENT_LEAN_LANE): build the next lean block on
    // the lean node's head, timestamp locked to the EVM lane (ts_ms = evm_ts * 1000)
    // so the two lanes advance in lockstep. The commitment is RECOMPUTED from
    // the returned bytes by LeanLanePayload::new — the shim's answer is only
    // cross-checked, never trusted.
    let lean_payload = match lean_shim {
        Some(shim) => {
            let head = shim
                .get_head()
                .await
                .wrap_err("lean lane: failed to fetch head for build")?;
            let ts_ms = execution_payload.timestamp() * 1000;
            let (claimed, bytes) = shim
                .build_block(head.commitment, head.number + 1, ts_ms, lean_budget_gas)
                .await
                .wrap_err("lean lane: buildBlock failed")?;
            let lane = arc_consensus_types::block::LeanLanePayload::new(bytes)
                .wrap_err("lean lane: built block failed strict decode")?;
            if lane.commitment() != claimed {
                return Err(eyre!(
                    "lean lane: recomputed commitment {} != shim's claimed {claimed}",
                    lane.commitment()
                ));
            }
            debug!(
                number = head.number + 1,
                txs = lane.decoded.tx_count,
                commitment = %lane.commitment(),
                "🪶 built lean payment block"
            );
            Some(lane)
        }
        None => None,
    };

    // Payment lane (second EL): build a payment payload on top of EL2's own head,
    // aligned to the EVM lane's timestamp so the two lanes advance in lockstep.
    let payment_payload = match payment_engine {
        Some(pe) => {
            let payment_parent = pe
                .eth
                .get_block_by_number("latest")
                .await
                .wrap_err("payment lane: failed to fetch EL2 head")?
                .ok_or_else(|| eyre!("payment lane: EL2 has no latest block"))?;
            let timestamp = execution_payload.timestamp();

            // Phase-1 builder separation v1 (docs/deferred-exec-100k.md): consume the
            // payload the remote builder PRE-BUILT for this turn (kicked at decide time).
            // Serve it only on an exact attribute match; any miss falls back to the local
            // build — v0's on-demand remote build is gone (it measured -5..-10% because it
            // put the build AND the builder's catch-up execution on the critical path).
            // Validation of the payload stays on the LOCAL engine either way.
            // The refresher's cycle (feed exec + confirm + 2 builds) lands ~350ms after
            // decide, but with DEFERRED exec get_value arrives ~30ms after decide — it
            // always beat the stash (measured: 0% hits at 2 blk/s, stale-candidate
            // misses before that). So: WAIT bounded for the in-flight prebuild instead
            // of falling back instantly — strictly better than the local fallback,
            // whose own build deadline is 500ms. Remove only the matched entry (the
            // old mem::take drained future-ts candidates on every miss).
            let mut prebuilt = None;
            if payment_builder_engine.is_some() {
                let wait_until =
                    tokio::time::Instant::now() + std::time::Duration::from_millis(450);
                loop {
                    {
                        let mut stash = prebuilt_slot.lock().await;
                        if let Some(i) = stash.iter().position(|p| {
                            p.parent == payment_parent.block_hash
                                && p.timestamp == timestamp
                                && p.fee_recipient == *fee_recipient
                                // Empty-candidate guard: a tx-starved builder produces
                                // VALID but EMPTY payloads; treat as a miss — the local
                                // path packs from our own pool.
                                && !p.payload.payload_inner.payload_inner.transactions.is_empty()
                        }) {
                            let p = stash.remove(i);
                            info!("🏗️ prebuilt payment payload HIT (builder-served, zero build on path)");
                            prebuilt = Some(p.payload);
                        }
                    }
                    if prebuilt.is_some() || tokio::time::Instant::now() >= wait_until {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                if prebuilt.is_none() {
                    let stash = prebuilt_slot.lock().await;
                    let ts_seen: Vec<u64> = stash.iter().map(|p| p.timestamp).collect();
                    warn!(
                        candidates = stash.len(), ts_prebuilt = ?ts_seen, ts_needed = timestamp,
                        "builder prebuilt MISS after wait; building locally"
                    );
                }
            }

            let payment_payload = match prebuilt {
                Some(p) => p,
                None => pe
                    .generate_block(&payment_parent, timestamp, fee_recipient)
                    .await
                    .wrap_err("payment lane: failed to build payment payload")?,
            };
            debug!(
                "🪙 Got payment payload: {:?}",
                PrettyPayload(&payment_payload)
            );
            Some(payment_payload)
        }
        None => None,
    };

    Ok(ConsensusBlock {
        height,
        round,
        valid_round: Round::Nil,
        proposer,
        validity: Validity::Valid,
        execution_payload,
        signature: None,
        payment_payload,
        lean_payload,
    })
}

/// Retrieves the previously built block for the given height and round.
/// Called by the consensus engine to re-use a previously built block.
/// Returns the first block found for the given height and round with the matching proposer.
///
/// There should be at most one block for a given height and round when the proposer is not byzantine.
/// We assume this implementation is not byzantine and we are the proposer for the given height and round.
/// Therefore there must be a single block for the rounds where we are the proposer, with the proposer address matching our own.
async fn get_previously_built_block(
    undecided_blocks: impl UndecidedBlocksRepository,
    proposer: Address,
    height: Height,
    round: Round,
) -> eyre::Result<Option<ConsensusBlock>> {
    let blocks = undecided_blocks.get_by_round(height, round).await?;
    let block = blocks.into_iter().find(|p| p.proposer == proposer);
    Ok(block)
}
