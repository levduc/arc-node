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
use tracing::{error, warn};

use malachitebft_app_channel::app::types::core::Validity;

use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadStatusEnum};

use arc_consensus_types::Address;
use arc_eth_engine::engine::Engine;
use arc_eth_engine::json_structures::ExecutionBlock;
use arc_eth_engine::rpc::EngineApiRpcError;

use crate::block::ConsensusBlock;
use crate::metrics::app::{AppMetrics, InvalidPayloadSource};
use crate::store::repositories::InvalidPayloadsRepository;
use arc_consensus_db::invalid_payloads::InvalidPayload;

pub async fn generate_payload_with_retry(
    previous_block: &ExecutionBlock,
    fee_recipient: &Address,
    generator: &impl PayloadGenerator,
    metrics: &AppMetrics,
) -> eyre::Result<ExecutionPayloadV3> {
    const MAX_RETRIES: usize = 5;
    const RETRY_POLICY: ConstantBuilder = ConstantBuilder::new()
        .with_delay(Duration::from_millis(100))
        .with_max_times(MAX_RETRIES);

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

        let _guard = metrics.start_engine_api_timer("generate_block");

        generator
            .generate_block(previous_block, timestamp, fee_recipient)
            .await
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
    ) -> eyre::Result<ExecutionPayloadV3>;
}

pub struct EnginePayloadGenerator<'a> {
    pub engine: &'a Engine,
}

impl<'a> PayloadGenerator for EnginePayloadGenerator<'a> {
    async fn generate_block(
        &self,
        parent: &ExecutionBlock,
        timestamp: u64,
        fee_recipient: &Address,
    ) -> eyre::Result<ExecutionPayloadV3> {
        self.engine
            .generate_block(parent, timestamp, fee_recipient)
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
}

impl<'a> EnginePayloadValidator<'a> {
    pub fn new(engine: &'a Engine, metrics: &'a AppMetrics) -> Self {
        Self { engine, metrics }
    }
}

impl PayloadValidator for EnginePayloadValidator<'_> {
    async fn validate_payload(
        &self,
        payload: &ExecutionPayloadV3,
    ) -> eyre::Result<PayloadValidationResult> {
        validate_payload(self.engine, payload, self.metrics).await
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
/// - `Ok(Valid)`: the engine accepted the payload, or returned an unexpected status
///   such as `SYNCING` or `ACCEPTED` (logged as a warning).
/// - `Ok(Invalid { reason })`: the engine explicitly rejected the payload, either
///   via its status response (`INVALID`) or via a JSON-RPC error
///   (`EngineApiRpcError`).
/// - `Err(..)`: the engine replied with status `SYNCING` or `ACCEPTED`, or an
///   unrelated internal error occurred in the call stack.
async fn validate_payload(
    engine: &Engine,
    execution_payload: &ExecutionPayloadV3,
    metrics: &AppMetrics,
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
        .notify_new_block(execution_payload, versioned_hashes)
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
                Err(eyre::eyre!(
                    "unexpected {status:?} status from engine for block {block_hash} at height {height}"
                ))
            }
        },
        Err(e) => {
            if let Ok(engine_api_error) = EngineApiRpcError::try_from(&e) {
                // JSON-RPC error here means that the call to
                // `engine.newPayload` failed the preliminary structural
                // validation of the payload.
                // Instead of returning an error and possibly crashing the app,
                // we mark the payload as invalid.
                error!(
                    %block_hash,
                    "Invalid payload: {engine_api_error}",
                );
                return Ok(PayloadValidationResult::Invalid {
                    reason: engine_api_error.to_string(),
                });
            }

            // Unrelated internal error in the call stack.
            let msg = format!(
                "call to EngineAPI::new_payload failed when validating block: {block_hash}",
            );
            Err(e.wrap_err(msg))
        }
    }
}

/// How the payment lane is validated during the consensus round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentExecMode {
    /// Current behavior: the payment payload is re-executed (engine newPayload)
    /// and the vote is gated on the engine verdict.
    Gated,
    /// EXPERIMENTAL (`ARC_PAYMENT_DEFERRED_EXEC=1`): the vote is gated only on
    /// structural validity (block-hash consistency, lane lockstep, parent link);
    /// execution happens off the vote path and is anchored at decide.
    Deferred,
}

