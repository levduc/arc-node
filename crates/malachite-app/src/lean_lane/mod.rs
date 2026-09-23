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

//! Consensus-layer logic of the lean payment lane (`ARC_PAYMENT_LEAN_LANE`).
//!
//! None of this runs with the lane off: handlers then hold no lean node and
//! every entry point here is skipped.
//!
//! The rule every function here keeps: only an observed protocol violation is
//! [`LeanVerdict::Invalid`]. Anything that is a property of this node (its lean
//! node is away or behind) is [`LeanVerdict::Abstain`], which yields no vote for
//! the round. An `Invalid` recorded against a value that later gets certified
//! sticks forever, because the valid-round rule re-proposes certified values
//! without re-validating them.

pub(crate) mod binding;
pub(crate) mod catchup;
#[cfg(test)]
pub(crate) mod test_lane;

use tracing::warn;

use arc_consensus_types::lean::LeanLanePayload;
use arc_consensus_types::BlockHash;
use arc_eth_engine::lean_shim::LeanNode;

use crate::metrics::app::LeanNoVerdictReason;

/// What the lean lane says about a block.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LeanVerdict {
    /// Nothing observably wrong with the lean section.
    Valid,
    /// An observed violation, with the reason to record for forensics.
    Invalid(String),
    /// This node cannot judge the block this round.
    Abstain(LeanNoVerdictReason, String),
}

/// Fetches the lean block `commitment` names from the local lean node.
///
/// `Ok(None)` when the node does not have it, or answers with bytes that do not
/// decode to that commitment. `Err` when the node could not be asked.
pub(crate) async fn fetch_lean_payload(
    node: &dyn LeanNode,
    commitment: BlockHash,
) -> eyre::Result<Option<LeanLanePayload>> {
    let Some(bytes) = node.get_block_bytes_by_commitment(commitment).await? else {
        return Ok(None);
    };
    match LeanLanePayload::new(bytes) {
        Ok(lane) if lane.commitment() == commitment => Ok(Some(lane)),
        Ok(lane) => {
            warn!(%commitment, answered = %lane.commitment(), "Lean node answered another block");
            Ok(None)
        }
        Err(e) => {
            warn!(%commitment, "Lean node's block failed strict decode: {e:#}");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_lane::{lean_head, test_lane, Answer};
    use super::*;
    use arc_consensus_types::lean::test_lean_block_bytes;

    #[tokio::test]
    async fn fetch_returns_only_the_block_the_commitment_names() {
        let bytes = test_lean_block_bytes(BlockHash::repeat_byte(1), 5, 0, &[]);
        let commitment = LeanLanePayload::new(bytes.clone()).unwrap().commitment();
        let other = test_lean_block_bytes(BlockHash::repeat_byte(2), 5, 0, &[]);

        let cases = [
            (Answer::Bytes(bytes.clone()), Some(Some(bytes))),
            (Answer::Bytes(other), Some(None)),
            (Answer::Bytes(vec![1, 2, 3]), Some(None)),
            (Answer::Missing, Some(None)),
            (Answer::Unreachable, None),
        ];
        for (answer, expected) in cases {
            let mut lane = test_lane(lean_head(0), 0, vec![]);
            lane.by_commitment = Some(answer);
            let got = fetch_lean_payload(&lane, commitment).await.ok();
            let got = got.map(|lane| lane.map(|l| l.bytes));
            assert_eq!(got, expected);
        }
    }
}
