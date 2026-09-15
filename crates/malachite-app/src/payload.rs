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

use std::time::Duration;

use backon::{BackoffBuilder, ConstantBuilder, Retryable};
use eyre::Context as _;
use tracing::{error, warn};

use malachitebft_app_channel::app::types::core::Validity;

use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadStatusEnum};

use arc_consensus_types::block::LeanLanePayload;
use arc_consensus_types::{Address, BlockHash, Height, Round, B256};
use arc_eth_engine::deadline::EngineDeadline;
use arc_eth_engine::engine::Engine;
use arc_eth_engine::json_structures::ExecutionBlock;
use arc_eth_engine::lean_shim::{LeanBuilder, LeanBytesResolver, LeanCatchup, LeanHead};
use arc_eth_engine::rpc::EngineApiRpcError;
/// Re-exported for the handlers: "no verdict right now" vs a real failure.
pub use arc_eth_engine::transient::is_transient;
use arc_eth_engine::transient::TransientDependencyError;

use crate::block::ConsensusBlock;
use crate::metrics::app::{AppMetrics, InvalidPayloadSource, LeanNoVerdictReason};
use crate::store::repositories::InvalidPayloadsRepository;
use arc_consensus_db::invalid_payloads::InvalidPayload;

/// Everything the proposer needs to build the lean block that the EVM
/// header will commit to (spec §5.4).
pub struct LeanBuild<'a, B: LeanBuilder> {
    pub builder: &'a B,
    pub head: LeanHead,
    pub budget_gas: u64,
}

pub async fn generate_payload_with_retry(
    previous_block: &ExecutionBlock,
    fee_recipient: &Address,
    generator: &impl PayloadGenerator,
    metrics: &AppMetrics,
    lean: Option<LeanBuild<'_, impl LeanBuilder>>,
) -> eyre::Result<(ExecutionPayloadV3, Option<LeanLanePayload>)> {
    const MAX_RETRIES: usize = 5;
    const RETRY_POLICY: ConstantBuilder = ConstantBuilder::new()
        .with_delay(Duration::from_millis(100))
        .with_max_times(MAX_RETRIES);

    let lean = lean.as_ref();

    let call_once = || async {
        // Ensure timestamp is non-decreasing by setting it to max(previous_block.timestamp, now())
        // This allows us to continue making progress, proposing blocks that have
        // the same block timestamp as the "jumped" block until enough time has elapsed such
        // that we can continue making progress with advancing timestamps.
        let now = Engine::timestamp_now();
        let timestamp = std::cmp::max(previous_block.timestamp, now);

        if previous_block.timestamp > now {
            // timestamp >= now (since max chose previous_block.timestamp > now)
            let skew = timestamp.saturating_sub(now);
            warn!(
                timestamp = timestamp,
                skew = skew,
                "Clock skew detected: using parent timestamp",
            );
        }

        // LEAN lane: build the lane block FIRST, timestamp-locked to the EVM
        // payload we are about to request, and bind it into the EVM header.
        // The commitment is RECOMPUTED from the returned bytes by
        // `LeanLanePayload::new` — the shim's claimed commitment is only
        // cross-checked, never trusted.
        let lean_payload = match lean {
            Some(l) => {
                let built = l
                    .builder
                    .build_lean_block(l.head, timestamp.saturating_mul(1000), l.budget_gas)
                    .await
                    .wrap_err("lean lane: buildBlock failed")?;
                let lane = LeanLanePayload::new(built.bytes)
                    .wrap_err("lean lane: built block failed strict decode")?;
                if lane.commitment() != built.commitment {
                    return Err(eyre::eyre!(
                        "lean lane: recomputed commitment {} != shim's claimed {}",
                        lane.commitment(),
                        built.commitment
                    ));
                }
                crate::height_timing::mark(crate::height_timing::Phase::BuildLean);
                crate::height_timing::set_lean_txs(lane.decoded.tx_count);
                Some(lane)
            }
            None => None,
        };
        let prev_randao = lean_payload
            .as_ref()
            .map(|l| l.commitment())
            .unwrap_or(B256::ZERO);

        let _guard = metrics.start_engine_api_timer("generate_block");

        let payload = generator
            .generate_block(previous_block, timestamp, fee_recipient, prev_randao)
            .await?;

        crate::height_timing::mark(crate::height_timing::Phase::BuildEvm);

        Ok((payload, lean_payload))
    };

    let mut attempt_num = 0usize;

    call_once
        .retry(RETRY_POLICY.build())
        .sleep(tokio::time::sleep) // give reth time to breathe
        .notify(|_e, dur| {
            // Bounded by MAX_RETRIES (5)
            #[allow(clippy::arithmetic_side_effects)]
            {
                attempt_num += 1;
            }
            let attempts_left = MAX_RETRIES.saturating_sub(attempt_num);
            error!(
                attempt = attempt_num,
                attempts_left,
                delay_ms = dur.as_millis(),
                "reth forgot its payload id; retrying (forking off the same previous block)"
            );
        })
        .when(|e| {
            EngineApiRpcError::try_from(e)
                .map(|err| err.is_unknown_payload())
                .unwrap_or(false)
        })
        .await
}

/// Introduced to improve testability of `generate_payload_with_retry`
#[cfg_attr(test, mockall::automock)]
pub trait PayloadGenerator: Send + Sync {
    async fn generate_block(
        &self,
        parent: &ExecutionBlock,
        timestamp: u64,
        fee_recipient: &Address,
        prev_randao: B256,
    ) -> eyre::Result<ExecutionPayloadV3>;
}

pub struct EnginePayloadGenerator<'a> {
    pub engine: &'a Engine,
    /// Consensus budget for the proposer's build sequence; extends the
    /// engine's per-call timeout floors when set.
    pub deadline: Option<EngineDeadline>,
}

impl<'a> PayloadGenerator for EnginePayloadGenerator<'a> {
    async fn generate_block(
        &self,
        parent: &ExecutionBlock,
        timestamp: u64,
        fee_recipient: &Address,
        prev_randao: B256,
    ) -> eyre::Result<ExecutionPayloadV3> {
        self.engine
            .generate_block(parent, timestamp, fee_recipient, prev_randao, self.deadline)
            .await
    }
}

/// Abstraction over execution payload validation.
///
/// This trait exists so that handler code can validate payloads
/// without depending on the concrete [`Engine`] type, making it
/// possible to substitute a mock in unit tests.
#[cfg_attr(test, mockall::automock)]
pub trait PayloadValidator {
    /// Validates an execution payload via the engine.
    async fn validate_payload(
        &self,
        payload: &ExecutionPayloadV3,
    ) -> eyre::Result<PayloadValidationResult>;
}

impl<T> PayloadValidator for &T
where
    T: PayloadValidator + ?Sized,
{
    async fn validate_payload(
        &self,
        payload: &ExecutionPayloadV3,
    ) -> eyre::Result<PayloadValidationResult> {
        (*self).validate_payload(payload).await
    }
}

/// [`PayloadValidator`] backed by a real [`Engine`] instance.
///
/// Delegates to the module-private [`validate_payload`] function,
/// which sends the payload to the execution client via
/// `engine.newPayload` and interprets the response.
pub struct EnginePayloadValidator<'a> {
    engine: &'a Engine,
    metrics: &'a AppMetrics,
    deadline: Option<EngineDeadline>,
}

impl<'a> EnginePayloadValidator<'a> {
    pub fn new(engine: &'a Engine, metrics: &'a AppMetrics) -> Self {
        Self {
            engine,
            metrics,
            deadline: None,
        }
    }

    /// Validator for the proposer's self-validation path: the consensus
    /// budget extends the engine's per-call timeout floor.
    pub fn new_with_deadline(
        engine: &'a Engine,
        metrics: &'a AppMetrics,
        deadline: EngineDeadline,
    ) -> Self {
        Self {
            engine,
            metrics,
            deadline: Some(deadline),
        }
    }
}

impl PayloadValidator for EnginePayloadValidator<'_> {
    async fn validate_payload(
        &self,
        payload: &ExecutionPayloadV3,
    ) -> eyre::Result<PayloadValidationResult> {
        validate_payload(self.engine, payload, self.metrics, self.deadline).await
    }
}

/// Result of validating an execution payload via the engine.
///
/// Carries the engine's verdict so that callers can act on it (e.g. store the
/// rejection reason) without losing the detail across the call boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadValidationResult {
    /// The engine accepted the payload.
    Valid,
    /// The engine rejected the payload for the given reason.
    Invalid { reason: String },
}

impl std::fmt::Display for PayloadValidationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valid => write!(f, "Valid"),
            Self::Invalid { reason } => {
                write!(f, "Invalid: {reason}")
            }
        }
    }
}

/// Validates an execution payload by sending it to the engine via `newPayload`.
///
/// # Return values
///
/// - `Ok(Valid)`: the engine accepted the payload.
/// - `Ok(Invalid { reason })`: the engine rejected the payload, either via its
///   status response (`INVALID`) or via a non-internal JSON-RPC error.
/// - `Err(..)`: no verdict was obtained — the engine replied with an unexpected
///   status (`SYNCING` or `ACCEPTED`, logged as a warning), returned a JSON-RPC
///   internal error, or an unrelated internal error occurred in the call stack.
async fn validate_payload(
    engine: &Engine,
    execution_payload: &ExecutionPayloadV3,
    metrics: &AppMetrics,
    deadline: Option<EngineDeadline>,
) -> eyre::Result<PayloadValidationResult> {
    let block_hash = execution_payload.payload_inner.payload_inner.block_hash;

    // EIP-4844 blobs are not supported and not needed for our use case.
    //
    // Rationale:
    // - Blobs are not required for private or public testnet deployments.
    //   Integration teams don't use them; blobs are typically used by L2s.
    // - Proper blob support requires propagating the actual blob data via
    //   consensus layer gossip mechanisms, which our current malachite-app
    //   implementation does not handle.
    // - Managing blob hashes alone without blob propagation is insufficient
    //   and would be an incomplete implementation.
    // - If blob support becomes necessary in the future, it will require
    //   a complete design including blob propagation mechanisms.
    let versioned_hashes = Vec::new();
    let _guard = metrics.start_engine_api_timer("notify_new_block");

    match engine
        .notify_new_block(execution_payload, versioned_hashes, deadline)
        .await
    {
        Ok(status) => match status.status {
            PayloadStatusEnum::Valid => Ok(PayloadValidationResult::Valid),
            PayloadStatusEnum::Invalid { validation_error } => {
                Ok(PayloadValidationResult::Invalid {
                    reason: validation_error,
                })
            }
            // The remaining cases are SYNCING and ACCEPTED:
            // - SYNCING: we don't expect this in ARC because the CL and EL are kept
            //   in sync. As a result, the EL should always have the information it
            //   needs to validate a payload.
            // - ACCEPTED: we don't expect to have side chains in ARC, so this status
            //   should never be returned.
            _ => {
                let height = execution_payload.payload_inner.payload_inner.block_number;
                warn!(
                    %block_hash,
                    %height,
                    "Unexpected payload status: {status:?}",
                );
                // SYNCING/ACCEPTED = the EL is BEHIND, not wrong: transient.
                // Handlers answer it with "no verdict" (skip the round /
                // re-request) — never with process death.
                Err(TransientDependencyError::new(
                    "execution engine",
                    format!(
                        "unexpected {status:?} status from engine for block {block_hash} at height {height}"
                    ),
                )
                .into())
            }
        },
        Err(e) => {
            if let Ok(engine_api_error) = EngineApiRpcError::try_from(&e) {
                if !engine_api_error.is_internal_error() {
                    error!(
                        %block_hash,
                        "Invalid payload: {engine_api_error}",
                    );
                    return Ok(PayloadValidationResult::Invalid {
                        reason: engine_api_error.to_string(),
                    });
                }
            }

            // Internal failures provide no deterministic payload verdict: the
            // client is AWAY (down / restarting), not wrong — transient. The
            // cause text is kept in the detail.
            let msg = format!(
                "call to EngineAPI::new_payload failed when validating block: {block_hash}",
            );
            Err(e.wrap_err(TransientDependencyError::new("execution engine", msg)))
        }
    }
}

