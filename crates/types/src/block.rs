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

use alloy_rpc_types_engine::ExecutionPayloadV3;
use bytesize::ByteSize;
use sha3::{Digest, Keccak256};
use ssz::{Decode, Encode};

use malachitebft_app_channel::app::types::core::{CommitCertificate, Round, Validity};
use malachitebft_app_channel::app::types::{LocallyProposedValue, ProposedValue};

use crate::ssz::{SszBlock, SszSignature};
use crate::{signing::Signature, Address, ArcContext, BlockHash, Height, Value};

/// A block as seen by the consensus layer.
///
/// This includes the execution payload, the metadata required for consensus,
/// and the signature for its proposal parts.
/// Note that this is a block that has been proposed but not yet decided, that is,
/// consensus has not yet been reached on it. Therefore, it might not become the
/// next head of the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsensusBlock {
    pub height: Height,
    pub round: Round,
    pub valid_round: Round,
    pub proposer: Address,
    pub validity: Validity,
    pub execution_payload: ExecutionPayloadV3,
    pub signature: Option<Signature>,
    /// Payment-lane execution payload (second EL). `None` for single-EL blocks.
    pub payment_payload: Option<ExecutionPayloadV3>,
}

impl ConsensusBlock {
    /// Returns the EVM execution block hash.
    ///
    /// This is used for all execution-layer interactions (parent lookup,
    /// forkchoice, RPC) and is NOT the value consensus votes on when a
    /// payment lane is present — see [`ConsensusBlock::value_id`].
    pub fn block_hash(&self) -> BlockHash {
        self.execution_payload
            .payload_inner
            .payload_inner
            .block_hash
    }

    /// Returns the consensus commitment: the value BFT votes on and certifies.
    ///
    /// It binds BOTH lanes so an equivocating proposer cannot fork the payment
    /// lane under a valid EVM commit certificate. For single-EL blocks (no
    /// payment lane) this equals [`ConsensusBlock::block_hash`] byte-for-byte,
    /// so existing chains and the single-EL path are unaffected.
    pub fn value_id(&self) -> BlockHash {
        commit_lanes(self.block_hash(), self.payment_block_hash())
    }

    /// The payment-lane execution block hash, if a payment lane is present.
    pub fn payment_block_hash(&self) -> Option<BlockHash> {
        self.payment_payload
            .as_ref()
            .map(|p| p.payload_inner.payload_inner.block_hash)
    }

    /// Returns the size of the block in bytes when encoded using SSZ.
    pub fn size_bytes(&self) -> ByteSize {
        // TODO: Cache this
        ByteSize::b(block_as_ssz_data(self).ssz_bytes_len() as u64)
    }

    /// Returns the size of the execution payload in bytes when encoded using SSZ.
    pub fn payload_size(&self) -> ByteSize {
        ByteSize::b(self.execution_payload.ssz_bytes_len() as u64)
    }
}

impl From<&ConsensusBlock> for ProposedValue<ArcContext> {
    fn from(block: &ConsensusBlock) -> Self {
        ProposedValue {
            height: block.height,
            round: block.round,
            proposer: block.proposer,
            valid_round: block.valid_round,
            value: Value::new(block.value_id()),
            validity: block.validity,
        }
    }
}

impl From<&ConsensusBlock> for LocallyProposedValue<ArcContext> {
    fn from(block: &ConsensusBlock) -> Self {
        LocallyProposedValue {
            height: block.height,
            round: block.round,
            value: Value::new(block.value_id()),
        }
    }
}

/// Converts a ConsensusBlock into a tuple suitable for SSZ encoding
pub fn block_as_ssz_data(block: &ConsensusBlock) -> SszBlock<&'_ ExecutionPayloadV3> {
    (
        block.height.as_u64(),
        block.round.as_u32(),
        block.valid_round.as_u32(),
        block.proposer.to_alloy_address(),
        block.validity.is_valid(),
        &block.execution_payload,
        block.signature.map(SszSignature),
        block.payment_payload.as_ref(),
    )
}

