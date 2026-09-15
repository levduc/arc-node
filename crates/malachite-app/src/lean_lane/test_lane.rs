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

//! The lean lane under test: one local node whose head the feed advances,
//! plus peers with configurable latency and coverage.
//!
//! Shared by `lean_lane::catchup`'s tests (the budget slice and the
//! lag/violation split) and `payload`'s tests (the verdict WIRING through
//! `validate_consensus_block`), so both drive the same double.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alloy_primitives::B256;
use arc_consensus_types::block::{test_lean_block_bytes as lean_block_bytes, LeanLanePayload};
use arc_eth_engine::lean_shim::{LeanCatchup, LeanHead, LeanValidation};

pub(crate) struct TestLane {
    /// Per peer: how long it takes to answer, and whether it has blocks.
    pub(crate) peers: Vec<(Duration, bool)>,
    /// The canonical chain the serving peers hand out, by number.
    pub(crate) chain: std::collections::HashMap<u64, Vec<u8>>,
    pub(crate) head: std::sync::Mutex<LeanHead>,
    /// Peers asked, in order — this is what pins the slice behaviour.
    pub(crate) asked: std::sync::Mutex<Vec<usize>>,
    /// Blocks handed to the speculative stage (vote path, fire-and-forget).
    pub(crate) staged: AtomicUsize,
}

impl LeanCatchup for TestLane {
    fn peer_count(&self) -> usize {
        self.peers.len()
    }

    async fn peer_block_bytes(&self, peer: usize, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        let (delay, serves) = self.peers[peer];
        self.asked.lock().expect("asked").push(peer);
        tokio::time::sleep(delay).await;
        Ok(serves.then(|| self.chain.get(&number).cloned()).flatten())
    }

    async fn feed_local(
        &self,
        bytes: Vec<u8>,
    ) -> eyre::Result<arc_eth_engine::lean_shim::NewBlockStatus> {
        let block = LeanLanePayload::new(bytes)?;
        *self.head.lock().expect("head") = LeanHead {
            commitment: block.commitment(),
            number: block.decoded.number,
            timestamp_ms: block.decoded.timestamp_ms,
        };
        Ok(arc_eth_engine::lean_shim::NewBlockStatus::Valid(
            block.commitment(),
        ))
    }

    async fn local_head(&self) -> eyre::Result<LeanHead> {
        Ok(*self.head.lock().expect("head"))
    }
}

impl LeanValidation for TestLane {
    async fn canonical_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        Ok(self.chain.get(&number).cloned())
    }

    fn stage_detached(&self, _bytes: Vec<u8>) {
        self.staged.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn lean_head(number: u64) -> LeanHead {
    LeanHead {
        commitment: B256::repeat_byte(0x11),
        number,
        timestamp_ms: number.saturating_mul(1000),
    }
}

/// Lean blocks `head.number + 1 ..= last`, linked onto `head`.
pub(crate) fn lean_chain(head: LeanHead, last: u64) -> std::collections::HashMap<u64, Vec<u8>> {
    let mut parent = head.commitment;
    let mut chain = std::collections::HashMap::new();
    for n in head.number.saturating_add(1)..=last {
        let bytes = lean_block_bytes(parent, n, n.saturating_mul(1000));
        parent = LeanLanePayload::new(bytes.clone())
            .expect("test lean block decodes")
            .commitment();
        chain.insert(n, bytes);
    }
    chain
}

pub(crate) fn commitment_of(chain: &std::collections::HashMap<u64, Vec<u8>>, number: u64) -> B256 {
    LeanLanePayload::new(chain[&number].clone())
        .expect("test lean block decodes")
        .commitment()
}

pub(crate) fn test_lane(head: LeanHead, last: u64, peers: Vec<(Duration, bool)>) -> TestLane {
    TestLane {
        peers,
        chain: lean_chain(head, last),
        head: std::sync::Mutex::new(head),
        asked: std::sync::Mutex::new(Vec::new()),
        staged: AtomicUsize::new(0),
    }
}