/// Wall-clock ceiling for the WHOLE validation-time catch-up (all peers, all
/// blocks). This runs inside the vote window: the shim's own transport retry
/// is ~15 s per call, and an unbounded loop of those misses the round
/// entirely. 5 s is under the 500 ms-pacer propose window's slack and still
/// buys dozens of peer blocks on a healthy fleet.
const CATCHUP_BUDGET: Duration = Duration::from_secs(5);

/// Floor on one peer's slice of the remaining budget, so that with many peers
/// configured each still gets a usable window (a slice below this is worth
/// less than the round trip it has to pay for). Always capped by what is
/// actually left, so the total never exceeds [`CATCHUP_BUDGET`].
const MIN_PEER_SLICE: Duration = Duration::from_millis(250);

/// Ceiling on the lag a validation-time catch-up will even ATTEMPT, in lean
/// blocks.
///
/// The budget above is 5 s for the whole catch-up, and every block inside it
/// costs a peer round trip plus a local append — a handful of blocks on a
/// healthy fleet, dozens at best. 1024 blocks is ~8.5 minutes of chain at the
/// 2 blk/s product target: three orders of magnitude past anything the budget
/// could close, so a proposal that far ahead is either value-sync's job (we
/// are genuinely far behind and consensus will sync us) or a proposer naming
/// an absurd number. Either way, attempting it only burns the whole vote
/// window before abstaining anyway.
///
/// The verdict stays an abstain rather than an `Invalid`: being this far
/// behind is still a property of THIS node, and an Invalid on a value that
/// later gets certified sticks forever (valid-round re-proposal skips
/// re-validation). A byzantine proposer gains one wasted round, not a halt —
/// and now pays nothing for it, since no budget is spent.
const MAX_CATCHUP_LAG: u64 = 1024;

/// Outcome of the lean lane's TIP rules (catch-up + parent linkage).
///
/// The split between the two failure arms is a safety rule, not a style
/// choice: an `Invalid` recorded against a value that later gets certified
/// sticks forever (the valid-round rule re-proposes certified values WITHOUT
/// re-validation), so only an observed protocol violation may produce one. Us
/// being BEHIND — a slow peer, a big backlog, the budget running out — is a
/// property of this node, not of the proposal, and must yield no verdict at
/// all for the round (docs/lean-lane-integration.md §3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LeanTipVerdict {
    /// The proposal's lean block links onto our head: lean section is valid.
    Linked,
    /// Observed protocol violation — `Invalid`, with the forensic reason.
    Violation(String),
    /// We could not get into a position to judge (still behind). No verdict.
    /// `reason` is the machine-readable half: it becomes the metric label, so
    /// an abstaining validator is visible in one scrape (CLAUDE.md §6).
    NoVerdict {
        reason: LeanNoVerdictReason,
        detail: String,
    },
}

/// "No verdict from the lean lane this round" — an abstain, carrying WHY in a
/// form the handlers can label a counter with instead of parsing text.
///
/// It is transient by construction: either the dependency error that caused it
/// sits underneath, or a [`TransientDependencyError`] marker does, so
/// [`is_transient`] keeps answering true through every `wrap_err` the handlers
/// add on the way up.
#[derive(Debug)]
pub struct LeanNoVerdict {
    pub reason: LeanNoVerdictReason,
    detail: String,
    /// The transient marker, when the dependency that caused this was AWAY.
    /// `None` when the cause was a real (non-transient) failure, so this
    /// wrapper never launders one into "just wait for the node".
    source: Option<TransientDependencyError>,
}

impl std::fmt::Display for LeanNoVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for LeanNoVerdict {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// "No verdict this round" as a transient error, so the handlers skip /
/// re-request instead of dying (see [`is_transient`]).
fn lean_no_verdict(reason: LeanNoVerdictReason, detail: String) -> eyre::Report {
    eyre::Report::new(LeanNoVerdict {
        reason,
        detail,
        source: Some(TransientDependencyError::new(
            "lean lane node",
            format!("lean lane: no verdict ({reason})"),
        )),
    })
}

/// Same, but over a dependency error that already explains itself.
///
/// The cause is FLATTENED into the message rather than kept as the source: a
/// boxed `eyre::Report` is opaque to [`is_transient`]'s downcast, so keeping
/// it would silently lose the transient marker (and with it the "skip the
/// round" behaviour). The classification is re-derived from the cause here
/// instead, so a genuinely non-transient failure stays non-transient.
fn lean_no_verdict_over(
    reason: LeanNoVerdictReason,
    detail: impl std::fmt::Display,
    cause: eyre::Report,
) -> eyre::Report {
    let detail = format!("{detail}: {cause:#}");
    if is_transient(&cause) {
        return lean_no_verdict(reason, detail);
    }
    eyre::Report::new(LeanNoVerdict {
        reason,
        detail,
        source: None,
    })
}

/// The lean lane's reason for abstaining, if this report is one — at the root
/// or anywhere under the `wrap_err` layers the handlers add.
pub fn lean_no_verdict_reason(report: &eyre::Report) -> Option<LeanNoVerdictReason> {
    report
        .downcast_ref::<LeanNoVerdict>()
        .map(|e| e.reason)
        .or_else(|| {
            report
                .chain()
                .find_map(|e| e.downcast_ref::<LeanNoVerdict>().map(|e| e.reason))
        })
}

/// Height of the last lean abstain we logged, so a validator that abstains on
/// every round of a stuck height says so once rather than per round. `u64::MAX`
/// is the "nothing logged yet" sentinel (no real height reaches it).
static LAST_LEAN_ABSTAIN_WARN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Count (and, once per height, log) an abstain caused by the LEAN lane.
///
/// Called by every handler that turns a no-verdict into "no vote": the vote
/// path, the round-start paths and value-sync. A no-op for any other error, so
/// callers can hand it every validation failure they see. Without this, a
/// validator whose lean node is behind abstains silently — containers up,
/// agreement passing, no counter moving (CLAUDE.md §6).
pub fn note_lean_abstain(metrics: &AppMetrics, height: Height, err: &eyre::Report) {
    let Some(reason) = lean_no_verdict_reason(err) else {
        return;
    };
    metrics.inc_lean_no_verdict(reason);
    let height_u64 = height.as_u64();
    if LAST_LEAN_ABSTAIN_WARN.swap(height_u64, std::sync::atomic::Ordering::Relaxed) != height_u64 {
        warn!(
            %height, %reason,
            "🪶 lean lane: no verdict — this node is abstaining for this height: {err:#}"
        );
    }
}

/// Pull the lean blocks between our head and `target` from the peer lean
/// nodes, feeding each to the local node, until we sit one block below
/// `target` or the budget runs out. Returns wherever our head ended up.
///
/// Peer blocks are safe to ingest pre-verdict: the local node recomputes
/// every commitment on ingest, and these are exactly the bytes value-sync
/// would feed anyway.
async fn catch_up_lean_head(
    node: &impl LeanCatchup,
    mut head: LeanHead,
    target: u64,
    budget: Duration,
) -> (LeanHead, Option<LeanNoVerdictReason>) {
    // `tokio::time::Instant` so the budget arithmetic reads the same clock as
    // the timeouts below — identical to `std::time::Instant` in production,
    // and advancing in virtual time under `#[tokio::test(start_paused)]`.
    let start = tokio::time::Instant::now();
    let remaining = || budget.checked_sub(start.elapsed()).filter(|d| !d.is_zero());
    let peers = node.peer_count();
    // Peers that already timed out or errored once. They would do it again on
    // every remaining block, and each repeat costs another slice — over a
    // multi-block catch-up that is the whole budget spent on one dead peer.
    let mut dead = vec![false; peers];
    let mut fed = 0u64;
    // Why we stopped short, if we did — the label on the abstain that follows.
    let mut stalled: Option<LeanNoVerdictReason> = None;
    // Our OWN node failed a feed or a head read: that is a local outage, not a
    // peer one, and the two want different operators looking at them.
    let mut local_failed = false;

    'catchup: while target > head.number.saturating_add(1) {
        let next = head.number.saturating_add(1);
        let mut advanced = false;
        for peer in 0..peers {
            if dead[peer] {
                continue;
            }
            let Some(left) = remaining() else {
                stalled = Some(LeanNoVerdictReason::BudgetExhausted);
                break 'catchup;
            };
            // PER-PEER SLICE. One unresponsive peer must not be able to spend
            // the whole budget: it gets an even share of what is left, so the
            // peers behind it still get their turn. (With the whole budget per
            // peer, one slow peer ahead of a healthy one meant no catch-up at
            // all — and, before the verdict split below, a persisted Invalid
            // vote on an honest block.)
            let alive_left = dead[peer..].iter().filter(|d| !**d).count();
            let alive_left = u32::try_from(alive_left).unwrap_or(u32::MAX);
            // `checked_div` rather than `/`: a zero divisor is unreachable
            // (this peer is alive) but must not be a panic path.
            let slice = left
                .checked_div(alive_left)
                .unwrap_or(left)
                .max(MIN_PEER_SLICE)
                .min(left);
            let bytes = match tokio::time::timeout(slice, node.peer_block_bytes(peer, next)).await {
                Ok(Ok(Some(bytes))) => bytes,
                // A peer that simply does not have this block yet is healthy
                // and cheap: ask it again for the next one.
                Ok(Ok(None)) => continue,
                // Timed out or errored: stop spending slices on it.
                _ => {
                    dead[peer] = true;
                    continue;
                }
            };
            // Feeding our OWN node is not the slow peer's fault, so it is
            // bounded by the whole remaining budget rather than a slice.
            let Some(left) = remaining() else {
                stalled = Some(LeanNoVerdictReason::BudgetExhausted);
                break 'catchup;
            };
            if matches!(
                tokio::time::timeout(left, node.feed_local(bytes)).await,
                Ok(Ok(arc_eth_engine::lean_shim::NewBlockStatus::Valid(_)))
            ) {
                advanced = true;
                fed = fed.saturating_add(1);
                break;
            }
            // Bytes in hand that our own node would not take (down, or still
            // SYNCING itself): remember it, so if nothing advances the abstain
            // points at us rather than at the peers.
            local_failed = true;
        }
        if !advanced {
            stalled = Some(if local_failed {
                LeanNoVerdictReason::LocalUnreachable
            } else if peers == 0 {
                LeanNoVerdictReason::NoPeers
            } else {
                LeanNoVerdictReason::PeersTimedOut
            });
            break 'catchup;
        }
        let Some(left) = remaining() else {
            stalled = Some(LeanNoVerdictReason::BudgetExhausted);
            break 'catchup;
        };
        match tokio::time::timeout(left, node.local_head()).await {
            Ok(Ok(h)) => head = h,
            _ => {
                stalled = Some(LeanNoVerdictReason::LocalUnreachable);
                break 'catchup;
            }
        }
    }

