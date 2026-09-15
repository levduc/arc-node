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

//! Validating the LEAN lane section of a block, once the EVM lane has passed.
//!
//! Validation here is STRUCTURAL + linkage only, in this order:
//!   1. resolve the lean bytes, if the header commits to a block the proposer
//!      did not frame (network origin only);
//!   2. the header binding — `prev_randao` must be the recomputed commitment;
//!   3. timestamp lockstep with the EVM lane;
//!   4. the parent linkage against our lean head (historic replay, or the tip
//!      rules in [`super::catchup`]), and the speculative stage for the anchor.
//!
//! The lean node has no forkchoice — `arc_newBlock` appends PERMANENTLY — so
//! undecided blocks are never fed to it; execution happens once, inline, at
//! the decide anchor. Safety comes from total STF (an invalid tx is a no-op,
//! so a byzantine proposer can never halt the lane) plus the certificate
//! binding the recomputed commitment.
//!
//! The one rule the shape of this module enforces: every "we could not judge"
//! is an `Err` (an abstain the handlers skip the round on), every "we judged
//! it wrong" is [`LeanVerdict::Invalid`]. An `Invalid` recorded against a
//! value that later gets certified sticks forever, because the valid-round
//! rule re-proposes certified values WITHOUT re-validation.

use arc_consensus_types::block::LeanLanePayload;
use arc_eth_engine::lean_shim::{LeanBytesResolver, LeanValidation};

use super::catchup::{validate_lean_tip, LeanTipVerdict, CATCHUP_BUDGET};
use super::{lean_no_verdict, lean_no_verdict_over};
use crate::block::ConsensusBlock;
use crate::metrics::app::LeanNoVerdictReason;

/// What the lean lane says about a block whose EVM lane already passed.
///
/// There is deliberately no "no verdict" variant: an abstain is an `Err`, so
/// it cannot be mistaken for a judgement or recorded against the block.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LeanVerdict {
    /// Nothing observably wrong with the lean section.
    Valid,
    /// An observed violation, with the forensic reason to record.
    Invalid(String),
}

