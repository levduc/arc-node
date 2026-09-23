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

//! The lean section of a block's validation, run once the EVM lane passed.
//!
//! Structural and linkage checks only, in this order:
//! 1. resolve the lean bytes from the local node when the header commits to a
//!    lean block the proposer did not ship;
//! 2. the header binding: `prev_randao` is the recomputed lean commitment;
//! 3. timestamp lockstep with the EVM lane;
//! 4. parent linkage against the local lean head (historic replay, or the tip
//!    rules in [`super::catchup`]), then a speculative stage for the anchor.
//!
//! The lean node appends permanently and has no forkchoice, so undecided
//! blocks are only staged, never appended; they execute once, when decided.

use arc_eth_engine::lean_shim::LeanNode;

use super::catchup::{validate_lean_tip, CATCHUP_BUDGET};
use super::{fetch_lean_payload, LeanVerdict};
use crate::block::ConsensusBlock;
use crate::metrics::app::LeanNoVerdictReason;

/// Judges the lean section of `block`.
///
/// `lean` is the local lean node for a block that arrived from the network. It
/// is `None` with the lane off, and for blocks this node built or loaded from
/// its own store: those keep the purely structural checks and never ask the
/// node anything.
pub(crate) async fn validate_lean_section(
    block: &ConsensusBlock,
    lean: Option<&dyn LeanNode>,
) -> LeanVerdict {
    let resolved;
    let lane = match (&block.lean_payload, block.header_lean_commitment(), lean) {
        (Some(lane), _, _) => lane,
        // The header commits to a lean block the proposer did not ship. Voting
        // for it on the EVM lane alone would certify a block whose decide
        // anchor can never complete, so it is valid only if our node has it.
        (None, Some(commitment), Some(node)) => match fetch_lean_payload(node, commitment).await {
            Ok(Some(lane)) => {
                resolved = lane;
                &resolved
            }
            Ok(None) => {
                return LeanVerdict::Invalid(format!(
                    "lean lane: header commits to unknown lean block {commitment}"
                ));
            }
            Err(e) => return unreachable_abstain("resolving the header commitment", e),
        },
        // A lane-enabled node never proposes without a lean block (a failed
        // lean build skips the round), so a network block with neither lean
        // bytes nor a header commitment can only come from a faulty proposer.
        // Voting it Valid would certify a height whose decide anchor refuses
        // it on every node, halting the chain.
        (None, None, Some(_)) => {
            return LeanVerdict::Invalid(
                "lean lane: lane-enabled block carries no lean commitment".to_string(),
            );
        }
        _ => return LeanVerdict::Valid,
    };

    if block.header_lean_commitment() != Some(lane.commitment()) {
        return LeanVerdict::Invalid(format!(
            "lean lane: header/lean mismatch (prev_randao {:?} vs recomputed {})",
            block.header_lean_commitment(),
            lane.commitment()
        ));
    }

    let evm_timestamp = block.execution_payload.timestamp();
    if lane.decoded.timestamp_ms != evm_timestamp.saturating_mul(1000) {
        return LeanVerdict::Invalid(format!(
            "lean lane: timestamp lockstep violation (lean ts_ms {} vs evm ts {evm_timestamp})",
            lane.decoded.timestamp_ms
        ));
    }

    let Some(node) = lean else {
        return LeanVerdict::Valid;
    };
    let head = match node.get_head().await {
        Ok(head) => head,
        Err(e) => return unreachable_abstain("validation", e),
    };

    // A block at or below our head is a replay of our own past (value-sync
    // while consensus lags the lean chain): valid iff it is our block.
    if lane.decoded.number <= head.number {
        return match node.get_block_bytes(lane.decoded.number).await {
            Ok(Some(ours)) if ours == lane.bytes => LeanVerdict::Valid,
            Ok(_) => LeanVerdict::Invalid(format!(
                "lean lane: historic block {} conflicts with our canonical chain",
                lane.decoded.number
            )),
            Err(e) => unreachable_abstain("historic validation", e),
        };
    }

    let verdict = validate_lean_tip(
        node,
        head,
        lane.decoded.number,
        lane.decoded.parent,
        CATCHUP_BUDGET,
    )
    .await;
    if verdict == LeanVerdict::Valid {
        node.stage_block(lane.bytes.clone());
    }
    verdict
}

