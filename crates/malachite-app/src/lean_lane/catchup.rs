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

//! The lean lane's validation-time TIP rules: catch this node's lean lane up
//! to the proposal from the peer lean nodes, then judge the parent linkage
//! against our head.
//!
//! The split between the two failure outcomes is the safety rule this module
//! exists for: an `Invalid` recorded against a value that later gets certified
//! sticks forever (the valid-round rule re-proposes certified values WITHOUT
//! re-validation), so only an observed protocol violation may produce one. Us
//! being BEHIND is a property of THIS node and must yield no verdict at all.

use std::time::Duration;

use arc_consensus_types::BlockHash;
use arc_eth_engine::lean_shim::{LeanCatchup, LeanHead};

use crate::metrics::app::LeanNoVerdictReason;

/// Wall-clock ceiling for the WHOLE validation-time catch-up (all peers, all
/// blocks). This runs inside the vote window: the shim's own transport retry
/// is ~15 s per call, and an unbounded loop of those misses the round
/// entirely. 5 s is under the 500 ms-pacer propose window's slack and still
/// buys dozens of peer blocks on a healthy fleet.
pub(crate) const CATCHUP_BUDGET: Duration = Duration::from_secs(5);

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

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::B256;

    use crate::lean_lane::test_lane::{commitment_of, lean_head, test_lane};

    // Virtual time (`start_paused`): the slow peers below sleep 30 s, which
    // the runtime skips to the earliest timeout deadline, so the budget rules
    // are exercised exactly and the tests run instantly.

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
}