/// Judge a block's lean lane section. `Err` means no verdict this round (the
/// lean node is away, or we are too far behind to judge) — never a failure of
/// the node.
pub(crate) async fn validate_lean_section(
    block: &ConsensusBlock,
    lean_shim: Option<&impl LeanValidation>,
    lean_resolver: Option<&impl LeanBytesResolver>,
    lean_bytes_required: bool,
) -> eyre::Result<LeanVerdict> {
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
    // (`lean_bytes_required == false`) keep the EVM-only reading:
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
                        return Ok(LeanVerdict::Invalid(format!(
                            "lean lane: header commits to unknown lean block {commitment}"
                        )));
                    }
                },
                Ok(None) => {
                    return Ok(LeanVerdict::Invalid(format!(
                        "lean lane: header commits to unknown lean block {commitment}"
                    )));
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
        // recomputed lean commitment. This runs before every other
        // lean check and before any shim call — a mismatched header is a
        // structural forgery regardless of what the lean bytes decode to.
        // (Trivially true for `resolved_lane`, which was fetched BY that
        // commitment and re-checked against it above.)
        if !block.lean_binding_ok() || block.header_lean_commitment() != Some(lane.commitment()) {
            return Ok(LeanVerdict::Invalid(format!(
                "lean lane: header/lean mismatch (prev_randao {:?} vs recomputed {})",
                block.header_lean_commitment(),
                lane.commitment()
            )));
        }
        let evm = &block.execution_payload.payload_inner.payload_inner;
        // Timestamp lockstep with the EVM lane. Numbers are NOT coupled to EVM
        // numbers: the lane may activate mid-chain, so lean numbers advance
        // 1-per-height from activation (sync serving maps by constant offset).
        if lane.decoded.timestamp_ms != evm.timestamp.saturating_mul(1000) {
            return Ok(LeanVerdict::Invalid(format!(
                "lean lane: timestamp lockstep violation (lean ts_ms {} vs evm ts {})",
                lane.decoded.timestamp_ms, evm.timestamp
            )));
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
            match shim.local_head().await {
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
                        match shim.canonical_block_bytes(lane.decoded.number).await {
                            Ok(Some(ours)) if ours == lane.bytes => {
                                // Canonical replay — lean lane section valid;
                                // skip tip-linkage checks entirely.
                            }
                            Ok(_) => {
                                return Ok(LeanVerdict::Invalid(format!(
                                    "lean lane: historic block {} conflicts with our canonical chain",
                                    lane.decoded.number
                                )));
                            }
                            Err(e) => {
                                return Err(lean_no_verdict_over(
                                    LeanNoVerdictReason::LocalUnreachable,
                                    "lean lane: node unreachable during historic validation",
                                    e,
                                ));
                            }
                        }
                        return Ok(LeanVerdict::Valid);
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
                            return Ok(LeanVerdict::Invalid(reason));
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
                    shim.stage_detached(lane.bytes.clone());

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

    Ok(LeanVerdict::Valid)
}

#[cfg(test)]
mod tests {
    use super::*;

    use arc_consensus_types::{Address, Height, Round, B256};
    use arc_eth_engine::lean_shim::{LeanCatchup, LeanHead, MockLeanBytesResolver, NewBlockStatus};
    use malachitebft_app_channel::app::types::core::Validity;

    /// A lean node that must never be called. `LeanValidation` is not
    /// `automock`ed (its catch-up tests need genuinely slow peers), so the
    /// "strict mock" is spelled out: every method panics.
    struct NeverCalled;

    impl LeanCatchup for NeverCalled {
        fn peer_count(&self) -> usize {
            panic!("the stock path must not ask the lean node anything")
        }
        async fn peer_block_bytes(&self, _p: usize, _n: u64) -> eyre::Result<Option<Vec<u8>>> {
            panic!("the stock path must not ask a peer lean node for bytes")
        }
        async fn feed_local(&self, _b: Vec<u8>) -> eyre::Result<NewBlockStatus> {
            panic!("the stock path must not feed the lean node")
        }
        async fn local_head(&self) -> eyre::Result<LeanHead> {
            panic!("the stock path must not read the lean head")
        }
    }

    impl LeanValidation for NeverCalled {
        async fn canonical_block_bytes(&self, _n: u64) -> eyre::Result<Option<Vec<u8>>> {
            panic!("the stock path must not read canonical lean bytes")
        }
        fn stage_detached(&self, _b: Vec<u8>) {
            panic!("the stock path must not stage anything")
        }
    }

    /// The concrete types a bare `None` leaves unconstrained.
    const NO_SHIM: Option<&NeverCalled> = None;
    const NO_RESOLVER: Option<&MockLeanBytesResolver> = None;

    fn stock_block() -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            validity: Validity::Valid,
            execution_payload: crate::block::tests_payload_helper(0x11, vec![]),
            signature: None,
            lean_payload: None,
        }
    }

    /// THE flag-off contract at the seam this module is: with no lean node and
    /// no lean payload — exactly the shape `ARC_PAYMENT_LEAN_LANE` unset
    /// produces at every call site — the lean section is a no-op that returns
    /// Valid. Both mocks are strict (`MockLeanValidation`/`MockLeanBytesResolver`
    /// with no expectations panic if called), so this also pins that nothing
    /// here reaches for a lean node on the stock path.
    #[tokio::test]
    async fn flag_off_is_a_no_op_that_never_touches_a_lean_node() {
        let block = stock_block();
        assert_eq!(
            block
                .execution_payload
                .payload_inner
                .payload_inner
                .prev_randao,
            B256::ZERO,
            "with the lane off the CL leaves prev_randao at zero",
        );
        assert_eq!(block.header_lean_commitment(), None);

        // `lean_bytes_required` is the network-origin flag; both readings must
        // be the same no-op when there is no lane at all.
        for required in [false, true] {
            let verdict = validate_lean_section(&block, NO_SHIM, NO_RESOLVER, required)
                .await
                .expect("the stock path never abstains");
            assert_eq!(verdict, LeanVerdict::Valid, "required={required}");
        }
    }

    /// A node running WITH the lane still votes on EVM-only heights (before
    /// activation, or a height whose proposer framed no lean block): a shim in
    /// hand but no lean payload and a zero header is not something to judge.
    #[tokio::test]
    async fn a_lane_enabled_node_is_a_no_op_on_an_evm_only_height() {
        let block = stock_block();
        let resolver = MockLeanBytesResolver::new();

        let verdict = validate_lean_section(&block, Some(&NeverCalled), Some(&resolver), true)
            .await
            .expect("an EVM-only height is not an abstain");

        assert_eq!(verdict, LeanVerdict::Valid);
    }
}