fn unreachable_abstain(during: &str, e: eyre::Report) -> LeanVerdict {
    LeanVerdict::Abstain(
        LeanNoVerdictReason::LocalUnreachable,
        format!("lean lane: node unreachable during {during}: {e:#}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::Ordering;

    use arc_consensus_types::lean::{test_lean_block_bytes, LeanLanePayload};
    use arc_consensus_types::B256;

    use crate::lean_lane::test_lane::{block, lean_head, strict_lane, test_lane, Answer, TestLane};

    fn commitment(bytes: &[u8]) -> B256 {
        LeanLanePayload::new(bytes.to_vec()).unwrap().commitment()
    }

    #[derive(Debug)]
    enum Expect {
        Valid,
        Invalid(&'static str),
        Abstain(LeanNoVerdictReason),
    }

    fn check(verdict: &LeanVerdict, expect: &Expect) -> bool {
        match (verdict, expect) {
            (LeanVerdict::Valid, Expect::Valid) => true,
            (LeanVerdict::Invalid(r), Expect::Invalid(s)) => r.contains(s),
            (LeanVerdict::Abstain(r, _), Expect::Abstain(e)) => r == e,
            _ => false,
        }
    }

    /// With no lean payload and a zero header: the flag-off shape (`None`) is a
    /// no-op that never contacts a lean node, while a lane-enabled node votes
    /// such a network block Invalid without contacting its node (a strict
    /// double) — certifying it would make every decide anchor refuse it.
    #[tokio::test]
    async fn an_evm_only_block_is_valid_only_with_the_lane_off() {
        let block = block(1, B256::ZERO, None);
        assert_eq!(
            validate_lean_section(&block, None).await,
            LeanVerdict::Valid
        );
        let strict = strict_lane();
        assert!(check(
            &validate_lean_section(&block, Some(&strict as &dyn LeanNode)).await,
            &Expect::Invalid("carries no lean commitment")
        ));
    }

    /// Structural failures are judged before any call to the lean node, and a
    /// block this node built or loaded from its store (`None`) gets only those.
    #[tokio::test]
    async fn binding_and_lockstep_are_checked_before_any_lean_node_call() {
        let bytes = test_lean_block_bytes(B256::repeat_byte(1), 5, 7_000, &[]);
        let bound = commitment(&bytes);
        let strict = strict_lane();
        let cases = [
            (block(7, bound, Some(bytes.clone())), Expect::Valid),
            (
                block(7, B256::repeat_byte(0xee), Some(bytes.clone())),
                Expect::Invalid("header/lean mismatch"),
            ),
            (
                block(7, B256::ZERO, Some(bytes.clone())),
                Expect::Invalid("header/lean mismatch"),
            ),
            (
                block(8, bound, Some(bytes.clone())),
                Expect::Invalid("timestamp lockstep"),
            ),
            // Store-loaded row: header commits, bytes were never stored.
            (block(7, bound, None), Expect::Valid),
        ];
        for (block, expect) in cases {
            let verdict = validate_lean_section(&block, None).await;
            assert!(check(&verdict, &expect), "{verdict:?} vs {expect:?}");
            if !matches!(expect, Expect::Valid) {
                let verdict = validate_lean_section(&block, Some(&strict)).await;
                assert!(check(&verdict, &expect), "{verdict:?} vs {expect:?}");
            }
        }
    }

    /// Linkage against the local head: linked blocks are staged; a wrong
    /// parent, a conflicting historic block and a node that cannot produce the
    /// block its header names are violations; lag and an unreachable node
    /// abstain. Local head 10, canonical chain 11..=12.
    #[tokio::test(start_paused = true)]
    async fn linkage_verdicts_against_the_local_lean_head() {
        let head = lean_head(10);
        let chain = test_lane(head, 13, vec![]).chain;
        let b = |n: u64| chain[&n].clone();
        let foreign = test_lean_block_bytes(B256::repeat_byte(0xbb), 11, 11_000, &[]);
        let framed = |bytes: Vec<u8>, ts: u64| block(ts, commitment(&bytes), Some(bytes));
        let header_only = |bytes: &[u8], ts: u64| block(ts, commitment(bytes), None);

        // (block, local head number, resolver answer, expected, staged)
        let cases: Vec<(ConsensusBlock, u64, Option<Answer>, Expect, usize)> = vec![
            (framed(b(11), 11), 10, None, Expect::Valid, 1),
            (
                framed(foreign.clone(), 11),
                10,
                None,
                Expect::Invalid("parent/number"),
                0,
            ),
            (
                framed(b(13), 13),
                10,
                None,
                Expect::Abstain(LeanNoVerdictReason::NoPeers),
                0,
            ),
            (framed(b(11), 11), 12, None, Expect::Valid, 0),
            (
                framed(foreign.clone(), 11),
                12,
                None,
                Expect::Invalid("historic block 11"),
                0,
            ),
            (header_only(&b(11), 11), 10, None, Expect::Valid, 1),
            (
                header_only(&b(11), 11),
                10,
                Some(Answer::Missing),
                Expect::Invalid("unknown lean block"),
                0,
            ),
            (
                header_only(&b(11), 11),
                10,
                Some(Answer::Bytes(foreign.clone())),
                Expect::Invalid("unknown lean block"),
                0,
            ),
            (
                header_only(&b(11), 11),
                10,
                Some(Answer::Unreachable),
                Expect::Abstain(LeanNoVerdictReason::LocalUnreachable),
                0,
            ),
        ];
        for (i, (block, head_number, answer, expect, staged)) in cases.into_iter().enumerate() {
            let lane = TestLane {
                by_commitment: answer,
                ..test_lane(head, 13, vec![])
            };
            if head_number != head.number {
                *lane.head.lock().unwrap() = lean_head(head_number);
            }
            let verdict = validate_lean_section(&block, Some(&lane)).await;
            assert!(
                check(&verdict, &expect),
                "case {i}: {verdict:?} vs {expect:?}"
            );
            assert_eq!(lane.staged.load(Ordering::SeqCst), staged, "case {i}");
        }
    }
}