    if fed > 0 {
        tracing::info!(
            "🪶 lean lane: validation-time catch-up fed {fed} blocks \
             from peers in {:?} (local head now {})",
            start.elapsed(),
            head.number
        );
    }
    (head, stalled)
}

/// The lean lane's tip rules: catch our node up to the proposal if we are
/// behind, then judge the parent linkage against our head.
pub(crate) async fn validate_lean_tip(
    node: &impl LeanCatchup,
    head: LeanHead,
    number: u64,
    parent: BlockHash,
    budget: Duration,
) -> LeanTipVerdict {
    // ABSURD LAG FIRST, before any budget is spent: a number this far ahead
    // cannot be reached inside the vote window no matter which peer answers.
    if number.saturating_sub(head.number) > MAX_CATCHUP_LAG {
        return LeanTipVerdict::NoVerdict {
            reason: LeanNoVerdictReason::GapTooLarge,
            detail: format!(
                "lean lane: proposal is {} blocks ahead of our head (block number {number}, \
                 local head {}) — past the {MAX_CATCHUP_LAG}-block catch-up ceiling, no \
                 verdict this round",
                number.saturating_sub(head.number),
                head.number
            ),
        };
    }

    let (head, stalled) = if number > head.number.saturating_add(1) {
        catch_up_lean_head(node, head, number, budget).await
    } else {
        (head, None)
    };

    // STILL BEHIND: the budget ran out, no peer could serve the next block, or
    // our node stopped answering. Nothing about the PROPOSAL was observed to
    // be wrong — we simply never got into a position to check it. No verdict.
    if number > head.number.saturating_add(1) {
        return LeanTipVerdict::NoVerdict {
            // A catch-up that ran at all says why it stopped; one that never
            // ran (number was already within reach, but our head moved back?)
            // cannot, so name the peers as the generic case.
            reason: stalled.unwrap_or(LeanNoVerdictReason::PeersTimedOut),
            detail: format!(
                "lean lane: still behind after catch-up (block number {number}, local head \
                 {}) — no verdict this round",
                head.number
            ),
        };
    }
    // Our head ran PAST the proposal while we were catching up (gossip). The
    // historic rule (byte-equality against our canonical chain) is the right
    // test now, and it ran against a stale head — re-judge next round rather
    // than fail the tip rule that no longer applies.
    if number <= head.number {
        return LeanTipVerdict::NoVerdict {
            reason: LeanNoVerdictReason::HeadRanPast,
            detail: format!(
                "lean lane: local head advanced past the proposal during catch-up (block \
                 number {number}, local head {}) — no verdict this round",
                head.number
            ),
        };
    }
    // We are exactly one below the proposal, so its parent MUST be our head:
    // a mismatch is an observed violation (and letting it get certified would
    // fail the decide anchor network-wide = a halt).
    if parent != head.commitment {
        return LeanTipVerdict::Violation(format!(
            "lean lane: parent/number mismatch (block parent {parent} number {number} vs \
             head {} number {})",
            head.commitment, head.number
        ));
    }
    LeanTipVerdict::Linked
}

/// Validates a consensus block's payload and stores it in the database
/// if the engine rejects it.
///
/// This is the higher-level entry point for callers that have a
/// [`ConsensusBlock`] and an [`InvalidPayloadsRepository`]. It delegates
/// to [`PayloadValidator::validate_payload`] for the actual engine call
/// and then persists an [`InvalidPayload`] record when the verdict is
/// `Invalid`.
///
/// # Return contract
///
/// - `Ok(Validity::Valid)`: the engine accepted the payload.
/// - `Ok(Validity::Invalid)`: the engine rejected the payload. Persisting
///   the forensic [`InvalidPayload`] record is **best-effort**: a failure
///   to append is logged at `error` but does not change the verdict
///   returned to the caller. The engine's verdict is authoritative and
///   must reach the consensus layer so the corresponding undecided block
///   is marked `Invalid` rather than left with a placeholder `Valid`.
/// - `Err(_)`: no verdict was obtained (engine transport error,
///   `SYNCING`/`ACCEPTED` status, etc.).
pub async fn validate_consensus_block(
    payload_validator: &impl PayloadValidator,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    lean_resolver: Option<&impl LeanBytesResolver>,
    lean_bytes_required: bool,
    block: &ConsensusBlock,
    store: &impl InvalidPayloadsRepository,
    metrics: &AppMetrics,
) -> eyre::Result<Validity> {
    // EVM lane (validated via the mockable validator).
    let result = payload_validator
        .validate_payload(&block.execution_payload)
        .await?;

    crate::height_timing::mark_at(
        block.height.as_u64(),
        crate::height_timing::Phase::EvmNewPayload,
    );

    if let PayloadValidationResult::Invalid { reason } = result {
        record_invalid_payload(block, &reason, store, metrics).await;
        return Ok(Validity::Invalid);
    }

    // LEAN payment lane: validation is STRUCTURAL + linkage only. The lean
    // node has no forkchoice — arc_newBlock appends PERMANENTLY — so undecided
    // blocks are never fed to it; execution happens once, inline, at the
    // decide anchor (affordable: ~us/output on the flat map). Safety comes
    // from total STF (invalid tx = no-op, a byzantine proposer can never
    // halt the lane) + the certificate binding the recomputed commitment.
    //
    // A block that arrived from the network (`lean_bytes_required`) whose
    // header commits to a lean block but carries NO lean bytes is the halt
    // case: the EVM lane alone would vote it Valid, and the decide anchor
    // would then wait 30s for bytes nobody has, fail the height, restart it,
    // and fail again — forever. It is Valid only if THIS node can produce
    // those bytes (staged, queued, canonical, or peer-fetched by the node),
    // in which case validation continues against them exactly as if they had
    // been framed. Self-authored rows re-validated from the local store
    // (`lean_bytes_required == false`, spec §5.5) keep the EVM-only reading:
    // their bytes live in the lean node, and decide needs none from the CL.
    let mut resolved_lane: Option<LeanLanePayload> = None;
    if block.lean_payload.is_none() && lean_bytes_required {
        if let (Some(commitment), Some(resolver)) = (block.header_lean_commitment(), lean_resolver)
        {
            match resolver.lean_bytes_by_commitment(commitment).await {
                Ok(Some(bytes)) => match LeanLanePayload::new(bytes) {
                    Ok(lane) if lane.commitment() == commitment => resolved_lane = Some(lane),
                    // The node answered with bytes that are not the block the
                    // header names (or do not decode). Never trust the claim:
                    // treat it as "cannot produce" — Invalid, not a crash.
                    _ => {
                        record_invalid_payload(
                            block,
                            &format!(
                                "lean lane: header commits to unknown lean block {commitment}"
                            ),
                            store,
                            metrics,
                        )
                        .await;
                        return Ok(Validity::Invalid);
                    }
                },
                Ok(None) => {
                    record_invalid_payload(
                        block,
                        &format!("lean lane: header commits to unknown lean block {commitment}"),
                        store,
                        metrics,
                    )
                    .await;
                    return Ok(Validity::Invalid);
                }
                // The node is AWAY, not answering "no": transient, no verdict.
                Err(e) => {
                    return Err(lean_no_verdict_over(
                        LeanNoVerdictReason::LocalUnreachable,
                        "lean lane: node unreachable while resolving the header commitment",
                        e,
                    ));
                }
            }
        }
    }

    if let Some(lane) = block.lean_payload.as_ref().or(resolved_lane.as_ref()) {
        crate::height_timing::set_lean_txs_at(block.height.as_u64(), lane.decoded.tx_count);

        // Header/lean binding: the EVM header's `prev_randao` must carry the
        // recomputed lean commitment (Task 5). This runs before every other
        // lean check and before any shim call — a mismatched header is a
        // structural forgery regardless of what the lean bytes decode to.
        // (Trivially true for `resolved_lane`, which was fetched BY that
        // commitment and re-checked against it above.)
        if !block.lean_binding_ok() || block.header_lean_commitment() != Some(lane.commitment()) {
            record_invalid_payload(
                block,
                &format!(
                    "lean lane: header/lean mismatch (prev_randao {:?} vs recomputed {})",
                    block.header_lean_commitment(),
                    lane.commitment()
                ),
                store,
                metrics,
            )
            .await;
            return Ok(Validity::Invalid);
        }
        let evm = &block.execution_payload.payload_inner.payload_inner;
        // Timestamp lockstep with the EVM lane. Numbers are NOT coupled to EVM
        // numbers: the lane may activate mid-chain, so lean numbers advance
        // 1-per-height from activation (sync serving maps by constant offset).
        if lane.decoded.timestamp_ms != evm.timestamp.saturating_mul(1000) {
            record_invalid_payload(
                block,
                &format!(
                    "lean lane: timestamp lockstep violation (lean ts_ms {} vs evm ts {})",
                    lane.decoded.timestamp_ms, evm.timestamp
                ),
                store,
                metrics,
            )
            .await;
            return Ok(Validity::Invalid);
        }
        // Parent linkage vs OUR lean head: prevents a byzantine proposer from
        // getting a wrong-parent block certified (which would make the decide
        // anchor fail network-wide = a halt).
        //
        // If WE are behind (block.number > head.number + 1), catch up from
        // peer lean nodes HERE, before the verdict. Waiting for value-sync
        // deadlocks: a lagging validator can't vote, so consensus can't
        // decide, so the decide-time catch-up never fires (measured live
        // 2026-08-22: 2/4 validators one lean block behind = permanent
        // 40/80-nil stall). Peer blocks are safe to ingest pre-verdict — the
        // local node recomputes every commitment on ingest, and appended
        // certified-chain blocks are exactly what sync would feed anyway.
        if let Some(shim) = lean_shim {
            match shim.get_head().await {
                Ok(head) => {
                    // ALREADY-CANONICAL (sync replay of a historic height):
                    // when consensus lags the lean chain (the node kept up via
                    // gossip while this validator's consensus fell behind),
                    // the synced value's lean block is in our PAST. Tip
                    // linkage (number == head+1) is the wrong test there —
                    // it rejected every such frame, wedging value-sync
                    // (measured 2026-08-22: val2 consensus at 15570, lean
                    // head 15523, lean number 15519 → invalid → sync dead).
                    // Valid iff it IS our canonical block at that number.
                    if lane.decoded.number <= head.number {
                        match shim.get_block_bytes(lane.decoded.number).await {
                            Ok(Some(ours)) if ours == lane.bytes => {
                                // Canonical replay — lean lane section valid;
                                // skip tip-linkage checks entirely.
                            }
                            Ok(_) => {
                                record_invalid_payload(
                                    block,
                                    &format!(
                                        "lean lane: historic block {} conflicts with our canonical chain",
                                        lane.decoded.number
                                    ),
                                    store,
                                    metrics,
                                )
                                .await;
                                return Ok(Validity::Invalid);
                            }
                            Err(e) => {
                                return Err(lean_no_verdict_over(
                                    LeanNoVerdictReason::LocalUnreachable,
                                    "lean lane: node unreachable during historic validation",
                                    e,
                                ));
                            }
                        }
                        return Ok(Validity::Valid);
                    }
                    // Tip rules (catch-up + linkage). The verdict split is
                    // the whole point: a LAG (we are behind, the budget ran
                    // out) must never become an Invalid — that sticks to a
                    // certified value forever under the valid-round rule —
                    // while a real linkage VIOLATION still must.
                    match validate_lean_tip(
                        shim,
                        head,
                        lane.decoded.number,
                        lane.decoded.parent,
                        CATCHUP_BUDGET,
                    )
                    .await
                    {
                        LeanTipVerdict::Linked => {}
                        LeanTipVerdict::Violation(reason) => {
                            record_invalid_payload(block, &reason, store, metrics).await;
                            return Ok(Validity::Invalid);
                        }
                        LeanTipVerdict::NoVerdict { reason, detail } => {
                            return Err(lean_no_verdict(reason, detail));
                        }
                    }
                    // Vote-gap execution (shim v1.3): stage the block now so
                    // the decide anchor promotes instead of executing on the
                    // critical path. Fire-and-forget — staging is speculative;
                    // failure (older node, races) just means the anchor takes
                    // the full path. Never blocks the vote.
                    let stage_shim = shim.clone();
                    let stage_bytes = lane.bytes.clone();
                    tokio::spawn(async move {
                        let _ = stage_shim.stage_block(&stage_bytes).await;
                    });

                    // Everything the vote waits on for the lean lane is done
                    // here: the head fetch, any catch-up feed and the linkage
                    // check. Staging itself is deliberately off the path.
                    crate::height_timing::mark_at(
                        block.height.as_u64(),
                        crate::height_timing::Phase::LeanStage,
                    );
                }
                Err(e) => {
                    // Unreachable past the shim's ~15s transport retry: the
                    // node is genuinely down. Return Err (no verdict) rather
                    // than Invalid — a recorded Invalid sticks to the value,
                    // and the valid-round rule re-proposes certified values
                    // WITHOUT re-validation, so a transient outage would wedge
                    // the height permanently (measured live 2026-08-22: all-4
                    // rolling restart => 0-precommit deadlock at height 3446).
                    return Err(lean_no_verdict_over(
                        LeanNoVerdictReason::LocalUnreachable,
                        "lean lane: node unreachable during validation",
                        e,
                    ));
                }
            }
        }
    }

    Ok(Validity::Valid)
}

