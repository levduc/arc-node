// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
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

pub use arc_consensus_types::block::*;

/// Test-only payload builder shared by unit tests in this crate.
#[cfg(test)]
pub fn tests_payload_helper(
    seed: u8,
    txs: Vec<alloy_primitives::Bytes>,
) -> alloy_rpc_types_engine::ExecutionPayloadV3 {
    use alloy_primitives::{Bloom, B256, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};
    alloy_rpc_types_engine::ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: B256::repeat_byte(seed),
                fee_recipient: Default::default(),
                state_root: B256::repeat_byte(seed.wrapping_add(1)),
                receipts_root: B256::repeat_byte(seed.wrapping_add(2)),
                logs_bloom: Bloom::default(),
                prev_randao: B256::ZERO,
                block_number: seed as u64,
                gas_limit: 30_000_000,
                gas_used: 21_000,
                timestamp: 1_000 + seed as u64,
                extra_data: alloy_primitives::Bytes::default(),
                base_fee_per_gas: U256::from(1u64),
                block_hash: B256::repeat_byte(seed.wrapping_add(3)),
                transactions: txs,
            },
            withdrawals: vec![],
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    }
}
