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

//! The tip rules of lean validation: catch the local lean node up to the
//! proposal from peer lean nodes, then judge the parent linkage.
//!
//! The catch-up runs before the verdict because waiting for value-sync
//! deadlocks: a validator that cannot vote never reaches decide. Peer blocks
//! are safe to feed before the verdict, since the local node recomputes every
//! commitment and they are what value-sync would feed anyway. Being behind is
//! this node's property, so running out of budget abstains; only a wrong
//! parent observed from one block below the proposal is a violation.

use std::time::Duration;

use tokio::time::{timeout, Instant};
use tracing::info;

use arc_consensus_types::BlockHash;
use arc_eth_engine::lean_shim::{LeanHead, LeanNode, NewBlockStatus};

use super::LeanVerdict;
use crate::metrics::app::LeanNoVerdictReason;

/// Wall-clock budget for one validation's whole catch-up (all peers, all
/// blocks). It runs inside the vote window.
pub(crate) const CATCHUP_BUDGET: Duration = Duration::from_secs(5);

/// Floor on one peer's share of the remaining budget, capped by what is left.
const MIN_PEER_SLICE: Duration = Duration::from_millis(250);

/// Largest lag, in lean blocks, a catch-up is attempted for. Anything further
/// cannot be closed within the budget, so it abstains without spending it.
const MAX_CATCHUP_LAG: u64 = 1024;

/// Judges the parent linkage of lean block `number` with parent `parent`
/// against the local `head`, catching up from peers first if behind.
/// Returns [`LeanVerdict::Valid`] when the block links onto the head.
pub(crate) async fn validate_lean_tip(
    node: &dyn LeanNode,
    head: LeanHead,
    number: u64,
    parent: BlockHash,
    budget: Duration,
) -> LeanVerdict {
    let lag = number.saturating_sub(head.number);
    if lag > MAX_CATCHUP_LAG {
        return LeanVerdict::Abstain(
            LeanNoVerdictReason::GapTooLarge,
            format!(
                "lean lane: proposal is {lag} blocks ahead of our head (block number {number}, \
                 local head {}), past the {MAX_CATCHUP_LAG}-block catch-up ceiling",
                head.number
            ),
        );
    }

    let (head, stalled) = if number > head.number.saturating_add(1) {
        catch_up(node, head, number, budget).await
    } else {
        (head, None)
    };

    if number > head.number.saturating_add(1) {
        return LeanVerdict::Abstain(
            stalled.unwrap_or(LeanNoVerdictReason::PeersTimedOut),
            format!(
                "lean lane: still behind after catch-up (block number {number}, local head {})",
                head.number
            ),
        );
    }
    // Gossip moved our head past the proposal while we caught up: the historic
    // rule applies now, so judge it again next round.
    if number <= head.number {
        return LeanVerdict::Abstain(
            LeanNoVerdictReason::HeadRanPast,
            format!(
                "lean lane: local head advanced past the proposal during catch-up (block \
                 number {number}, local head {})",
                head.number
            ),
        );
    }
    if parent != head.commitment {
        return LeanVerdict::Invalid(format!(
            "lean lane: parent/number mismatch (block parent {parent} number {number} vs head {} \
             number {})",
            head.commitment, head.number
        ));
    }
    LeanVerdict::Valid
}