/// Best-effort persistence of an invalid-payload forensic record. Logs on failure
/// but never changes the verdict (the engine's verdict is authoritative).
async fn record_invalid_payload(
    block: &ConsensusBlock,
    reason: &str,
    store: &impl InvalidPayloadsRepository,
    metrics: &AppMetrics,
) {
    warn!(
        height = %block.height,
        round = %block.round,
        block_hash = %block.self_reported_block_hash(),
        proposer = %block.proposer,
        reason = %reason,
        "Engine rejected payload, storing for forensics",
    );
    metrics.inc_invalid_payloads_count(InvalidPayloadSource::EngineReject);
    let invalid = InvalidPayload::new_from_block(block, reason);
    if let Err(e) = store.append(invalid).await {
        error!(
            height = %block.height,
            round = %block.round,
            block_hash = %block.self_reported_block_hash(),
            proposer = %block.proposer,
            "Failed to persist invalid-payload forensic record: {e}",
        );
    }
}

/// An execution payload that does not belong at its place in the chain.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PayloadBindingError {
    #[error("payload block number {actual} does not match consensus height {expected}")]
    HeightMismatch { expected: u64, actual: u64 },

    #[error(
        "payload parent hash {actual} is not the block finalized at the previous height ({expected})"
    )]
    ParentMismatch {
        expected: BlockHash,
        actual: BlockHash,
    },
}

impl PayloadBindingError {
    /// The counter label for this rule. A wrong block number is a property of the
    /// payload alone. A wrong parent compares it against local state, so that label
    /// also rises when this node holds the wrong view of the previous height.
    pub fn invalid_payload_source(&self) -> InvalidPayloadSource {
        match self {
            Self::HeightMismatch { .. } => InvalidPayloadSource::PayloadHeight,
            Self::ParentMismatch { .. } => InvalidPayloadSource::PayloadParent,
        }
    }
}

/// Makes sure that an execution payload belongs at the given consensus height.
///
/// Arc keeps one execution block per consensus height. A payload therefore
/// carries that height as its block number, and it extends the block that the
/// node finalized at the height before.
///
/// The parent rule needs the immediate predecessor. During batch value sync,
/// `previous_block` can lag by several heights. This function therefore applies
/// the parent rule only when `previous_block` sits one height below `height`.
pub fn check_payload_binding(
    payload: &ExecutionPayloadV3,
    height: Height,
    previous_block: Option<&ExecutionBlock>,
) -> Result<(), PayloadBindingError> {
    let payload = &payload.payload_inner.payload_inner;
    let height = height.as_u64();

    if payload.block_number != height {
        return Err(PayloadBindingError::HeightMismatch {
            expected: height,
            actual: payload.block_number,
        });
    }

    let Some(previous) = previous_block else {
        return Ok(());
    };

    if previous.block_number.checked_add(1) != Some(height) {
        return Ok(());
    }

    if payload.parent_hash != previous.block_hash {
        return Err(PayloadBindingError::ParentMismatch {
            expected: previous.block_hash,
            actual: payload.parent_hash,
        });
    }

    Ok(())
}

/// A validity verdict together with the rule that produced it.
///
/// A caller that reconciles a fresh verdict with a stored one needs to know
/// which rule spoke. Only an engine verdict can differ between two runs against
/// the same parent state, so only an engine verdict describes replay divergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockVerdict {
    /// The binding rules rejected the payload, and this is the rule that did. The
    /// engine was not asked. The rule reaches the caller because the two do not
    /// read alike: one blames the payload, the other can blame this node.
    Unbound(PayloadBindingError),
    /// The engine returned this verdict.
    Engine(Validity),
}

impl BlockVerdict {
    pub fn validity(&self) -> Validity {
        match self {
            Self::Unbound(_) => Validity::Invalid,
            Self::Engine(validity) => *validity,
        }
    }
}

/// Establishes the validity of a block that arrived from the network.
///
/// The binding rules run first. The engine never sees a payload that breaks
/// them, so such a payload never enters the block tree of the execution client.
/// A payload that keeps the rules gets its verdict from [`validate_consensus_block`].
// Two lanes, two repositories and an origin flag: every argument is a distinct
// collaborator, and bundling them into a struct would only move the list.
#[allow(clippy::too_many_arguments)]
pub async fn establish_block_validity(
    payload_validator: &impl PayloadValidator,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    lean_resolver: Option<&impl LeanBytesResolver>,
    lean_bytes_required: bool,
    block: &ConsensusBlock,
    previous_block: Option<&ExecutionBlock>,
    store: &impl InvalidPayloadsRepository,
    metrics: &AppMetrics,
) -> eyre::Result<BlockVerdict> {
    if let Err(error) =
        check_payload_binding(&block.execution_payload, block.height, previous_block)
    {
        warn!(
            height = %block.height,
            round = %block.round,
            block_hash = %block.self_reported_block_hash(),
            proposer = %block.proposer,
            reason = %error,
            "Payload is not bound to its place in the chain, storing for forensics",
        );

        metrics.inc_invalid_payloads_count(error.invalid_payload_source());

        persist_invalid_payload_best_effort(
            store,
            InvalidPayload::new_from_block(block, &error.to_string()),
            block.height,
            block.round,
            block.proposer,
        )
        .await;

        return Ok(BlockVerdict::Unbound(error));
    }

    validate_consensus_block(
        payload_validator,
        lean_shim,
        lean_resolver,
        lean_bytes_required,
        block,
        store,
        metrics,
    )
    .await
    .map(BlockVerdict::Engine)
}

