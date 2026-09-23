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

//! The one lean node test double: a local node whose head the feed advances,
//! peers with configurable latency, and a strict mode
//! in which any call panics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use alloy_primitives::B256;
use async_trait::async_trait;

use alloy_rpc_types_engine::ExecutionPayloadV3;
use arc_consensus_types::lean::{test_lean_block_bytes, LeanLanePayload};
use arc_consensus_types::{Address, Block, Height, Round};
use arc_eth_engine::lean_shim::{LeanHead, LeanNode, LeanNodeUnreachable, NewBlockStatus};
use malachitebft_app_channel::app::types::core::Validity;

use crate::block::ConsensusBlock;

/// An answer to a bytes request.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    Bytes(Vec<u8>),
    Missing,
    Unreachable,
}

pub(crate) struct TestLane {
    /// Any call panics: pins that a path never contacts the lean node.
    pub(crate) strict: bool,
    /// Per peer: answer latency, and whether it serves blocks.
    pub(crate) peers: Vec<(Duration, bool)>,
    /// The canonical chain, by number: served by peers and `get_block_bytes`.
    pub(crate) chain: HashMap<u64, Vec<u8>>,
    pub(crate) head: Mutex<LeanHead>,
    /// Answer to `get_block_bytes_by_commitment`; `None` looks up `chain`.
    pub(crate) by_commitment: Option<Answer>,
    /// Peers asked, in order.
    pub(crate) asked: Mutex<Vec<usize>>,
    /// `(parent, number, timestamp_ms, budget_gas)` of every build.
    pub(crate) builds: Mutex<Vec<(B256, u64, u64, u64)>>,
    pub(crate) staged: AtomicUsize,
    pub(crate) resolved: AtomicUsize,
}

impl TestLane {
    fn touch(&self, call: &str) {
        assert!(
            !self.strict,
            "the lean node must not be called here: {call}"
        );
    }
}

fn unreachable(method: &str) -> eyre::Report {
    LeanNodeUnreachable {
        method: method.to_owned(),
        detail: "test double".to_owned(),
    }
    .into()
}

#[async_trait]
impl LeanNode for TestLane {
    async fn get_head(&self) -> eyre::Result<LeanHead> {
        self.touch("get_head");
        Ok(*self.head.lock().unwrap())
    }

    async fn build_block(
        &self,
        parent: B256,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> eyre::Result<(B256, Vec<u8>)> {
        self.touch("build_block");
        let recorded = (parent, number, timestamp_ms, budget_gas);
        self.builds.lock().unwrap().push(recorded);
        let bytes = test_lean_block_bytes(parent, number, timestamp_ms, &[]);
        Ok((commitment_of_bytes(&bytes), bytes))
    }

    async fn new_block(&self, bytes: Vec<u8>) -> eyre::Result<NewBlockStatus> {
        self.touch("new_block");
        let block = LeanLanePayload::new(bytes)?;
        *self.head.lock().unwrap() = LeanHead {
            commitment: block.commitment(),
            number: block.decoded.number,
            timestamp_ms: block.decoded.timestamp_ms,
        };
        Ok(NewBlockStatus::Valid(block.commitment()))
    }

    async fn new_block_by_commitment(&self, _commitment: B256) -> eyre::Result<NewBlockStatus> {
        unimplemented!("the decide anchor is not exercised here")
    }

    fn stage_block(&self, _bytes: Vec<u8>) {
        self.touch("stage_block");
        self.staged.fetch_add(1, Ordering::SeqCst);
    }

    async fn get_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        self.touch("get_block_bytes");
        Ok(self.chain.get(&number).cloned())
    }

    async fn get_block_bytes_by_commitment(
        &self,
        commitment: B256,
    ) -> eyre::Result<Option<Vec<u8>>> {
        self.touch("get_block_bytes_by_commitment");
        self.resolved.fetch_add(1, Ordering::SeqCst);
        match &self.by_commitment {
            Some(Answer::Bytes(bytes)) => Ok(Some(bytes.clone())),
            Some(Answer::Missing) => Ok(None),
            Some(Answer::Unreachable) => Err(unreachable("arc_getBlockBytes")),
            None => Ok(self
                .chain
                .values()
                .find(|b| commitment_of_bytes(b) == commitment)
                .cloned()),
        }
    }

    fn peer_count(&self) -> usize {
        self.touch("peer_count");
        self.peers.len()
    }

    async fn peer_block_bytes(&self, peer: usize, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        self.touch("peer_block_bytes");
        let (delay, serves) = self.peers[peer];
        self.asked.lock().unwrap().push(peer);
        tokio::time::sleep(delay).await;
        Ok(serves.then(|| self.chain.get(&number).cloned()).flatten())
    }
}

pub(crate) fn lean_head(number: u64) -> LeanHead {
    LeanHead {
        commitment: B256::repeat_byte(0x11),
        number,
        timestamp_ms: number * 1000,
    }
}

fn commitment_of_bytes(bytes: &[u8]) -> B256 {
    LeanLanePayload::new(bytes.to_vec())
        .expect("test lean block decodes")
        .commitment()
}

pub(crate) fn commitment_of(chain: &HashMap<u64, Vec<u8>>, number: u64) -> B256 {
    commitment_of_bytes(&chain[&number])
}

/// A lane whose local head is `head`, with lean blocks `head+1 ..= last`
/// linked onto it in `chain`.
pub(crate) fn test_lane(head: LeanHead, last: u64, peers: Vec<(Duration, bool)>) -> TestLane {
    let mut chain = HashMap::new();
    let mut parent = head.commitment;
    for n in head.number + 1..=last {
        let bytes = test_lean_block_bytes(parent, n, n * 1000, &[]);
        parent = commitment_of_bytes(&bytes);
        chain.insert(n, bytes);
    }
    TestLane {
        strict: false,
        peers,
        chain,
        head: Mutex::new(head),
        by_commitment: None,
        asked: Mutex::new(Vec::new()),
        builds: Mutex::new(Vec::new()),
        staged: AtomicUsize::new(0),
        resolved: AtomicUsize::new(0),
    }
}

/// A lean node that panics on any call.
pub(crate) fn strict_lane() -> TestLane {
    TestLane {
        strict: true,
        ..test_lane(lean_head(0), 0, vec![])
    }
}

/// A block whose EVM header has timestamp `evm_ts` and `prev_randao` set to
/// `header`, carrying `lean` as framed lean bytes (or none).
pub(crate) fn block(evm_ts: u64, header: B256, lean: Option<Vec<u8>>) -> ConsensusBlock {
    let mut payload = ExecutionPayloadV3::from_block_unchecked(B256::ZERO, &Block::default());
    payload.payload_inner.payload_inner.timestamp = evm_ts;
    payload.payload_inner.payload_inner.prev_randao = header;
    ConsensusBlock {
        height: Height::new(1),
        round: Round::new(0),
        valid_round: Round::Nil,
        proposer: Address::new([0u8; 20]),
        validity: Validity::Valid,
        execution_payload: payload,
        signature: None,
        lean_payload: lean.map(|b| LeanLanePayload::new(b).unwrap()),
    }
}
