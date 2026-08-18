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
