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

//! Phase-1 builder separation, v1 (docs/deferred-exec-100k.md): the payment payload our
//! REMOTE builder pre-built for this node's next proposer turn.
//!
//! Kicked at decide time by the validator that RoundRobin selects as the next proposer
//! (chained behind the builder follow feed so ordering is deterministic), consumed by
//! `get_value` only when every attribute matches exactly. Any mismatch falls back to the
//! local build path — a miss can slow a height, never break one.

use std::sync::Arc;

use alloy_rpc_types_engine::ExecutionPayloadV3;
use tokio::sync::Mutex;

use arc_consensus_types::{Address, BlockHash};

/// A payment payload pre-built by the remote builder, with the attributes it was built for.
pub struct PrebuiltPayment {
    pub parent: BlockHash,
    pub timestamp: u64,
    pub fee_recipient: Address,
    pub payload: ExecutionPayloadV3,
}

/// Shared stash: the prebuilt candidates for our next proposer turn. Two entries in
/// practice — timestamps t0 and t0+1 (the dual-timestamp trick that kills the
/// second-rollover miss class by construction).
pub type PrebuiltSlot = Arc<Mutex<Vec<PrebuiltPayment>>>;

/// Trigger for the continuous refresher: set by decided on the NEXT proposer
/// only (the same node that feeds), carrying the exact head the builder is
/// about to have and the timestamp base. Event-driven — the refresher wakes on
/// notify instead of polling, because the decide->get_value gap (~0.3-0.5s) is
/// shorter than any polite polling cadence (v2.0's 500ms tick scored 0/1,077
/// hits). Only-next-proposer gating also keeps the builder link frugal (the
/// 4x-fetch lesson).
#[derive(Default)]
pub struct RefresherTrigger {
    /// (expected builder head after our feed, its timestamp, its block number).
    /// The number lets the refresher tell "feed still executing" (builder head
    /// BEHIND want -> keep polling) from "builder moved PAST our stale want"
    /// (someone else fed since -> bail immediately and clear). Without it the
    /// confirm loop burned its full 2s budget on every stale drift-wake, and a
    /// fresh trigger could queue behind that burn past get_value (the dominant
    /// isolated-ingress miss class: 0-40% hits, phase-dependent).
    pub expected: Mutex<Option<(BlockHash, u64, u64)>>,
    pub notify: tokio::sync::Notify,
}
pub type RefresherHandle = Arc<RefresherTrigger>;

/// Continuous builder refresher (v2 of the prebuild): instead of a one-shot kick
/// at decide with a PREDICTED timestamp (whose staleness under stretched rounds
/// was the dominant miss class — observed ts_needed up to prebuilt+6s), a loop
/// keeps the stash tracking REALITY: whenever the builder's head moves or the
/// wall clock drifts past the stashed candidates, rebuild (head, now) and
/// (head, now+1). The proposer then finds a matching payload whenever the
/// builder is current — no prediction involved. Fail-safe like everything in
/// this module: any error just leaves the stash stale and get_value builds
/// locally.
pub async fn run_refresher(
    builder: arc_eth_engine::engine::Engine,
    slot: PrebuiltSlot,
    trigger: RefresherHandle,
    fee_recipient: Address,
) {
    use tracing::{debug, info};
    info!("🏗️ builder refresher: event-driven prebuild loop starting");
    loop {
        // Wake on the decide-time trigger, or every 500ms for drift repair
        // (clock rollover while waiting for get_value on a stretched round).
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            trigger.notify.notified(),
        )
        .await;
        let Some((want_head, head_ts, want_number)) = *trigger.expected.lock().await else {
            continue;
        };
        // Wait (briefly) for the builder to have executed our feed: poll its
        // head until it matches what we just fed. The feed runs concurrently.
        // If the builder's head is already PAST our want (another validator fed
        // a newer height since), the want is stale forever -- bail on the first
        // poll and CLEAR it, so a fresh trigger is never queued behind a 2s
        // burn against a head the builder will not show again.
        let mut head = None;
        let mut stale = false;
        for _ in 0..40 {
            match builder.eth.get_block_by_number("latest").await {
                Ok(Some(h)) if h.block_hash == want_head => {
                    head = Some(h);
                    break;
                }
                Ok(Some(h)) if h.block_number > want_number => {
                    stale = true;
                    break;
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        let Some(head) = head else {
            debug!(
                "builder refresher: fed head {want_head} not reached (stale={stale}); clearing"
            );
            *trigger.expected.lock().await = None;
            continue;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let t0 = head_ts.max(now);
        let wanted = [t0, t0 + 1];
        let missing: Vec<u64> = {
            let stash = slot.lock().await;
            wanted
                .iter()
                .copied()
                .filter(|ts| {
                    !stash.iter().any(|p| {
                        p.parent == head.block_hash
                            && p.timestamp == *ts
                            && p.fee_recipient == fee_recipient
                    })
                })
                .collect()
        };
        if missing.is_empty() {
            continue;
        }
        // Build the wanted timestamps CONCURRENTLY. Sequentially, two deadline-bounded
        // builds (~150ms each) plus feed/confirm/fetch total ~550ms — just past a
        // 512ms height, so every stash landed ~40ms after get_value and hit rate at
        // 2 blk/s was ZERO (measured; at the degraded ~1.5s cadence the same code
        // hit 22%). Concurrent builds cut the cycle to ~350ms.
        let results = futures::future::join_all(
            missing
                .iter()
                .map(|ts| builder.generate_block(&head, *ts, &fee_recipient)),
        )
        .await;
        let mut built: Vec<PrebuiltPayment> = Vec::new();
        for (ts, r) in missing.iter().zip(results) {
            match r {
                Ok(payload) => built.push(PrebuiltPayment {
                    parent: head.block_hash,
                    timestamp: *ts,
                    fee_recipient,
                    payload,
                }),
                Err(e) => {
                    debug!("builder refresher: build for ts {ts} failed: {e:#}");
                }
            }
        }
        if built.is_empty() {
            continue;
        }
        let mut stash = slot.lock().await;
        stash.retain(|p| p.parent == head.block_hash && p.timestamp >= t0);
        stash.extend(built);
        debug!(
            "builder refresher: stash has {} candidates for head {}",
            stash.len(),
            head.block_hash
        );
    }
}
