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

/// Gate for the continuous refresher: only the NEXT proposer's refresher may
/// build. Without this, all four validators' refreshers each pulled ~6MB
/// payloads from the builder continuously — saturating its link with fetches
/// for payloads three of them would never use (the fetch-side twin of the
/// 4x-feed lesson). Set by decided (which knows im_next), cleared on consume.
pub type RefresherActive = Arc<std::sync::atomic::AtomicBool>;

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
    active: RefresherActive,
    fee_recipient: Address,
) {
    use tracing::{debug, info};
    info!("🏗️ builder refresher: continuous prebuild loop starting");
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Only the next proposer builds (see RefresherActive).
        if !active.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        // Current builder head (its canonical payment chain, kept current by the
        // single-feeder decide feed).
        let head = match builder.eth.get_block_by_number("latest").await {
            Ok(Some(h)) => h,
            _ => continue,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let t0 = head.timestamp.max(now);
        let wanted = [t0, t0 + 1];
        // What's missing? (stash may hold them from the previous tick)
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
        let mut built: Vec<PrebuiltPayment> = Vec::new();
        for ts in &missing {
            match builder.generate_block(&head, *ts, &fee_recipient).await {
                Ok(payload) => built.push(PrebuiltPayment {
                    parent: head.block_hash,
                    timestamp: *ts,
                    fee_recipient,
                    payload,
                }),
                Err(e) => {
                    debug!("builder refresher: build for ts {ts} failed: {e:#}");
                    break;
                }
            }
        }
        if built.is_empty() {
            continue;
        }
        let mut stash = slot.lock().await;
        // Drop stale candidates (other parents, or timestamps now in the past),
        // keep still-valid ones, add the new builds.
        stash.retain(|p| p.parent == head.block_hash && p.timestamp >= t0);
        stash.extend(built);
        debug!(
            "builder refresher: stash now has {} candidates for head {}",
            stash.len(),
            head.block_hash
        );
    }
}
