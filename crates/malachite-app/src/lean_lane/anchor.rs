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

//! The decide anchor: promote the lean block the certificate's EVM header
//! commits to.
//!
//! This is the one place where a lean-lane failure stops the chain, so the
//! whole loop — SYNCING polling, transient tolerance, the total deadline —
//! lives here behind a single call from `handlers::decided`.

use eyre::eyre;
use tracing::{debug, info, warn};

use arc_consensus_types::{BlockHash, Height};
use arc_eth_engine::lean_shim::{LeanAnchor, NewBlockStatus};
use arc_eth_engine::transient::is_transient;

/// The decide anchor's total wall-clock budget. Past it the height fails and
/// restarts, which is the right answer: the lane cannot advance without these
/// bytes and a silent wait would stall consensus invisibly.
pub(crate) const ANCHOR_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Anchor the decided lean block by commitment. The node promotes its staged
/// copy (staged at validation), applies a queued one, or fetches the bytes from
/// a peer node — all node-side. The CL only waits, transient-tolerant, up to
/// the deadline. Historic no-op: an already-canonical commitment answers VALID.
///
/// `deadline` is the TOTAL budget and it is a real bound: every shim call is
/// wrapped in the remaining slice of it. Without that, one call sitting in the
/// shim's own ~15s transport retry (times 30 attempts) makes "30s" a fiction —
/// the anchor could hold the decide path for minutes.
pub(crate) async fn anchor_lean_lane(
    shim: &impl LeanAnchor,
    commitment: BlockHash,
    height: Height,
    deadline: std::time::Duration,
) -> eyre::Result<()> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(150);
    let start = std::time::Instant::now();
    let mut polls = 0u64;
    loop {
        polls = polls.saturating_add(1);
        let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
            return Err(eyre!("lean lane: could not anchor {commitment} at height={height} within {deadline:?} ({polls} polls) — halting"));
        };
        let call = tokio::time::timeout(remaining, shim.anchor_by_commitment(commitment)).await;
        match call {
            Ok(Ok(NewBlockStatus::Valid(c))) if c == commitment => {
                if polls > 2 {
                    info!(
                        "🪶 Lean lane anchored at height {height} after {polls} polls in {:?}",
                        start.elapsed()
                    );
                } else {
                    debug!(
                        "🪶 Lean lane anchored at decide in {:?} (height {height})",
                        start.elapsed()
                    );
                }
                return Ok(());
            }
            Ok(Ok(NewBlockStatus::Valid(c))) => {
                return Err(eyre!("lean lane: node answered commitment {c} for anchor {commitment} at height={height} — halting"));
            }
            Ok(Ok(NewBlockStatus::Syncing)) => tokio::time::sleep(POLL).await,
            Ok(Err(e)) if is_transient(&e) => {
                warn!("lean lane: node unreachable at anchor ({e:#}); waiting");
                tokio::time::sleep(POLL).await;
            }
            Ok(Err(e)) => return Err(e.wrap_err("lean lane: newBlock{commitment} failed")),
            // A call that outran the budget is the node being slow, not wrong:
            // treat it as transient and let the deadline check above end the
            // loop on the next turn.
            Err(_) => {
                warn!("lean lane: anchor call for {commitment} outran the remaining budget");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arc_consensus_types::B256;
    use eyre::eyre;

    use arc_eth_engine::lean_shim::MockLeanAnchor;
    use arc_eth_engine::transient::TransientDependencyError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    const TEST_DEADLINE: Duration = Duration::from_secs(5);

    fn anchor_commitment() -> B256 {
        B256::repeat_byte(0x5c)
    }

    /// A node with the block already staged: one call, straight through.
    #[tokio::test]
    async fn anchor_promotes_on_the_first_answer() {
        let commitment = anchor_commitment();
        let mut shim = MockLeanAnchor::new();
        shim.expect_anchor_by_commitment()
            .withf(move |c| *c == commitment)
            .times(1)
            .returning(move |_| Ok(NewBlockStatus::Valid(commitment)));

        anchor_lean_lane(&shim, commitment, Height::new(7), TEST_DEADLINE)
            .await
            .expect("a staged block anchors on the first call");
    }

    /// SYNCING is "wait and ask again", not failure: the node is backfilling
    /// itself. The loop must poll until it answers VALID.
    #[tokio::test]
    async fn anchor_polls_through_syncing_until_valid() {
        let commitment = anchor_commitment();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);

        let mut shim = MockLeanAnchor::new();
        shim.expect_anchor_by_commitment().returning(move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) < 2 {
                Ok(NewBlockStatus::Syncing)
            } else {
                Ok(NewBlockStatus::Valid(commitment))
            }
        });

        anchor_lean_lane(&shim, commitment, Height::new(7), TEST_DEADLINE)
            .await
            .expect("the anchor must survive a node that is still syncing");

        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "expected at least two SYNCING polls plus the VALID answer, got {}",
            calls.load(Ordering::SeqCst)
        );
    }

    /// A node that appends a DIFFERENT block than the certificate names has
    /// diverged: stop, never accept the answer.
    #[tokio::test]
    async fn anchor_fails_when_the_node_appends_another_commitment() {
        let commitment = anchor_commitment();
        let other = B256::repeat_byte(0x99);
        let mut shim = MockLeanAnchor::new();
        shim.expect_anchor_by_commitment()
            .times(1)
            .returning(move |_| Ok(NewBlockStatus::Valid(other)));

        let err = anchor_lean_lane(&shim, commitment, Height::new(7), TEST_DEADLINE)
            .await
            .expect_err("a different commitment must never pass as anchored");

        assert!(
            err.to_string().contains("node answered commitment"),
            "got: {err:#}"
        );
    }

    /// A lean node restarting mid-decide is AWAY, not wrong: keep waiting.
    #[tokio::test]
    async fn anchor_tolerates_a_transient_outage() {
        let commitment = anchor_commitment();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);

        let mut shim = MockLeanAnchor::new();
        shim.expect_anchor_by_commitment().returning(move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(TransientDependencyError::new("lean lane node", "restarting").into())
            } else {
                Ok(NewBlockStatus::Valid(commitment))
            }
        });

        anchor_lean_lane(&shim, commitment, Height::new(7), TEST_DEADLINE)
            .await
            .expect("a transient outage must not fail the anchor");

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A node that is wrong (not away) gets no waiting at all.
    #[tokio::test]
    async fn anchor_propagates_a_hard_error() {
        let mut shim = MockLeanAnchor::new();
        shim.expect_anchor_by_commitment()
            .times(1)
            .returning(|_| Err(eyre!("bad request")));

        let err = anchor_lean_lane(&shim, anchor_commitment(), Height::new(7), TEST_DEADLINE)
            .await
            .expect_err("a non-transient shim error must fail the anchor");

        assert!(err.to_string().contains("newBlock"), "got: {err:#}");
    }

    /// The deadline is a real bound: a node that never produces the block ends
    /// the height instead of holding the decide path open forever.
    #[tokio::test]
    async fn anchor_gives_up_at_the_deadline() {
        let mut shim = MockLeanAnchor::new();
        shim.expect_anchor_by_commitment()
            .returning(|_| Ok(NewBlockStatus::Syncing));

        let start = std::time::Instant::now();
        let err = anchor_lean_lane(
            &shim,
            anchor_commitment(),
            Height::new(7),
            Duration::from_millis(400),
        )
        .await
        .expect_err("a node that never catches up must not stall decide forever");

        assert!(err.to_string().contains("could not anchor"), "got: {err:#}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the anchor must return at its deadline, took {:?}",
            start.elapsed()
        );
    }

    /// A single shim call that hangs must not outlive the total budget — the
    /// whole point of bounding each call by the REMAINING deadline.
    #[tokio::test]
    async fn anchor_deadline_bounds_a_hanging_call() {
        // Hand-written rather than mocked: the mock's `returning` closure is
        // synchronous, and what is under test is a call that never returns.
        struct HangingAnchor;
        impl LeanAnchor for HangingAnchor {
            async fn anchor_by_commitment(&self, _c: B256) -> eyre::Result<NewBlockStatus> {
                // Longer than any anchor deadline: the shim's own transport
                // retry budget is ~15s, so an unbounded call could hold decide
                // far past the "30s" the loop advertises.
                tokio::time::sleep(Duration::from_secs(120)).await;
                Ok(NewBlockStatus::Syncing)
            }
        }

        let start = std::time::Instant::now();
        let err = anchor_lean_lane(
            &HangingAnchor,
            anchor_commitment(),
            Height::new(7),
            Duration::from_millis(300),
        )
        .await
        .expect_err("a hanging call must end at the deadline");

        assert!(err.to_string().contains("could not anchor"), "got: {err:#}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
    }
}