/// Persists a forensic [`InvalidPayload`] record on a best-effort basis.
///
/// A persistence failure is logged at `error` and swallowed so it cannot abort the
/// caller's primary path. For call sites where the block was never assembled, so only
/// `height`, `round`, and `proposer` are available; [`validate_consensus_block`] logs
/// `block_hash` too because it has the assembled block.
pub(crate) async fn persist_invalid_payload_best_effort(
    store: &impl InvalidPayloadsRepository,
    invalid: InvalidPayload,
    height: Height,
    round: Round,
    proposer: Address,
) {
    if let Err(e) = store.append(invalid).await {
        error!(
            %height, %round, %proposer,
            "Failed to persist invalid-payload forensic record: {e}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use eyre::eyre;

    use malachitebft_app_channel::app::types::core::Validity;

    use alloy_primitives::{Address as AlloyAddress, Bloom, Bytes as AlloyBytes, U256};
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadStatus,
    };

    use arc_consensus_types::block::test_lean_block_bytes as lean_block_bytes;
    use arc_consensus_types::{Address, Height, Round, B256};
    use arc_eth_engine::engine::{MockEngineAPI, MockEthereumAPI};
    use arc_eth_engine::json_structures::ExecutionBlock;
    use arc_eth_engine::lean_shim::{MockLeanBuilder, MockLeanBytesResolver};

    /// Shorthand for "no lean lane" at a `generate_payload_with_retry` call
    /// site: the concrete builder type is otherwise unconstrained.
    type NoLean = Option<LeanBuild<'static, MockLeanBuilder>>;

    /// Shorthand for "no lean resolver": same reason, at the validation calls.
    const NO_RESOLVER: Option<&MockLeanBytesResolver> = None;

    /// A lean-mode block as it arrives when the proposer framed only the EVM
    /// lane: the header commits to `commitment`, no lean bytes are carried.
    fn block_with_header_commitment_only(commitment: B256) -> ConsensusBlock {
        let mut block = test_block();
        block
            .execution_payload
            .payload_inner
            .payload_inner
            .prev_randao = commitment;
        block
    }

    fn valid_validator() -> MockPayloadValidator {
        let mut validator = MockPayloadValidator::new();
        validator
            .expect_validate_payload()
            .returning(|_| Ok(PayloadValidationResult::Valid));
        validator
    }

    use crate::block::ConsensusBlock;
    use crate::metrics::app::AppMetrics;
    use crate::store::repositories::mocks::MockInvalidPayloadsRepository;
    use arc_consensus_db::invalid_payloads::InvalidPayload;

    fn test_payload(timestamp: u64) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: AlloyAddress::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp,
                    extra_data: AlloyBytes::default(),
                    base_fee_per_gas: U256::from(1u64),
                    block_hash: B256::ZERO,
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    #[tokio::test]
    async fn validate_payload_returns_valid_on_ok_status() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _, _| {
            Ok(PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            })
        });

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let result = validate_payload(&engine, &payload, &metrics, None)
            .await
            .expect("payload validation should succeed");

        assert_eq!(result, PayloadValidationResult::Valid);
    }

    #[tokio::test]
    async fn validate_payload_returns_invalid_on_invalid_status() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _, _| {
            Ok(PayloadStatus {
                status: PayloadStatusEnum::Invalid {
                    validation_error: "validation error".to_string(),
                },
                latest_valid_hash: None,
            })
        });

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let result = validate_payload(&engine, &payload, &metrics, None)
            .await
            .expect("payload validation should succeed");

        assert_eq!(
            result,
            PayloadValidationResult::Invalid {
                reason: "validation error".to_string(),
            },
        );
    }

    #[tokio::test]
    async fn validate_payload_returns_invalid_on_non_internal_rpc_error() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _, _| {
            let rpc_error = EngineApiRpcError::new(42, "engine API error", None);
            Err(eyre::Report::new(rpc_error))
        });

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let result = validate_payload(&engine, &payload, &metrics, None)
            .await
            .expect("should succeed without error");

        match &result {
            PayloadValidationResult::Invalid { reason } => {
                assert!(
                    reason.contains("engine API error"),
                    "reason should contain the RPC error message, got: {reason}",
                );
            }
            other => {
                panic!("expected Invalid, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn validate_payload_propagates_internal_rpc_error() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _, _| {
            let rpc_error = EngineApiRpcError::new(-32603, "Internal error", None);
            Err(eyre::Report::new(rpc_error))
        });

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let err = validate_payload(&engine, &payload, &metrics, None)
            .await
            .expect_err("internal RPC error should not produce a payload verdict");

        let engine_api_error = EngineApiRpcError::try_from(&err)
            .expect("error chain should preserve the Engine API error");
        assert!(engine_api_error.is_internal_error());
        assert!(
            err.to_string()
                .contains("call to EngineAPI::new_payload failed"),
            "error message should describe the failure, got: {err}",
        );
    }

    #[tokio::test]
    async fn validate_payload_propagates_other_errors() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload()
            .returning(|_, _, _, _| Err(eyre::eyre!("some error")));

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let err = validate_payload(&engine, &payload, &metrics, None)
            .await
            .expect_err("payload validation should return an error");

        let msg = err.to_string();
        assert!(
            msg.contains("call to EngineAPI::new_payload failed"),
            "error message should describe the failure, got: {msg}",
        );
        assert!(
            is_transient(&err),
            "an engine transport failure is the client being AWAY: transient"
        );
    }

    #[tokio::test]
    async fn validate_payload_returns_err_on_unexpected_status() {
        let test_cases = [PayloadStatusEnum::Syncing, PayloadStatusEnum::Accepted];

        for status in test_cases {
            let mut mock = MockEngineAPI::new();
            let status_for_mock = status.clone();
            mock.expect_new_payload().returning(move |_, _, _, _| {
                Ok(PayloadStatus {
                    status: status_for_mock.clone(),
                    latest_valid_hash: None,
                })
            });

            let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
            let payload = test_payload(0);
            let metrics = AppMetrics::default();

            let result = validate_payload(&engine, &payload, &metrics, None)
                .await
                .expect_err("payload validation should return an error");

            let got_msg = result.to_string();
            let want_status = PayloadStatus {
                status,
                latest_valid_hash: None,
            };
            let want_err_msg = format!(
                "unexpected {want_status:?} status from engine for block {} \
                 at height {}",
                payload.payload_inner.payload_inner.block_hash,
                payload.payload_inner.payload_inner.block_number,
            );
            assert_eq!(got_msg, want_err_msg);
        }
    }

    fn test_block() -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            execution_payload: test_payload(0),
            validity: Validity::Valid,
            signature: None,
            lean_payload: None,
        }
    }

    /// Payload that carries `height` as its block number and `parent_hash` as its parent.
    fn bound_payload(height: u64, parent_hash: B256) -> ExecutionPayloadV3 {
        let mut payload = test_payload(0);
        payload.payload_inner.payload_inner.block_number = height;
        payload.payload_inner.payload_inner.parent_hash = parent_hash;
        payload
    }

    fn prev_block(number: u64, block_hash: B256) -> ExecutionBlock {
        ExecutionBlock {
            block_hash,
            block_number: number,
            parent_hash: B256::ZERO,
            timestamp: 0,
        }
    }

    fn block_at(height: u64, payload: ExecutionPayloadV3) -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(height),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            execution_payload: payload,
            validity: Validity::Valid,
            signature: None,
            lean_payload: None,
        }
    }

    #[test]
    fn binding_accepts_payload_at_its_height_that_extends_the_previous_block() {
        let parent = B256::repeat_byte(0x11);
        let payload = bound_payload(11, parent);

        check_payload_binding(&payload, Height::new(11), Some(&prev_block(10, parent)))
            .expect("a payload at its height that extends the previous block is bound");
    }

    /// A fresh block forked from an older canonical ancestor: block number 5
    /// proposed at height 11.
    #[test]
    fn binding_rejects_block_number_below_the_consensus_height() {
        let payload = bound_payload(5, B256::repeat_byte(0x44));

        let error = check_payload_binding(&payload, Height::new(11), None)
            .expect_err("block number 5 does not belong at height 11");

        assert!(
            matches!(
                error,
                PayloadBindingError::HeightMismatch {
                    expected: 11,
                    actual: 5
                }
            ),
            "got {error:?}",
        );
    }

    /// The height rule holds even when the payload extends the previous block,
    /// which is what a payload replayed from an older height looks like.
    #[test]
    fn binding_rejects_block_number_above_the_consensus_height() {
        let parent = B256::repeat_byte(0x11);
        let payload = bound_payload(12, parent);

        let error = check_payload_binding(&payload, Height::new(11), Some(&prev_block(10, parent)))
            .expect_err("block number 12 does not belong at height 11");

        assert!(
            matches!(
                error,
                PayloadBindingError::HeightMismatch {
                    expected: 11,
                    actual: 12
                }
            ),
            "got {error:?}",
        );
    }

    #[test]
    fn binding_rejects_parent_that_is_not_the_previous_block() {
        let expected = B256::repeat_byte(0x11);
        let other = B256::repeat_byte(0x22);
        let payload = bound_payload(11, other);

        let error =
            check_payload_binding(&payload, Height::new(11), Some(&prev_block(10, expected)))
                .expect_err("a payload that extends another block is not bound");

        match error {
            PayloadBindingError::ParentMismatch {
                expected: e,
                actual: a,
            } => {
                assert_eq!(e, expected);
                assert_eq!(a, other);
            }
            other => panic!("got {other:?}"),
        }
    }

    /// Batch value sync leaves `previous_block` several heights behind, so it is
    /// not the parent of the payload under test and the parent rule cannot apply.
    #[test]
    fn binding_skips_the_parent_rule_when_the_previous_block_lags() {
        let payload = bound_payload(11, B256::repeat_byte(0x22));

        check_payload_binding(
            &payload,
            Height::new(11),
            Some(&prev_block(7, B256::repeat_byte(0x11))),
        )
        .expect("the parent rule does not apply to a previous block that lags");
    }

    #[test]
    fn binding_skips_the_parent_rule_without_a_previous_block() {
        let payload = bound_payload(11, B256::repeat_byte(0x22));

        check_payload_binding(&payload, Height::new(11), None)
            .expect("the parent rule needs a previous block");
    }

    #[tokio::test]
    async fn establish_block_validity_rejects_an_unbound_payload_without_asking_the_engine() {
        let mut validator = MockPayloadValidator::new();
        validator.expect_validate_payload().times(0);

        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .withf(|ip: &InvalidPayload| {
                ip.height == Height::new(11)
                    && ip.reason.contains("does not match consensus height")
            })
            .returning(|_| Ok(()));

        let metrics = AppMetrics::default();
        let block = block_at(11, bound_payload(5, B256::ZERO));

        let verdict = establish_block_validity(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            None,
            &store,
            &metrics,
        )
        .await
        .expect("a binding error is a verdict, not a failure");

        assert_eq!(
            verdict,
            BlockVerdict::Unbound(PayloadBindingError::HeightMismatch {
                expected: 11,
                actual: 5,
            }),
            "the caller must be able to tell which rule rejected the payload",
        );
        assert_eq!(verdict.validity(), Validity::Invalid);
        assert_eq!(metrics.get_invalid_payloads_count(), 1);
    }

    /// The parent rule reaches the caller as its own variant. A caller that answers
    /// the two rules differently reads this, and the two rules blame different
    /// parties.
    #[tokio::test]
    async fn establish_block_validity_reports_which_rule_rejected_the_payload() {
        let expected = B256::repeat_byte(0xAB);

        let mut validator = MockPayloadValidator::new();
        validator.expect_validate_payload().times(0);

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(1).returning(|_| Ok(()));

        let metrics = AppMetrics::default();
        let actual = B256::repeat_byte(0xCD);
        let block = block_at(11, bound_payload(11, actual));

        let verdict = establish_block_validity(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            Some(&prev_block(10, expected)),
            &store,
            &metrics,
        )
        .await
        .expect("a binding error is a verdict, not a failure");

        assert_eq!(
            verdict,
            BlockVerdict::Unbound(PayloadBindingError::ParentMismatch { expected, actual }),
        );
        assert_eq!(
            metrics.get_invalid_payloads_count_by_source(InvalidPayloadSource::PayloadParent),
            1,
        );
    }

    #[tokio::test]
    async fn establish_block_validity_asks_the_engine_about_a_bound_payload() {
        let parent = B256::repeat_byte(0x11);

        let mut validator = MockPayloadValidator::new();
        validator
            .expect_validate_payload()
            .times(1)
            .returning(|_| Ok(PayloadValidationResult::Valid));

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);

        let metrics = AppMetrics::default();
        let block = block_at(11, bound_payload(11, parent));

        let verdict = establish_block_validity(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            Some(&prev_block(10, parent)),
            &store,
            &metrics,
        )
        .await
        .expect("should succeed");

        assert_eq!(verdict, BlockVerdict::Engine(Validity::Valid));
        assert_eq!(metrics.get_invalid_payloads_count(), 0);
    }

    #[tokio::test]
    async fn validate_consensus_block_returns_valid() {
        let mut validator = MockPayloadValidator::new();
        validator
            .expect_validate_payload()
            .returning(|_| Ok(PayloadValidationResult::Valid));

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);

        let metrics = AppMetrics::default();
        let block = test_block();
        let result = validate_consensus_block(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("should succeed");

        assert_eq!(result, Validity::Valid);
        assert_eq!(metrics.get_invalid_payloads_count(), 0);
    }

    #[tokio::test]
    async fn validate_consensus_block_returns_invalid_and_stores() {
        let mut validator = MockPayloadValidator::new();
        validator.expect_validate_payload().returning(|_| {
            Ok(PayloadValidationResult::Invalid {
                reason: "bad block".into(),
            })
        });

        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .withf(|ip: &InvalidPayload| {
                ip.height == Height::new(1)
                    && ip.round == Round::new(0)
                    && ip.proposer_address == Address::new([0u8; 20])
                    && ip.reason == "bad block"
                    && ip.payload.is_some()
            })
            .returning(|_| Ok(()));

        let metrics = AppMetrics::default();
        let block = test_block();
        let result = validate_consensus_block(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("should succeed");

        assert_eq!(result, Validity::Invalid);
        assert_eq!(metrics.get_invalid_payloads_count(), 1);
    }

    #[tokio::test]
    async fn validate_consensus_block_propagates_validation_error() {
        let mut validator = MockPayloadValidator::new();
        validator
            .expect_validate_payload()
            .returning(|_| Err(eyre!("engine down")));

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);

        let metrics = AppMetrics::default();
        let block = test_block();
        let err = validate_consensus_block(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect_err("should propagate error");

        assert!(
            err.to_string().contains("engine down"),
            "error should contain the original message, \
             got: {err}",
        );
        assert_eq!(metrics.get_invalid_payloads_count(), 0);
    }

    #[tokio::test]
    async fn validate_consensus_block_keeps_transient_marker_through_propagation() {
        let mut validator = MockPayloadValidator::new();
        validator.expect_validate_payload().returning(|_| {
            Err(TransientDependencyError::new("execution engine", "unexpected SYNCING").into())
        });

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);

        let metrics = AppMetrics::default();
        let block = test_block();
        let err = validate_consensus_block(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect_err("a transient error must propagate as Err (no verdict), never as Invalid");

        assert!(
            is_transient(&err),
            "marker lost in validate_consensus_block: {err:#}"
        );
        // ...and through the wrap_err layer every handler adds on the way up.
        let wrapped = err.wrap_err("Payload validation failed on block built from synced value");
        assert!(
            is_transient(&wrapped),
            "marker lost under wrap_err: {wrapped:#}"
        );
        assert_eq!(metrics.get_invalid_payloads_count(), 0);
    }

    #[tokio::test]
    async fn validate_consensus_block_returns_invalid_when_forensics_persist_fails() {
        // When the engine returns Invalid but persisting the forensic record
        // fails (e.g. transient DB issue), the engine's verdict is still the
        // authoritative answer and must be returned. Otherwise the caller
        // (validate_undecided_blocks) treats it as "no verdict obtained" and
        // leaves the placeholder `Valid` in undecided_blocks, masking a
        // rejected block.
        let mut validator = MockPayloadValidator::new();
        validator.expect_validate_payload().returning(|_| {
            Ok(PayloadValidationResult::Invalid {
                reason: "bad".into(),
            })
        });

        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .returning(|_| Err(std::io::Error::other("disk full")));

        let metrics = AppMetrics::default();
        let block = test_block();
        let validity = validate_consensus_block(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("verdict should be returned even when forensics persist fails");

        assert_eq!(validity, Validity::Invalid);
        assert_eq!(metrics.get_invalid_payloads_count(), 1);
    }

    /// A header whose `prev_randao` disagrees with the carried lean payload's
    /// recomputed commitment must be rejected before any other lean check
    /// (timestamp lockstep, parent linkage) or shim call runs.
    #[tokio::test]
    async fn lean_header_mismatch_is_invalid_before_any_shim_call() {
        let mut validator = MockPayloadValidator::new();
        validator
            .expect_validate_payload()
            .returning(|_| Ok(PayloadValidationResult::Valid));
        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .withf(|ip| ip.reason.contains("header/lean mismatch"))
            .returning(|_| Ok(()));
        let metrics = AppMetrics::default();
        let mut block = test_block();
        let bytes = lean_block_bytes(
            B256::repeat_byte(1),
            5,
            block.execution_payload.timestamp() * 1000,
        );
        block.lean_payload = Some(LeanLanePayload::new(bytes).unwrap());
        block
            .execution_payload
            .payload_inner
            .payload_inner
            .prev_randao = B256::repeat_byte(0xee);
        let v = validate_consensus_block(
            &validator,
            None,
            NO_RESOLVER,
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .unwrap();
        assert_eq!(v, Validity::Invalid);
    }

    /// Critical: a block from the network whose header commits to a lean block
    /// but carries no lean bytes. If THIS node can produce those bytes, the
    /// block is bound and validation continues against them.
    #[tokio::test]
    async fn network_block_without_lean_bytes_is_valid_when_the_node_has_them() {
        let bytes = lean_block_bytes(B256::repeat_byte(1), 5, 0);
        let commitment = LeanLanePayload::new(bytes.clone()).unwrap().commitment();

        let mut resolver = MockLeanBytesResolver::new();
        let answer = bytes.clone();
        resolver
            .expect_lean_bytes_by_commitment()
            .withf(move |c| *c == commitment)
            .times(1)
            .returning(move |_| Ok(Some(answer.clone())));

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);
        let metrics = AppMetrics::default();
        let block = block_with_header_commitment_only(commitment);

        let v = validate_consensus_block(
            &valid_validator(),
            None,
            Some(&resolver),
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("a resolvable commitment is a verdict, not a failure");

        assert_eq!(v, Validity::Valid);
        assert_eq!(metrics.get_invalid_payloads_count(), 0);
    }

    /// The halt case: nobody local has the bytes. Voting Valid here certifies a
    /// block whose decide anchor can never complete — so it is Invalid.
    #[tokio::test]
    async fn network_block_without_lean_bytes_is_invalid_when_the_node_lacks_them() {
        let commitment = B256::repeat_byte(0x7a);

        let mut resolver = MockLeanBytesResolver::new();
        resolver
            .expect_lean_bytes_by_commitment()
            .times(1)
            .returning(|_| Ok(None));

        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .withf(move |ip: &InvalidPayload| {
                ip.reason == format!("lean lane: header commits to unknown lean block {commitment}")
            })
            .returning(|_| Ok(()));
        let metrics = AppMetrics::default();
        let block = block_with_header_commitment_only(commitment);

        let v = validate_consensus_block(
            &valid_validator(),
            None,
            Some(&resolver),
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("an unresolvable commitment is a verdict, not a failure");

        assert_eq!(v, Validity::Invalid);
        assert_eq!(metrics.get_invalid_payloads_count(), 1);
    }

    /// A node that answers with SOME block is not a node that answers with THE
    /// block: the commitment is recomputed from the bytes, never taken on
    /// trust, so a mismatched answer reads exactly like "I do not have it".
    #[tokio::test]
    async fn network_block_is_invalid_when_the_node_answers_other_bytes() {
        let commitment = LeanLanePayload::new(lean_block_bytes(B256::repeat_byte(1), 5, 0))
            .unwrap()
            .commitment();
        let other = lean_block_bytes(B256::repeat_byte(2), 9, 0);
        assert_ne!(
            LeanLanePayload::new(other.clone()).unwrap().commitment(),
            commitment
        );

        let mut resolver = MockLeanBytesResolver::new();
        resolver
            .expect_lean_bytes_by_commitment()
            .times(1)
            .returning(move |_| Ok(Some(other.clone())));

        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .withf(move |ip: &InvalidPayload| ip.reason.contains("unknown lean block"))
            .returning(|_| Ok(()));
        let metrics = AppMetrics::default();
        let block = block_with_header_commitment_only(commitment);

        let v = validate_consensus_block(
            &valid_validator(),
            None,
            Some(&resolver),
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("a wrong answer is a verdict, not a failure");

        assert_eq!(v, Validity::Invalid);
    }

    /// The same row re-validated from the LOCAL store after a restart (spec
    /// §5.5): it is this node's own earlier work, the store never held the lean
    /// bytes, and decide needs none from the CL. EVM lane only, no shim call.
    #[tokio::test]
    async fn store_loaded_block_without_lean_bytes_stays_valid_without_asking_the_node() {
        let commitment = B256::repeat_byte(0x7a);

        let mut resolver = MockLeanBytesResolver::new();
        resolver.expect_lean_bytes_by_commitment().times(0);

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);
        let metrics = AppMetrics::default();
        let block = block_with_header_commitment_only(commitment);

        let v = validate_consensus_block(
            &valid_validator(),
            None,
            Some(&resolver),
            false,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect("a store-loaded row keeps its EVM-only reading");

        assert_eq!(v, Validity::Valid);
    }

    /// The node being AWAY is not the node saying "no": no verdict at all, so
    /// the round is skipped instead of a permanent Invalid being recorded.
    #[tokio::test]
    async fn unreachable_node_yields_no_verdict_when_resolving_the_header_commitment() {
        let mut resolver = MockLeanBytesResolver::new();
        resolver.expect_lean_bytes_by_commitment().returning(|_| {
            Err(TransientDependencyError::new("lean lane node", "shim: request failed").into())
        });

        let mut store = MockInvalidPayloadsRepository::new();
        store.expect_append().times(0);
        let metrics = AppMetrics::default();
        let block = block_with_header_commitment_only(B256::repeat_byte(0x7a));

        let err = validate_consensus_block(
            &valid_validator(),
            None,
            Some(&resolver),
            true,
            &block,
            &store,
            &metrics,
        )
        .await
        .expect_err("an unreachable lean node must not become an Invalid verdict");

        assert!(is_transient(&err), "marker lost: {err:#}");
        assert_eq!(metrics.get_invalid_payloads_count(), 0);
    }

    // ---------------------------------------------------------------------
    // Validation-time catch-up: the budget slice and the lag/violation split.
    //
    // Virtual time (`start_paused`): the slow peers below sleep 30 s, which
    // the runtime skips to the earliest timeout deadline, so the budget rules
    // are exercised exactly and the tests run instantly.
    // ---------------------------------------------------------------------

    /// A lean lane under test: one local node (a head that the feed advances)
    /// plus peers with configurable latency.
    struct TestLane {
        /// Per peer: how long it takes to answer, and whether it has blocks.
        peers: Vec<(Duration, bool)>,
        /// The canonical chain the serving peers hand out, by number.
        chain: std::collections::HashMap<u64, Vec<u8>>,
        head: std::sync::Mutex<LeanHead>,
        /// Peers asked, in order — this is what pins the slice behaviour.
        asked: std::sync::Mutex<Vec<usize>>,
    }

    impl LeanCatchup for TestLane {
        fn peer_count(&self) -> usize {
            self.peers.len()
        }

        async fn peer_block_bytes(
            &self,
            peer: usize,
            number: u64,
        ) -> eyre::Result<Option<Vec<u8>>> {
            let (delay, serves) = self.peers[peer];
            self.asked.lock().expect("asked").push(peer);
            tokio::time::sleep(delay).await;
            Ok(serves.then(|| self.chain.get(&number).cloned()).flatten())
        }

        async fn feed_local(
            &self,
            bytes: Vec<u8>,
        ) -> eyre::Result<arc_eth_engine::lean_shim::NewBlockStatus> {
            let block = LeanLanePayload::new(bytes)?;
            *self.head.lock().expect("head") = LeanHead {
                commitment: block.commitment(),
                number: block.decoded.number,
                timestamp_ms: block.decoded.timestamp_ms,
            };
            Ok(arc_eth_engine::lean_shim::NewBlockStatus::Valid(
                block.commitment(),
            ))
        }

        async fn local_head(&self) -> eyre::Result<LeanHead> {
            Ok(*self.head.lock().expect("head"))
        }
    }

    fn lean_head(number: u64) -> LeanHead {
        LeanHead {
            commitment: B256::repeat_byte(0x11),
            number,
            timestamp_ms: number.saturating_mul(1000),
        }
    }

    /// Lean blocks `head.number + 1 ..= last`, linked onto `head`.
    fn lean_chain(head: LeanHead, last: u64) -> std::collections::HashMap<u64, Vec<u8>> {
        let mut parent = head.commitment;
        let mut chain = std::collections::HashMap::new();
        for n in head.number.saturating_add(1)..=last {
            let bytes = lean_block_bytes(parent, n, n.saturating_mul(1000));
            parent = LeanLanePayload::new(bytes.clone())
                .expect("test lean block decodes")
                .commitment();
            chain.insert(n, bytes);
        }
        chain
    }

    fn commitment_of(chain: &std::collections::HashMap<u64, Vec<u8>>, number: u64) -> B256 {
        LeanLanePayload::new(chain[&number].clone())
            .expect("test lean block decodes")
            .commitment()
    }

    fn test_lane(head: LeanHead, last: u64, peers: Vec<(Duration, bool)>) -> TestLane {
        TestLane {
            peers,
            chain: lean_chain(head, last),
            head: std::sync::Mutex::new(head),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The fix: each peer gets a SLICE of the remaining budget, so one peer
    /// that never answers cannot spend the whole of it — the healthy peer
    /// behind it still gets its turn and the catch-up succeeds.
    #[tokio::test(start_paused = true)]
    async fn a_slow_peer_costs_only_its_slice_so_a_fast_peer_still_catches_us_up() {
        let head = lean_head(10);
        let lane = test_lane(
            head,
            12,
            vec![(Duration::from_secs(30), true), (Duration::ZERO, true)],
        );
        let parent_of_12 = commitment_of(&lane.chain, 11);

        let verdict = validate_lean_tip(&lane, head, 12, parent_of_12, CATCHUP_BUDGET).await;

        assert_eq!(verdict, LeanTipVerdict::Linked);
        assert_eq!(
            lane.asked.lock().expect("asked").as_slice(),
            &[0, 1],
            "the slow peer must not eat the whole budget"
        );
        assert_eq!(lane.head.lock().expect("head").number, 11);
    }

    /// The safety rule: when the BUDGET (not the proposal) is what ran out, we
    /// are merely behind — no protocol violation was observed — so the round
    /// gets NO verdict. An `Invalid` here would stick to the value forever
    /// under the valid-round rule.
    #[tokio::test(start_paused = true)]
    async fn every_peer_slow_yields_no_verdict_never_invalid() {
        let head = lean_head(10);
        let lane = test_lane(
            head,
            12,
            vec![
                (Duration::from_secs(30), true),
                (Duration::from_secs(30), true),
            ],
        );
        let parent_of_12 = commitment_of(&lane.chain, 11);

        let started = tokio::time::Instant::now();
        let verdict = validate_lean_tip(&lane, head, 12, parent_of_12, CATCHUP_BUDGET).await;

        // The budget running out surfaces as the peer timeout that consumed
        // it — which is the more useful label of the two for an operator.
        assert!(
            matches!(
                verdict,
                LeanTipVerdict::NoVerdict {
                    reason: LeanNoVerdictReason::PeersTimedOut,
                    ..
                }
            ),
            "an exhausted catch-up budget is a lag, not a verdict: {verdict:?}"
        );
        assert_eq!(
            lane.asked.lock().expect("asked").as_slice(),
            &[0, 1],
            "both peers get a slice before the budget is gone"
        );
        assert!(
            started.elapsed() <= CATCHUP_BUDGET,
            "the whole catch-up must stay inside the budget, took {:?}",
            started.elapsed()
        );
    }

    /// A peer that does not answer costs ONE slice for the whole catch-up,
    /// not one per block — over a multi-block backlog that is the difference
    /// between catching up and spending the budget on a dead peer.
    #[tokio::test(start_paused = true)]
    async fn a_dead_peer_is_asked_once_across_a_multi_block_catch_up() {
        let head = lean_head(10);
        let lane = test_lane(
            head,
            14,
            vec![(Duration::from_secs(30), true), (Duration::ZERO, true)],
        );
        let parent_of_14 = commitment_of(&lane.chain, 13);

        let verdict = validate_lean_tip(&lane, head, 14, parent_of_14, CATCHUP_BUDGET).await;

        assert_eq!(verdict, LeanTipVerdict::Linked);
        let asked = lane.asked.lock().expect("asked").clone();
        assert_eq!(
            asked.iter().filter(|p| **p == 0).count(),
            1,
            "the unresponsive peer is dropped after its first timeout"
        );
        assert_eq!(
            asked.iter().filter(|p| **p == 1).count(),
            3,
            "blocks 11, 12 and 13 all come from the healthy peer"
        );
    }

    /// No peers configured at all is the same shape: we cannot get into a
    /// position to judge, so we do not judge.
    #[tokio::test(start_paused = true)]
    async fn being_behind_without_peers_yields_no_verdict() {
        let head = lean_head(10);
        let lane = test_lane(head, 12, vec![]);
        let parent_of_12 = commitment_of(&lane.chain, 11);

        let verdict = validate_lean_tip(&lane, head, 12, parent_of_12, CATCHUP_BUDGET).await;

        assert!(
            matches!(
                verdict,
                LeanTipVerdict::NoVerdict {
                    reason: LeanNoVerdictReason::NoPeers,
                    ..
                }
            ),
            "no peers to catch up from is a lag: {verdict:?}"
        );
    }

    /// A proposer naming an absurd lean number must not cost every validator
    /// the whole catch-up budget: the gap is rejected before a single peer is
    /// asked. Still an abstain, not an Invalid — being behind is our property,
    /// and an Invalid would stick to the value if it were later certified.
    #[tokio::test(start_paused = true)]
    async fn an_absurd_lag_abstains_without_spending_the_budget() {
        let head = lean_head(10);
        let lane = test_lane(head, 12, vec![(Duration::ZERO, true)]);
        let absurd = head.number + MAX_CATCHUP_LAG + 1;

        let started = tokio::time::Instant::now();
        let verdict =
            validate_lean_tip(&lane, head, absurd, B256::repeat_byte(0xbb), CATCHUP_BUDGET).await;

        assert!(
            matches!(
                verdict,
                LeanTipVerdict::NoVerdict {
                    reason: LeanNoVerdictReason::GapTooLarge,
                    ..
                }
            ),
            "an unreachable lag is a lag, not a violation: {verdict:?}"
        );
        assert!(
            lane.asked.lock().expect("asked").is_empty(),
            "no peer may be asked for a gap we cannot close"
        );
        assert_eq!(started.elapsed(), Duration::ZERO, "no budget may be spent");
    }

    /// The ceiling is a ceiling, not a cliff in front of ordinary lag: a
    /// backlog right at the limit is still caught up and judged.
    #[tokio::test(start_paused = true)]
    async fn a_lag_at_the_ceiling_is_still_caught_up() {
        let head = lean_head(10);
        let target = head.number + MAX_CATCHUP_LAG;
        let lane = test_lane(head, target, vec![(Duration::ZERO, true)]);
        let parent = commitment_of(&lane.chain, target - 1);

        let verdict = validate_lean_tip(&lane, head, target, parent, CATCHUP_BUDGET).await;

        assert_eq!(verdict, LeanTipVerdict::Linked);
    }

    /// The other half of the split: once the catch-up HAS put us one block
    /// below the proposal, a wrong parent is an observed violation and still
    /// votes the block down.
    #[tokio::test(start_paused = true)]
    async fn a_wrong_parent_after_a_successful_catch_up_is_still_invalid() {
        let head = lean_head(10);
        let lane = test_lane(head, 12, vec![(Duration::ZERO, true)]);

        let verdict =
            validate_lean_tip(&lane, head, 12, B256::repeat_byte(0xbb), CATCHUP_BUDGET).await;

        assert!(
            matches!(verdict, LeanTipVerdict::Violation(_)),
            "a parent that does not match the head we reached is a violation: {verdict:?}"
        );
        assert_eq!(
            lane.head.lock().expect("head").number,
            11,
            "the catch-up itself succeeded"
        );
    }

    /// "No verdict" must reach the handlers as a TRANSIENT error, so they skip
    /// the round instead of dying (and never record an Invalid).
    #[test]
    fn no_verdict_is_a_transient_error() {
        let err = lean_no_verdict(
            LeanNoVerdictReason::BudgetExhausted,
            "lean lane: still behind after catch-up".to_string(),
        );
        assert!(is_transient(&err), "marker lost: {err:#}");
    }

    /// The reason must survive the `wrap_err` layers the handlers add, or the
    /// abstain gets counted as "unlabelled" exactly when it matters.
    #[test]
    fn the_abstain_reason_survives_wrapping() {
        let err = lean_no_verdict(
            LeanNoVerdictReason::PeersTimedOut,
            "lean lane: still behind after catch-up".to_string(),
        )
        .wrap_err("Payload validation failed on block built after receiving proposal part");

        assert_eq!(
            lean_no_verdict_reason(&err),
            Some(LeanNoVerdictReason::PeersTimedOut)
        );
        assert!(is_transient(&err), "marker lost: {err:#}");
    }

    /// An error that has nothing to do with the lean lane must not be counted
    /// as a lean abstain — the counter is only useful if it is specific.
    #[test]
    fn a_plain_error_is_not_a_lean_abstain() {
        let metrics = AppMetrics::default();
        let err = eyre::eyre!("engine call failed");

        note_lean_abstain(&metrics, Height::new(7), &err);

        assert_eq!(
            LeanNoVerdictReason::ALL
                .iter()
                .map(|r| metrics.get_lean_no_verdict(*r))
                .sum::<u64>(),
            0
        );
    }

    /// The counter the health gate reads: every abstain lands on it, labelled.
    #[test]
    fn a_lean_abstain_is_counted_under_its_reason() {
        let metrics = AppMetrics::default();
        let err = lean_no_verdict(
            LeanNoVerdictReason::LocalUnreachable,
            "lean lane: node unreachable during validation".to_string(),
        )
        .wrap_err("Payload validation failed");

        note_lean_abstain(&metrics, Height::new(7), &err);
        note_lean_abstain(&metrics, Height::new(7), &err);

        assert_eq!(
            metrics.get_lean_no_verdict(LeanNoVerdictReason::LocalUnreachable),
            2,
            "every abstain counts, even when only the first one logs"
        );
        assert_eq!(
            metrics.get_lean_no_verdict(LeanNoVerdictReason::PeersTimedOut),
            0
        );
    }

    #[tokio::test]
    async fn persist_invalid_payload_best_effort_swallows_persistence_failure() {
        let mut store = MockInvalidPayloadsRepository::new();
        store
            .expect_append()
            .times(1)
            .returning(|_| Err(std::io::Error::other("disk full")));

        let height = Height::new(1);
        let round = Round::new(0);
        let proposer = Address::new([0u8; 20]);
        let invalid = InvalidPayload::new_without_payload(height, round, proposer, "bad");

        persist_invalid_payload_best_effort(&store, invalid, height, round, proposer).await;
    }

    #[derive(Clone, Debug)]
    enum Scenario {
        Success,
        UnknownPayloadUntil { succeed_on: usize },
        OtherError,
    }

    struct TestPayloadGenerator {
        scenario: Scenario,
        attempts: AtomicUsize,
    }

    impl TestPayloadGenerator {
        fn new(scenario: Scenario) -> Self {
            Self {
                scenario,
                attempts: AtomicUsize::new(0),
            }
        }

        fn dummy_payload(timestamp: u64) -> ExecutionPayloadV3 {
            test_payload(timestamp)
        }
    }

    impl PayloadGenerator for TestPayloadGenerator {
        async fn generate_block(
            &self,
            _parent: &ExecutionBlock,
            timestamp: u64,
            _fee_recipient: &Address,
            _prev_randao: B256,
        ) -> eyre::Result<ExecutionPayloadV3> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            match self.scenario {
                Scenario::Success => Ok(Self::dummy_payload(timestamp)),
                Scenario::UnknownPayloadUntil { succeed_on } => {
                    if attempt < succeed_on {
                        Err(EngineApiRpcError::new(-38001, "Unknown payload", None).into())
                    } else {
                        Ok(Self::dummy_payload(timestamp))
                    }
                }
                Scenario::OtherError => Err(eyre!("a different error")),
            }
        }
    }

    fn parent_block(timestamp: u64) -> ExecutionBlock {
        ExecutionBlock {
            block_hash: B256::ZERO,
            block_number: 0,
            parent_hash: B256::ZERO,
            timestamp,
        }
    }

    fn fee_recipient() -> Address {
        AlloyAddress::ZERO.into()
    }

    fn metrics() -> AppMetrics {
        AppMetrics::new()
    }

    #[tokio::test]
    async fn retry_success_first_attempt() {
        let generator = TestPayloadGenerator::new(Scenario::Success);
        let (payload, lean) = generate_payload_with_retry(
            &parent_block(0),
            &fee_recipient(),
            &generator,
            &metrics(),
            NoLean::None,
        )
        .await
        .expect("payload generation should succeed on first try");
        assert!(lean.is_none(), "flag off: no lean payload");

        assert_eq!(
            generator.attempts.load(Ordering::SeqCst),
            1,
            "should only attempt once"
        );
        assert!(payload.timestamp() >= parent_block(0).timestamp);
    }

    #[tokio::test]
    async fn retry_unknown_until_success() {
        let succeed_on = 6; // 5 failures + 1 success; limit of max retries
        let generator = TestPayloadGenerator::new(Scenario::UnknownPayloadUntil { succeed_on });
        let (payload, _lean) = generate_payload_with_retry(
            &parent_block(10),
            &fee_recipient(),
            &generator,
            &metrics(),
            NoLean::None,
        )
        .await
        .expect("payload should eventually succeed");

        assert_eq!(
            generator.attempts.load(Ordering::SeqCst),
            succeed_on,
            "attempt count should equal succeed_on"
        );
        assert!(payload.timestamp() >= parent_block(10).timestamp);
    }

    #[tokio::test]
    async fn retry_unknown_too_late() {
        let succeed_on = 7; // exceeds max retries
        let generator = TestPayloadGenerator::new(Scenario::UnknownPayloadUntil { succeed_on });
        let err = generate_payload_with_retry(
            &parent_block(100),
            &fee_recipient(),
            &generator,
            &metrics(),
            NoLean::None,
        )
        .await
        .expect_err("should fail after exhausting retries");

        let engine_err =
            EngineApiRpcError::try_from(err).expect("error should be EngineApiRpcError");
        assert!(
            engine_err.is_unknown_payload(),
            "error should be UnknownPayload kind"
        );
        assert_eq!(
            generator.attempts.load(Ordering::SeqCst),
            6,
            "total attempts should be 6 (1 initial + 5 retries)"
        );
    }

    #[tokio::test]
    async fn retry_immediate_other_error() {
        let generator = TestPayloadGenerator::new(Scenario::OtherError);
        let err = generate_payload_with_retry(
            &parent_block(1000),
            &fee_recipient(),
            &generator,
            &metrics(),
            NoLean::None,
        )
        .await
        .expect_err("should fail immediately without retry");

        if let Ok(engine_err) = EngineApiRpcError::try_from(err) {
            assert!(
                !engine_err.is_unknown_payload(),
                "should not classify as UnknownPayload"
            );
        }
        assert_eq!(
            generator.attempts.load(Ordering::SeqCst),
            1,
            "should only attempt once"
        );
    }

    fn test_execution_block(number: u64, timestamp: u64) -> ExecutionBlock {
        ExecutionBlock {
            block_hash: B256::repeat_byte(number as u8),
            block_number: number,
            parent_hash: B256::ZERO,
            timestamp,
        }
    }

    #[tokio::test]
    async fn lean_block_is_built_first_and_its_commitment_becomes_prev_randao() {
        use arc_eth_engine::lean_shim::{LeanBuilt, LeanHead, MockLeanBuilder};
        // a real lean block so the CL's recompute agrees with the claim
        let parent = LeanHead {
            commitment: B256::repeat_byte(1),
            number: 4,
            timestamp_ms: 0,
        };
        let previous_block = test_execution_block(9, 1_000);
        let expect_ts_ms = std::cmp::max(previous_block.timestamp, Engine::timestamp_now()) * 1000;
        let bytes = lean_block_bytes(parent.commitment, 5, expect_ts_ms);
        let commitment = arc_consensus_types::block::decode_lean_block(&bytes)
            .unwrap()
            .commitment;

        let mut builder = MockLeanBuilder::new();
        let b2 = bytes.clone();
        builder
            .expect_build_lean_block()
            .withf(move |p, ts, budget| p.number == 4 && *ts == expect_ts_ms && *budget == 7)
            .times(1)
            .returning(move |_, _, _| {
                Ok(LeanBuilt {
                    commitment,
                    bytes: b2.clone(),
                })
            });

        let mut generator = MockPayloadGenerator::new();
        generator
            .expect_generate_block()
            .withf(move |_, _, _, prev_randao| *prev_randao == commitment)
            .times(1)
            .returning(|_, ts, _, _| Ok(test_payload(ts)));

        let metrics = AppMetrics::default();
        let (payload, lean) = generate_payload_with_retry(
            &previous_block,
            &Address::default(),
            &generator,
            &metrics,
            Some(LeanBuild {
                builder: &builder,
                head: parent,
                budget_gas: 7,
            }),
        )
        .await
        .unwrap();
        assert_eq!(lean.unwrap().commitment(), commitment);
        assert_eq!(payload.timestamp() * 1000, expect_ts_ms);
    }
}