/// Pulls the blocks between `head` and `target` from the peers and feeds them
/// to the local node, until the head sits one below `target` or the budget is
/// spent. Returns the head reached and, if it stopped short, why.
///
/// Each peer gets an even share of what is left of the budget (at least
/// [`MIN_PEER_SLICE`]), so one unresponsive peer cannot spend all of it, and a
/// peer that timed out or failed once is not asked again.
async fn catch_up(
    node: &dyn LeanNode,
    mut head: LeanHead,
    target: u64,
    budget: Duration,
) -> (LeanHead, Option<LeanNoVerdictReason>) {
    let start = Instant::now();
    let remaining = || budget.checked_sub(start.elapsed()).filter(|d| !d.is_zero());
    let peers = node.peer_count();
    let mut dead = vec![false; peers];
    let mut fed = 0u64;
    let mut stalled = None;
    // Set when our own node would not take bytes a peer served, so the abstain
    // names the local node rather than the peers.
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
            let alive = dead[peer..].iter().filter(|d| !**d).count();
            let slice = left
                .checked_div(u32::try_from(alive).unwrap_or(u32::MAX))
                .unwrap_or(left)
                .max(MIN_PEER_SLICE)
                .min(left);
            let bytes = match timeout(slice, node.peer_block_bytes(peer, next)).await {
                Ok(Ok(Some(bytes))) => bytes,
                // Healthy, just does not have it yet: ask again next block.
                Ok(Ok(None)) => continue,
                _ => {
                    dead[peer] = true;
                    continue;
                }
            };
            let Some(left) = remaining() else {
                stalled = Some(LeanNoVerdictReason::BudgetExhausted);
                break 'catchup;
            };
            if let Ok(Ok(NewBlockStatus::Valid(_))) = timeout(left, node.new_block(bytes)).await {
                advanced = true;
                fed = fed.saturating_add(1);
                break;
            }
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
            break;
        }
        let Some(left) = remaining() else {
            stalled = Some(LeanNoVerdictReason::BudgetExhausted);
            break;
        };
        match timeout(left, node.get_head()).await {
            Ok(Ok(h)) => head = h,
            _ => {
                stalled = Some(LeanNoVerdictReason::LocalUnreachable);
                break;
            }
        }
    }

    if fed > 0 {
        info!(
            "🪶 lean lane: validation-time catch-up fed {fed} blocks from peers in {:?} (local \
             head now {})",
            start.elapsed(),
            head.number
        );
    }
    (head, stalled)
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::B256;

    use crate::lean_lane::test_lane::{commitment_of, lean_head, test_lane};

    const SLOW: Duration = Duration::from_secs(30);
    const FAST: Duration = Duration::ZERO;

    fn abstain_reason(verdict: &LeanVerdict) -> Option<LeanNoVerdictReason> {
        match verdict {
            LeanVerdict::Abstain(reason, _) => Some(*reason),
            _ => None,
        }
    }

    /// Lag abstains with a reason, never `Invalid`; the budget and per-peer
    /// slices bound the time spent. Virtual time: slow peers sleep 30 s.
    #[tokio::test(start_paused = true)]
    async fn lag_abstains_and_the_budget_is_sliced_per_peer() {
        // (peers, target) -> (expected reason, peers asked in order)
        type Peers = Vec<(Duration, bool)>;
        let cases: [(Peers, u64, LeanNoVerdictReason, Vec<usize>); 3] = [
            (
                vec![(SLOW, true), (SLOW, true)],
                12,
                LeanNoVerdictReason::PeersTimedOut,
                vec![0, 1],
            ),
            (vec![], 12, LeanNoVerdictReason::NoPeers, vec![]),
            (
                vec![(FAST, true)],
                10 + MAX_CATCHUP_LAG + 1,
                LeanNoVerdictReason::GapTooLarge,
                vec![],
            ),
        ];
        for (peers, target, reason, asked) in cases {
            let head = lean_head(10);
            let lane = test_lane(head, target.min(12), peers);
            let started = Instant::now();

            let verdict = validate_lean_tip(&lane, head, target, B256::ZERO, CATCHUP_BUDGET).await;

            assert_eq!(abstain_reason(&verdict), Some(reason), "{verdict:?}");
            assert_eq!(*lane.asked.lock().unwrap(), asked);
            assert!(started.elapsed() <= CATCHUP_BUDGET);
            if reason == LeanNoVerdictReason::GapTooLarge {
                assert_eq!(started.elapsed(), Duration::ZERO, "no budget spent");
            }
        }
    }

    /// A slow peer costs only its slice, and a dead peer is asked once for the
    /// whole catch-up, so the healthy peer behind it still catches us up.
    #[tokio::test(start_paused = true)]
    async fn a_slow_peer_costs_one_slice_and_a_fast_peer_catches_us_up() {
        let head = lean_head(10);
        let lane = test_lane(head, 14, vec![(SLOW, true), (FAST, true)]);
        let parent = commitment_of(&lane.chain, 13);

        let verdict = validate_lean_tip(&lane, head, 14, parent, CATCHUP_BUDGET).await;

        assert_eq!(verdict, LeanVerdict::Valid);
        assert_eq!(*lane.asked.lock().unwrap(), vec![0, 1, 1, 1]);
        assert_eq!(lane.head.lock().unwrap().number, 13);
    }

    /// The ceiling is not a cliff: a lag right at it is still caught up.
    #[tokio::test(start_paused = true)]
    async fn a_lag_at_the_ceiling_is_still_caught_up() {
        let head = lean_head(10);
        let target = head.number + MAX_CATCHUP_LAG;
        let lane = test_lane(head, target, vec![(FAST, true)]);
        let parent = commitment_of(&lane.chain, target - 1);

        let verdict = validate_lean_tip(&lane, head, target, parent, CATCHUP_BUDGET).await;

        assert_eq!(verdict, LeanVerdict::Valid);
    }

    /// Once one block below the proposal, a wrong parent is a violation,
    /// whether or not a catch-up ran first.
    #[tokio::test(start_paused = true)]
    async fn a_wrong_parent_one_below_the_proposal_is_invalid() {
        for number in [11, 12] {
            let head = lean_head(10);
            let lane = test_lane(head, 12, vec![(FAST, true)]);

            let verdict =
                validate_lean_tip(&lane, head, number, B256::repeat_byte(0xbb), CATCHUP_BUDGET)
                    .await;

            assert!(matches!(verdict, LeanVerdict::Invalid(ref r) if r.contains("parent/number")));
            assert_eq!(lane.head.lock().unwrap().number, number - 1);
        }
    }
}