/// Decided block, ie. an execution payload together with its commit certificate.
/// A decided block is a block for which consensus has been reached and therefore
/// it can't be removed from the chain.
#[derive(Clone, Debug)]
pub struct DecidedBlock {
    pub execution_payload: ExecutionPayloadV3,
    /// Payment-lane execution payload (second EL). `None` for single-EL blocks.
    pub payment_payload: Option<ExecutionPayloadV3>,
    pub certificate: CommitCertificate<ArcContext>,
}

impl DecidedBlock {
    /// Creates a new decided block from the lane payloads and a commit certificate.
    /// The commitment over both lanes must match the value id in the commit
    /// certificate (see [`ConsensusBlock::value_id`]).
    pub fn new(
        execution_payload: ExecutionPayloadV3,
        payment_payload: Option<ExecutionPayloadV3>,
        certificate: CommitCertificate<ArcContext>,
    ) -> Self {
        let evm_block_hash = execution_payload.payload_inner.payload_inner.block_hash;
        let payment_block_hash = payment_payload
            .as_ref()
            .map(|p| p.payload_inner.payload_inner.block_hash);
        let value_id = commit_lanes(evm_block_hash, payment_block_hash);
        let certificate_value_id = certificate.value_id.block_hash();

        assert_eq!(
            value_id, certificate_value_id,
            "Commitment over the execution payloads does not match the value id in the commit certificate"
        );

        Self {
            execution_payload,
            payment_payload,
            certificate,
        }
    }

    /// Reconstructs a decided block from persisted CL state.
    ///
    /// The CL decided store caches only the EVM payload; the authoritative
    /// commitment over both lanes is `certificate.value_id`, and the payment
    /// block is persisted canonically by the payment EL. Unlike
    /// [`DecidedBlock::new`], this does NOT recompute/verify the commitment,
    /// because the payment payload is not available from the CL store.
    pub fn from_stored_evm_only(
        execution_payload: ExecutionPayloadV3,
        certificate: CommitCertificate<ArcContext>,
    ) -> Self {
        Self {
            execution_payload,
            payment_payload: None,
            certificate,
        }
    }

    /// Returns the height at which the block was decided.
    pub fn height(&self) -> Height {
        self.certificate.height
    }
}

/// Computes the consensus commitment over both lanes.
///
/// When there is no payment lane the commitment is exactly the EVM block hash,
/// preserving backward compatibility with single-EL blocks. When a payment lane
/// is present the commitment is `keccak256(evm_block_hash ‖ payment_block_hash)`.
///
/// This is the single source of truth for the commitment: both
/// [`ConsensusBlock::value_id`] and [`DecidedBlock::new`] go through it so they
/// can never drift.
pub fn commit_lanes(evm_block_hash: BlockHash, payment_block_hash: Option<BlockHash>) -> BlockHash {
    match payment_block_hash {
        None => evm_block_hash,
        Some(payment_block_hash) => {
            let mut hasher = Keccak256::new();
            hasher.update(evm_block_hash.as_slice());
            hasher.update(payment_block_hash.as_slice());
            BlockHash::from_slice(&hasher.finalize())
        }
    }
}

/// Length-frames the two lane payloads into a single byte buffer:
/// `[u64-LE len(evm)] [evm SSZ] [payment SSZ (optional)]`.
///
/// The length prefix lets the decoder split the two lanes; an absent payment
/// lane produces no trailer. Shared by the proposal-streaming path and the
/// value-sync path so both produce and consume identical bytes.
pub fn frame_lanes(
    execution_payload: &ExecutionPayloadV3,
    payment_payload: Option<&ExecutionPayloadV3>,
) -> Vec<u8> {
    let evm = execution_payload.as_ssz_bytes();
    let mut buf = Vec::with_capacity(8 + evm.len());
    buf.extend_from_slice(&(evm.len() as u64).to_le_bytes());
    buf.extend_from_slice(&evm);
    if let Some(payment) = payment_payload {
        buf.extend_from_slice(&payment.as_ssz_bytes());
    }
    buf
}

