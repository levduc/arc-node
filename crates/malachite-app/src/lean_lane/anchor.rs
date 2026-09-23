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

//! The decide anchor: append the lean block the decided EVM header commits to.

use std::time::Duration;

use eyre::eyre;
use tokio::time::{sleep, timeout, Instant};
use tracing::{debug, info, warn};

use arc_consensus_types::{BlockHash, Height};
use arc_eth_engine::lean_shim::{is_unreachable, LeanNode, NewBlockStatus};

/// Total wall-clock budget of one anchor. Past it the height fails and is
/// restarted: the lane cannot advance without this block.
pub(crate) const ANCHOR_DEADLINE: Duration = Duration::from_secs(30);

/// Delay between polls while the node is syncing or unreachable.
const POLL: Duration = Duration::from_millis(150);

/// Appends the lean block `commitment` names. The lean node resolves the bytes
/// itself (its staged copy, a queued copy, or a peer), so the consensus layer
/// only waits: through `SYNCING` and an unreachable node, until `deadline`.
/// Every call is bounded by what is left of the deadline.
pub(crate) async fn anchor_lean_lane(
    node: &dyn LeanNode,
    commitment: BlockHash,
    height: Height,
    deadline: Duration,
) -> eyre::Result<()> {
    let start = Instant::now();
    let mut polls = 0u64;
    loop {
        polls = polls.saturating_add(1);
        let Some(remaining) = deadline
            .checked_sub(start.elapsed())
            .filter(|d| !d.is_zero())
        else {
            return Err(eyre!(
                "lean lane: could not anchor {commitment} at height={height} within {deadline:?} \
                 ({polls} polls)"
            ));
        };
        match timeout(remaining, node.new_block_by_commitment(commitment)).await {
            Ok(Ok(NewBlockStatus::Valid(c))) if c == commitment => {
                if polls > 2 {
                    info!(%height, polls, elapsed = ?start.elapsed(), "🪶 Lean lane anchored");
                } else {
                    debug!(%height, elapsed = ?start.elapsed(), "🪶 Lean lane anchored");
                }
                return Ok(());
            }
            Ok(Ok(NewBlockStatus::Valid(c))) => {
                return Err(eyre!(
                    "lean lane: node appended {c} for anchor {commitment} at height={height}"
                ));
            }
            Ok(Ok(NewBlockStatus::Syncing)) => sleep(POLL).await,
            Ok(Err(e)) if is_unreachable(&e) => {
                warn!(%height, "Lean lane: node unreachable at anchor, waiting: {e:#}");
                sleep(POLL).await;
            }
            Ok(Err(e)) => return Err(e.wrap_err("lean lane: arc_newBlock{commitment} failed")),
            // The node is slow, not wrong: the deadline check ends the loop.
            Err(_) => warn!(%height, "Lean lane: anchor call outran the deadline"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::Ordering;

    use arc_consensus_types::B256;

    use crate::lean_lane::test_lane::{lean_head, test_lane, Step};

    async fn run(steps: Vec<Step>) -> (eyre::Result<()>, usize, Duration) {
        let mut lane = test_lane(lean_head(0), 0, vec![]);
        lane.anchor = std::sync::Mutex::new(steps.into());
        let start = Instant::now();
        let result = anchor_lean_lane(&lane, COMMITMENT, Height::new(7), ANCHOR_DEADLINE).await;
        (
            result,
            lane.anchored.load(Ordering::SeqCst),
            start.elapsed(),
        )
    }

    const COMMITMENT: B256 = B256::repeat_byte(0x5c);

    #[tokio::test(start_paused = true)]
    async fn anchor_waits_through_syncing_and_an_unreachable_node() {
        use Step::*;
        for (steps, calls) in [
            (vec![Valid(COMMITMENT)], 1),
            (vec![Syncing, Syncing, Valid(COMMITMENT)], 3),
            (vec![Unreachable, Valid(COMMITMENT)], 2),
        ] {
            let (result, anchored, _) = run(steps).await;
            result.expect("anchors");
            assert_eq!(anchored, calls);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn anchor_fails_on_a_wrong_block_or_a_hard_error() {
        for (step, expected) in [
            (Step::Valid(B256::repeat_byte(0x99)), "node appended"),
            (Step::Fail, "arc_newBlock"),
        ] {
            let (result, anchored, _) = run(vec![step]).await;
            let err = result.expect_err("must not anchor");
            assert!(format!("{err:#}").contains(expected), "{err:#}");
            assert_eq!(anchored, 1, "no waiting on a node that is wrong");
        }
    }

    /// The deadline is a real bound, including for a call that never returns.
    #[tokio::test(start_paused = true)]
    async fn anchor_gives_up_at_the_deadline() {
        for step in [Step::Syncing, Step::Hang] {
            let (result, _, elapsed) = run(vec![step]).await;
            let err = result.expect_err("must end at the deadline");
            assert!(err.to_string().contains("could not anchor"), "{err:#}");
            assert!(elapsed >= ANCHOR_DEADLINE);
            assert!(elapsed <= ANCHOR_DEADLINE + POLL);
        }
    }
}