/// Structural validation of a payment payload — no engine call, no execution.
///
/// Checks, in order:
/// 1. **Block-hash consistency**: rebuild the header from the payload
///    (`into_block_raw` recomputes the transactions root from the raw tx bytes,
///    no signature recovery) with Arc's `parent_beacon_block_root = parent_hash`
///    convention, and require `header.hash_slow() == payload.block_hash`. This
///    transitively commits the tx list, parent hash, timestamp, gas fields and
///    the claimed state root — the body voted on is the body the hash names.
/// 2. **Lane lockstep**: payment block_number/timestamp equal the EVM lane's
///    (build invariant of `build_block`).
/// 3. **Parent link** (best-effort): payment parent_hash matches the previous
///    decided payment block hash when the caller knows it; skipped when `None`
///    (first height after boot/restart).
pub fn validate_payment_payload_structurally(
    payment: &ExecutionPayloadV3,
    evm: &ExecutionPayloadV3,
    expected_parent: Option<alloy_primitives::B256>,
) -> PayloadValidationResult {
    let inner = &payment.payload_inner.payload_inner;
    let claimed_hash = inner.block_hash;
    let parent_hash = inner.parent_hash;

    // 2) lane lockstep (cheap; check first to fail fast on garbage)
    let evm_inner = &evm.payload_inner.payload_inner;
    if inner.block_number != evm_inner.block_number {
        return PayloadValidationResult::Invalid {
            reason: format!(
                "payment lane block_number {} != evm lane {}",
                inner.block_number, evm_inner.block_number
            ),
        };
    }
    if inner.timestamp != evm_inner.timestamp {
        return PayloadValidationResult::Invalid {
            reason: format!(
                "payment lane timestamp {} != evm lane {}",
                inner.timestamp, evm_inner.timestamp
            ),
        };
    }

    // 3) parent link, when known
    if let Some(expected) = expected_parent {
        if parent_hash != expected {
            return PayloadValidationResult::Invalid {
                reason: format!(
                    "payment lane parent {parent_hash} != expected {expected}"
                ),
            };
        }
    }

    // 1) block-hash consistency
    let mut block = match payment.clone().into_block_raw() {
        Ok(b) => b,
        Err(e) => {
            return PayloadValidationResult::Invalid {
                reason: format!("payment payload malformed: {e}"),
            }
        }
    };
    // Arc convention: parent_beacon_block_root = parent execution block hash
    // (see eth-engine notify_new_block / payload attributes).
    block.header.parent_beacon_block_root = Some(parent_hash);
    // Prague: Arc's consensus protocol ships payloads with NO requests sidecar,
    // so the only requests hash any Arc validator can ever accept (gated mode
    // enforces this via newPayload with empty execution_requests) is the empty
    // one. Measured on live payment blocks: requestsHash = sha256 of empty.
    block.header.requests_hash = Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH);
    let computed = block.header.hash_slow();
    if computed != claimed_hash {
        return PayloadValidationResult::Invalid {
            reason: format!(
                "payment block hash mismatch: computed {computed}, claimed {claimed_hash}"
            ),
        };
    }

    PayloadValidationResult::Valid
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
    payment_engine: Option<&Engine>,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    block: &ConsensusBlock,
    store: &impl InvalidPayloadsRepository,
    metrics: &AppMetrics,
    payment_exec_mode: PaymentExecMode,
    expected_payment_parent: Option<alloy_primitives::B256>,
) -> eyre::Result<Validity> {
    // EVM lane (validated via the mockable validator).
    let result = payload_validator
        .validate_payload(&block.execution_payload)
        .await?;

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
    if let Some(lane) = block.lean_payload.as_ref() {
        if block.payment_payload.is_some() {
            record_invalid_payload(
                block,
                "lean lane: block carries BOTH payment_payload and lean_payload",
                store,
                metrics,
            )
            .await;
            return Ok(Validity::Invalid);
        }
        let evm = &block.execution_payload.payload_inner.payload_inner;
        // Lane lockstep: number and (ms-scaled) timestamp must mirror the EVM
        // lane exactly — this also makes sync serving a trivial by-number fetch.
        if lane.decoded.number != evm.block_number
            || lane.decoded.timestamp_ms != evm.timestamp * 1000
        {
            record_invalid_payload(
                block,
                &format!(
                    "lean lane: lockstep violation (lean number {} ts_ms {} vs evm number {} ts {})",
                    lane.decoded.number, lane.decoded.timestamp_ms, evm.block_number, evm.timestamp
                ),
                store,
                metrics,
            )
            .await;
            return Ok(Validity::Invalid);
        }
        // Parent linkage vs OUR lean head: prevents a byzantine proposer from
        // getting a wrong-parent block certified (which would make the decide
        // anchor fail network-wide = a halt). If WE are behind, we vote Nil
        // and sync catches us up — safe either way.
        if let Some(shim) = lean_shim {
            match shim.get_head().await {
                Ok(head) => {
                    if lane.decoded.parent != head.commitment
                        || lane.decoded.number != head.number + 1
                    {
                        record_invalid_payload(
                            block,
                            &format!(
                                "lean lane: parent/number mismatch (block parent {} number {} vs head {} number {})",
                                lane.decoded.parent, lane.decoded.number, head.commitment, head.number
                            ),
                            store,
                            metrics,
                        )
                        .await;
                        return Ok(Validity::Invalid);
                    }
                }
                Err(e) => {
                    // Unreachable lean node: cannot verify linkage -> Nil vote.
                    record_invalid_payload(
                        block,
                        &format!("lean lane: node unreachable during validation: {e:#}"),
                        store,
                        metrics,
                    )
                    .await;
                    return Ok(Validity::Invalid);
                }
            }
        }
    }

    // Payment lane (second EL): re-execute the payment payload when both the
    // payload and a payment engine are present. A block is valid only if BOTH
    // lanes validate, so every validator computes identical roots for each lane.
    if let (Some(engine), Some(payment_payload)) = (payment_engine, block.payment_payload.as_ref()) {
        let payment_result = match payment_exec_mode {
            PaymentExecMode::Gated => {
                let payment_validator = EnginePayloadValidator::new(engine, metrics);
                payment_validator.validate_payload(payment_payload).await?
            }
            PaymentExecMode::Deferred => validate_payment_payload_structurally(
                payment_payload,
                &block.execution_payload,
                expected_payment_parent,
            ),
        };
        if let PayloadValidationResult::Invalid { reason } = payment_result {
            let reason = format!("payment lane: {reason}");
            record_invalid_payload(block, &reason, store, metrics).await;
            return Ok(Validity::Invalid);
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
        block_hash = %block.block_hash(),
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
            block_hash = %block.block_hash(),
            proposer = %block.proposer,
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

    use arc_consensus_types::{Address, Height, Round, B256};
    use arc_eth_engine::engine::{MockEngineAPI, MockEthereumAPI};
    use arc_eth_engine::json_structures::ExecutionBlock;

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

    /// Builds a payment payload whose block_hash is CORRECT for its contents
    /// under Arc's pbbr = parent_hash convention.
    fn structurally_valid_payload(
        parent: B256,
        block_number: u64,
        timestamp: u64,
        transactions: Vec<AlloyBytes>,
    ) -> ExecutionPayloadV3 {
        let mut p = test_payload(timestamp);
        p.payload_inner.payload_inner.parent_hash = parent;
        p.payload_inner.payload_inner.block_number = block_number;
        p.payload_inner.payload_inner.transactions = transactions;
        let mut block = p
            .clone()
            .into_block_raw()
            .expect("test payload must convert");
        block.header.parent_beacon_block_root = Some(parent);
        // Prague: Arc's consensus protocol ships payloads with NO requests sidecar,
        // so the only requests hash any Arc validator can ever accept (gated mode
        // enforces this via newPayload with empty execution_requests) is the empty
        // one. Measured on live payment blocks: requestsHash = sha256 of empty.
        block.header.requests_hash = Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH);
        p.payload_inner.payload_inner.block_hash = block.header.hash_slow();
        p
    }

    #[test]
    fn structural_ok_and_hash_tamper_detected() {
        let parent = B256::repeat_byte(0xaa);
        let txs = vec![AlloyBytes::from(vec![0xde, 0xad, 0xbe, 0xef])];
        let evm = structurally_valid_payload(B256::repeat_byte(0x11), 7, 1000, vec![]);
        let pay = structurally_valid_payload(parent, 7, 1000, txs.clone());

        // valid as built
        assert_eq!(
            validate_payment_payload_structurally(&pay, &evm, Some(parent)),
            PayloadValidationResult::Valid
        );

        // tamper with the tx list without recomputing the hash -> must be Invalid
        let mut tampered = pay.clone();
        tampered.payload_inner.payload_inner.transactions =
            vec![AlloyBytes::from(vec![0x01, 0x02])];
        assert!(matches!(
            validate_payment_payload_structurally(&tampered, &evm, Some(parent)),
            PayloadValidationResult::Invalid { .. }
        ));

        // tamper with the claimed hash -> Invalid
        let mut tampered = pay.clone();
        tampered.payload_inner.payload_inner.block_hash = B256::repeat_byte(0x77);
        assert!(matches!(
            validate_payment_payload_structurally(&tampered, &evm, Some(parent)),
            PayloadValidationResult::Invalid { .. }
        ));

        // tamper with a header claim (state_root) without recomputing -> Invalid
        let mut tampered = pay.clone();
        tampered.payload_inner.payload_inner.state_root = B256::repeat_byte(0x55);
        assert!(matches!(
            validate_payment_payload_structurally(&tampered, &evm, Some(parent)),
            PayloadValidationResult::Invalid { .. }
        ));
    }

    #[test]
    fn structural_lane_lockstep_enforced() {
        let parent = B256::repeat_byte(0xaa);
        let evm = structurally_valid_payload(B256::repeat_byte(0x11), 7, 1000, vec![]);

        // block_number mismatch
        let pay = structurally_valid_payload(parent, 8, 1000, vec![]);
        assert!(matches!(
            validate_payment_payload_structurally(&pay, &evm, Some(parent)),
            PayloadValidationResult::Invalid { .. }
        ));

        // timestamp mismatch
        let pay = structurally_valid_payload(parent, 7, 1001, vec![]);
        assert!(matches!(
            validate_payment_payload_structurally(&pay, &evm, Some(parent)),
            PayloadValidationResult::Invalid { .. }
        ));
    }

    #[test]
    fn structural_parent_link() {
        let parent = B256::repeat_byte(0xaa);
        let evm = structurally_valid_payload(B256::repeat_byte(0x11), 7, 1000, vec![]);
        let pay = structurally_valid_payload(parent, 7, 1000, vec![]);

        // wrong expected parent -> Invalid
        assert!(matches!(
            validate_payment_payload_structurally(
                &pay,
                &evm,
                Some(B256::repeat_byte(0xbb))
            ),
            PayloadValidationResult::Invalid { .. }
        ));

        // unknown expected parent -> check skipped, Valid
        assert_eq!(
            validate_payment_payload_structurally(&pay, &evm, None),
            PayloadValidationResult::Valid
        );
    }

    #[tokio::test]
    async fn validate_payload_returns_valid_on_ok_status() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _| {
            Ok(PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            })
        });

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let result = validate_payload(&engine, &payload, &metrics)
            .await
            .expect("payload validation should succeed");

        assert_eq!(result, PayloadValidationResult::Valid);
    }

    #[tokio::test]
    async fn validate_payload_returns_invalid_on_invalid_status() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _| {
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

        let result = validate_payload(&engine, &payload, &metrics)
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
    async fn validate_payload_returns_invalid_on_rpc_error() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload().returning(|_, _, _| {
            let rpc_error = EngineApiRpcError::new(42, "engine API error", None);
            Err(eyre::Report::new(rpc_error))
        });

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let result = validate_payload(&engine, &payload, &metrics)
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
    async fn validate_payload_propagates_other_errors() {
        let mut mock = MockEngineAPI::new();
        mock.expect_new_payload()
            .returning(|_, _, _| Err(eyre::eyre!("some error")));

        let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
        let payload = test_payload(0);
        let metrics = AppMetrics::default();

        let err = validate_payload(&engine, &payload, &metrics)
            .await
            .expect_err("payload validation should return an error");

        let msg = err.to_string();
        assert!(
            msg.contains("call to EngineAPI::new_payload failed"),
            "error message should describe the failure, got: {msg}",
        );
    }

    #[tokio::test]
    async fn validate_payload_returns_err_on_unexpected_status() {
        let test_cases = [PayloadStatusEnum::Syncing, PayloadStatusEnum::Accepted];

        for status in test_cases {
            let mut mock = MockEngineAPI::new();
            let status_for_mock = status.clone();
            mock.expect_new_payload().returning(move |_, _, _| {
                Ok(PayloadStatus {
                    status: status_for_mock.clone(),
                    latest_valid_hash: None,
                })
            });

            let engine = Engine::new(Box::new(mock), Box::new(MockEthereumAPI::new()));
            let payload = test_payload(0);
            let metrics = AppMetrics::default();

            let result = validate_payload(&engine, &payload, &metrics)
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
            payment_payload: None,
            lean_payload: None,
        }
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
            None,
            &block,
            &store,
            &metrics,
            PaymentExecMode::Gated,
            None,
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
            None,
            &block,
            &store,
            &metrics,
            PaymentExecMode::Gated,
            None,
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
            None,
            &block,
            &store,
            &metrics,
            PaymentExecMode::Gated,
            None,
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
            None,
            &block,
            &store,
            &metrics,
            PaymentExecMode::Gated,
            None,
        )
            .await
            .expect("verdict should be returned even when forensics persist fails");

        assert_eq!(validity, Validity::Invalid);
        assert_eq!(metrics.get_invalid_payloads_count(), 1);
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
        let payload =
            generate_payload_with_retry(&parent_block(0), &fee_recipient(), &generator, &metrics())
                .await
                .expect("payload generation should succeed on first try");

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
        let payload = generate_payload_with_retry(
            &parent_block(10),
            &fee_recipient(),
            &generator,
            &metrics(),
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
}