/// Inverse of [`frame_lanes`]: splits a framed buffer back into the EVM payload
/// and the optional payment payload.
pub fn unframe_lanes(
    bytes: &[u8],
) -> eyre::Result<(ExecutionPayloadV3, Option<ExecutionPayloadV3>)> {
    if bytes.len() < 8 {
        return Err(eyre::eyre!("lane bytes too short to contain length prefix"));
    }
    let len_evm = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    let evm_end = 8usize
        .checked_add(len_evm)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| eyre::eyre!("invalid evm payload length prefix"))?;
    let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&bytes[8..evm_end])
        .map_err(|e| eyre::eyre!("Failed to decode execution payload: {e:?}"))?;
    let payment_payload = if bytes.len() > evm_end {
        Some(
            ExecutionPayloadV3::from_ssz_bytes(&bytes[evm_end..])
                .map_err(|e| eyre::eyre!("Failed to decode payment payload: {e:?}"))?,
        )
    } else {
        None
    };
    Ok((execution_payload, payment_payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Address, Height, Round};
    use alloy_primitives::{Bloom, Bytes as AlloyBytes, B256, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};
    use malachitebft_app_channel::app::types::core::Validity;

    /// Builds a payload whose block hash (and state root) is keyed by `seed`, so
    /// distinct seeds are distinguishable lanes.
    fn payload(seed: u8) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: alloy_primitives::Address::ZERO,
                    state_root: B256::repeat_byte(seed),
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: AlloyBytes::default(),
                    base_fee_per_gas: U256::from(1u64),
                    block_hash: B256::repeat_byte(seed),
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    fn block(evm: ExecutionPayloadV3, payment: Option<ExecutionPayloadV3>) -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            validity: Validity::Valid,
            execution_payload: evm,
            signature: None,
            payment_payload: payment,
        }
    }

    /// Single-EL blocks must commit to exactly the EVM block hash, so existing
    /// chains and the single-EL path are byte-for-byte unaffected.
    #[test]
    fn value_id_equals_block_hash_without_payment_lane() {
        let b = block(payload(0x11), None);
        assert_eq!(b.value_id(), b.block_hash());
    }

    /// With a payment lane, the commitment binds BOTH lanes and therefore differs
    /// from the bare EVM block hash.
    #[test]
    fn value_id_binds_payment_lane() {
        let evm = payload(0x11);
        let pay = payload(0x22);
        let b = block(evm, Some(pay));
        assert_ne!(
            b.value_id(),
            b.block_hash(),
            "value_id must not collapse to the EVM hash when a payment lane is present"
        );
    }

    /// The equivocation guard: two blocks with the SAME EVM lane but DIFFERENT
    /// payment lanes must produce DIFFERENT value_ids. Otherwise a proposer could
    /// fork the payment lane under one EVM commit certificate.
    #[test]
    fn value_id_changes_when_only_payment_lane_changes() {
        let evm = payload(0x11);
        let b1 = block(evm.clone(), Some(payload(0x22)));
        let b2 = block(evm.clone(), Some(payload(0x33)));
        assert_eq!(b1.block_hash(), b2.block_hash(), "test setup: EVM lane identical");
        assert_ne!(
            b1.payment_block_hash(),
            b2.payment_block_hash(),
            "test setup: payment lanes must differ"
        );
        assert_ne!(
            b1.value_id(),
            b2.value_id(),
            "same EVM lane + different payment lane must yield different commitments"
        );
    }

    /// `commit_lanes` is order-sensitive: swapping the two lane hashes yields a
    /// different commitment (guards against a symmetric-hash mistake).
    #[test]
    fn commit_lanes_is_order_sensitive() {
        let a = BlockHash::from_slice(&[0xAA; 32]);
        let b = BlockHash::from_slice(&[0xBB; 32]);
        assert_ne!(commit_lanes(a, Some(b)), commit_lanes(b, Some(a)));
    }

    /// A dual-lane block survives the value-sync framing round-trip with both
    /// lanes intact and the same value_id on both ends.
    #[test]
    fn frame_unframe_round_trips_both_lanes() {
        let evm = payload(0x11);
        let pay = payload(0x22);
        let framed = frame_lanes(&evm, Some(&pay));
        let (evm_out, pay_out) = unframe_lanes(&framed).unwrap();
        assert_eq!(evm_out, evm);
        assert_eq!(pay_out, Some(pay));
    }

    #[test]
    fn frame_unframe_round_trips_single_lane() {
        let evm = payload(0x11);
        let framed = frame_lanes(&evm, None);
        let (evm_out, pay_out) = unframe_lanes(&framed).unwrap();
        assert_eq!(evm_out, evm);
        assert_eq!(pay_out, None);
    }
}
