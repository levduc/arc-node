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

use bytes::Bytes;
use eyre::Context as _;
use sha3::Digest;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::streaming::CHUNK_SIZE;

use malachitebft_app_channel::app::streaming::{StreamContent, StreamId, StreamMessage};
use malachitebft_app_channel::app::types::core::{Round, Validity};
use malachitebft_app_channel::NetworkMsg;

use arc_consensus_types::lean::{decode_value, encode_value};
use arc_consensus_types::proposer::ProposerSelector;
use arc_consensus_types::signing::{Signature, SigningError, SigningProvider, VerificationResult};
use arc_consensus_types::{
    ArcContext, Height, ProposalData, ProposalFin, ProposalInit, ProposalPart, ProposalParts,
    Validator, ValidatorSet,
};

use arc_eth_engine::lean_shim::LeanNode;

use crate::block::ConsensusBlock;
use crate::lean_lane::fetch_lean_payload;

#[cfg_attr(test, mockall::automock(type Error = std::io::Error;))]
pub trait PublishProposalPart {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn publish_proposal_part(
        &self,
        msg: StreamMessage<ProposalPart>,
    ) -> Result<(), Self::Error>;
}

impl<T> PublishProposalPart for &'_ T
where
    T: PublishProposalPart,
{
    type Error = T::Error;

    async fn publish_proposal_part(
        &self,
        msg: StreamMessage<ProposalPart>,
    ) -> Result<(), Self::Error> {
        (*self).publish_proposal_part(msg).await
    }
}

impl PublishProposalPart for mpsc::Sender<NetworkMsg<ArcContext>> {
    type Error = mpsc::error::SendError<NetworkMsg<ArcContext>>;

    async fn publish_proposal_part(
        &self,
        msg: StreamMessage<ProposalPart>,
    ) -> Result<(), Self::Error> {
        self.send(NetworkMsg::PublishProposalPart(msg)).await
    }
}

/// Streams the given proposal parts over the network.
pub async fn stream_proposal(
    publish: impl PublishProposalPart,
    height: Height,
    round: Round,
    stream_messages: Vec<StreamMessage<ProposalPart>>,
) -> Result<(), eyre::Error> {
    for msg in stream_messages {
        info!(
            %height, %round, stream_id = %msg.stream_id, sequence = %msg.sequence,
            "Streaming proposal part: {:?}", msg.content
        );

        publish
            .publish_proposal_part(msg)
            .await
            .wrap_err("Failed to send proposal part to network")?;
    }

    Ok(())
}

/// Splits the given consensus block into proposal parts and prepares stream messages
/// for each part, along with the signature of the entire proposal.
pub async fn prepare_stream(
    stream_id: StreamId,
    signing_provider: &impl SigningProvider<ArcContext>,
    consensus_block: &ConsensusBlock,
    lean_lane: bool,
) -> eyre::Result<(Vec<StreamMessage<ProposalPart>>, Signature)> {
    let (parts, signature) = make_proposal_parts(signing_provider, consensus_block, lean_lane)
        .await
        .wrap_err("Failed to construct proposal parts")?;

    // +1 for the Fin message; Vec length <= isize::MAX, so +1 cannot overflow usize
    #[allow(clippy::arithmetic_side_effects)]
    let mut msgs = Vec::with_capacity(parts.len() + 1);
    let mut sequence = 0u64;

    for part in parts {
        let msg = StreamMessage::new(stream_id.clone(), sequence, StreamContent::Data(part));
        // Bounded by parts.len() which is bounded by MAX_MESSAGES_PER_STREAM
        #[allow(clippy::arithmetic_side_effects)]
        {
            sequence += 1;
        }
        msgs.push(msg);
    }

    msgs.push(StreamMessage::new(stream_id, sequence, StreamContent::Fin));

    Ok((msgs, signature))
}

/// Splits the given consensus block into proposal parts and computes the signature
/// for the entire proposal. `lean_lane` selects the value encoding (see
/// [`encode_value`]); with it off the data is the payload's SSZ bytes.
pub async fn make_proposal_parts(
    signing_provider: &impl SigningProvider<ArcContext>,
    block: &ConsensusBlock,
    lean_lane: bool,
) -> Result<(Vec<ProposalPart>, Signature), SigningError> {
    let mut hasher = sha3::Keccak256::new();
    let mut parts = Vec::new();

    let data = encode_value(
        &block.execution_payload,
        block.lean_payload.as_ref(),
        lean_lane,
    );

    // Init
    {
        parts.push(ProposalPart::Init(ProposalInit::new(
            block.height,
            block.round,
            block.valid_round,
            block.proposer,
        )));

        hasher.update(block.height.as_u64().to_be_bytes().as_slice());
        hasher.update(block.round.as_i64().to_be_bytes().as_slice());
    }

    // Data
    {
        for chunk in data.chunks(CHUNK_SIZE) {
            let chunk_data = ProposalData::new(Bytes::copy_from_slice(chunk));
            parts.push(ProposalPart::Data(chunk_data));
            hasher.update(chunk);
        }
    }

    // Fin
    let signature = match &block.signature {
        Some(signature) => {
            // Use the existing signature if it exists (restreaming)
            *signature
        }
        None => {
            // We are streaming a new proposal, so we need to sign it
            let hash = hasher.finalize().to_vec();
            signing_provider.sign_bytes(&hash).await?
        }
    };

    parts.push(ProposalPart::Fin(ProposalFin::new(signature)));

    Ok((parts, signature))
}

/// Validates the proposal parts by checking the proposer and signature.
///
/// ## Important
/// This function assumes that the parts are for the current height
pub async fn validate_proposal_parts(
    parts: &ProposalParts,
    expected_proposer: &Validator,
    signing_provider: &impl SigningProvider<ArcContext>,
) -> bool {
    // Check that the parts are from the expected proposer
    if expected_proposer.address != parts.proposer() {
        warn!(
            parts.height = %parts.height(),
            parts.round = %parts.round(),
            parts.proposer = %parts.proposer(),
            expected_proposer = %expected_proposer.address,
            "Received proposal part from non-proposer, ignoring"
        );

        return false;
    }

    let fin = parts.fin();
    let hash = parts.hash();

    assert_eq!(
        expected_proposer.address,
        parts.proposer(),
        "Proposer address must match expected proposer"
    );

    // Check proposal parts signature
    // NOTE: `expected_proposer` is guaranteed to be the proposer of these parts
    let result = signing_provider
        .verify_signed_bytes(&hash, &fin.signature, &expected_proposer.public_key)
        .await;

    match result {
        Ok(VerificationResult::Valid) => true,

        Ok(VerificationResult::Invalid) => {
            warn!(
                parts.height = %parts.height(),
                parts.round = %parts.round(),
                parts.proposer = %parts.proposer(),
                parts.hash = %hex::encode(hash),
                parts.signature = %hex::encode(fin.signature.to_bytes()),
                "Received proposal parts with invalid signature, ignoring"
            );

            false
        }

        Err(error) => {
            warn!(
                parts.height = %parts.height(),
                parts.round = %parts.round(),
                parts.proposer = %parts.proposer(),
                parts.hash = %hex::encode(hash),
                parts.signature = %hex::encode(fin.signature.to_bytes()),
                %error,
                "Error verifying proposal parts signature, ignoring"
            );

            false
        }
    }
}

/// Resolves the expected proposer for a set of proposal parts.
///
/// Selection uses `parts.round()`, which the parts signature covers. Restreamed parts
/// carry the round and proposer the block was stored with, so the same round resolves
/// the original proposer.
pub fn resolve_expected_proposer<'a>(
    proposer_selector: &dyn ProposerSelector,
    validator_set: &'a ValidatorSet,
    parts: &ProposalParts,
) -> &'a Validator {
    proposer_selector.select_proposer(validator_set, parts.height(), parts.round())
}

/// Re-assemble a [`ConsensusBlock`] from its [`ProposalParts`], decoding the
/// data with the value encoding `lean_lane` selects.
pub fn assemble_block_from_parts(
    parts: &ProposalParts,
    lean_lane: bool,
) -> eyre::Result<ConsensusBlock> {
    // Calculate total size and allocate buffer
    let total_size = parts.data_size();
    let mut block_bytes = Vec::with_capacity(total_size);

    // Concatenate all chunks
    for part in parts.data() {
        block_bytes.extend_from_slice(&part.bytes);
    }

    // Convert the concatenated data vector into an execution payload
    let (execution_payload, lean_payload) = decode_value(&block_bytes, lean_lane)?;

    let consensus_block = ConsensusBlock {
        height: parts.height(),
        round: parts.round(),
        valid_round: parts.init().pol_round,
        proposer: parts.proposer(),
        validity: Validity::Valid,
        execution_payload,
        signature: Some(parts.fin().signature),
        lean_payload,
    };

    Ok(consensus_block)
}

/// Puts the lean bytes back on a block read from the undecided store, which
/// keeps only the EVM payload.
///
/// A restreamed proposal reuses the stored signature, which covers the value
/// as first framed, lean bytes included; re-framing without them produces
/// parts no receiver can verify. Returns `false` when the lean node cannot
/// produce the block the header commits to.
pub async fn rehydrate_lean_payload(
    block: &mut ConsensusBlock,
    lean: &dyn LeanNode,
) -> eyre::Result<bool> {
    if block.lean_payload.is_some() {
        return Ok(true);
    }
    let Some(commitment) = block.header_lean_commitment() else {
        return Ok(true);
    };
    block.lean_payload = fetch_lean_payload(lean, commitment).await?;
    Ok(block.lean_payload.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    use arc_consensus_types::proposer::RoundRobin;
    use arc_consensus_types::signing::SigningProvider;
    use arc_consensus_types::{Address, ProposalFin, ProposalInit, ValidatorSet};
    use arc_signer::local::{LocalSigningProvider, PrivateKey, PublicKey};

    fn make_validator_set(n: usize) -> (Vec<PrivateKey>, ValidatorSet) {
        let mut rng = rand::thread_rng();
        let keys: Vec<PrivateKey> = (0..n).map(|_| PrivateKey::generate(&mut rng)).collect();
        let validators: Vec<Validator> = keys
            .iter()
            .map(|k| Validator::new(k.public_key(), 1))
            .collect();
        (keys, ValidatorSet::new(validators))
    }

    /// Build minimal ProposalParts with the given init fields and sign with the given key.
    async fn make_signed_parts(
        height: Height,
        round: Round,
        pol_round: Round,
        proposer_pub: PublicKey,
        signing_key: &PrivateKey,
    ) -> ProposalParts {
        use sha3::Digest;

        let proposer = Address::from_public_key(&proposer_pub);
        let init = ProposalInit::new(height, round, pol_round, proposer);

        let mut hasher = sha3::Keccak256::new();
        hasher.update(height.as_u64().to_be_bytes());
        hasher.update(round.as_i64().to_be_bytes());
        let hash = hasher.finalize().to_vec();

        let provider = LocalSigningProvider::new(signing_key.clone());
        let signature = provider.sign_bytes(&hash).await.unwrap();

        ProposalParts::new(vec![
            ProposalPart::Init(init),
            ProposalPart::Fin(ProposalFin::new(signature)),
        ])
        .unwrap()
    }

    #[test]
    fn resolve_proposer_without_pol_round_uses_parts_round() {
        let selector = RoundRobin;
        let (_keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let round = Round::new(2);

        // Build minimal parts with pol_round = Nil
        let init = ProposalInit::new(
            height,
            round,
            Round::Nil,
            validator_set.get_by_index(0).unwrap().address,
        );
        let fin = ProposalFin::new(arc_consensus_types::signing::Signature::test());
        let parts =
            ProposalParts::new(vec![ProposalPart::Init(init), ProposalPart::Fin(fin)]).unwrap();

        let expected = resolve_expected_proposer(&selector, &validator_set, &parts);
        let round_proposer = selector.select_proposer(&validator_set, height, round);

        assert_eq!(expected.address, round_proposer.address);
    }

    #[test]
    fn resolve_proposer_with_pol_round_still_uses_parts_round() {
        let selector = RoundRobin;
        let (_keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let parts_round = Round::new(2);
        let pol_round = Round::new(0);

        let parts_round_proposer = selector.select_proposer(&validator_set, height, parts_round);
        let pol_round_proposer = selector.select_proposer(&validator_set, height, pol_round);

        // Ensure they differ so the test is meaningful
        assert_ne!(
            parts_round_proposer.address, pol_round_proposer.address,
            "Test requires different proposers for parts_round and pol_round"
        );

        // Parts with non-Nil pol_round
        let init = ProposalInit::new(height, parts_round, pol_round, pol_round_proposer.address);
        let fin = ProposalFin::new(arc_consensus_types::signing::Signature::test());
        let parts =
            ProposalParts::new(vec![ProposalPart::Init(init), ProposalPart::Fin(fin)]).unwrap();

        let expected = resolve_expected_proposer(&selector, &validator_set, &parts);

        // Always resolves to the parts.round() proposer, regardless of pol_round
        assert_eq!(expected.address, parts_round_proposer.address);
        assert_ne!(expected.address, pol_round_proposer.address);
    }

    /// Parts signed by the proposer for parts.round() pass validation.
    #[tokio::test]
    async fn proposal_parts_from_correct_proposer_pass_validation() {
        let selector = RoundRobin;
        let (keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let round = Round::new(2);

        let expected_proposer = selector.select_proposer(&validator_set, height, round);
        let signing_key = keys
            .iter()
            .find(|k| Address::from_public_key(&k.public_key()) == expected_proposer.address)
            .unwrap();

        let parts = make_signed_parts(
            height,
            round,
            Round::Nil,
            signing_key.public_key(),
            signing_key,
        )
        .await;

        let resolved = resolve_expected_proposer(&selector, &validator_set, &parts);
        let provider = LocalSigningProvider::new(signing_key.clone());
        assert!(validate_proposal_parts(&parts, resolved, &provider).await);
    }

    /// Parts carrying a non-Nil pol_round but signed by the pol_round proposer
    /// rather than the parts.round() proposer fail validation.
    #[tokio::test]
    async fn proposal_parts_signed_by_non_proposer_fail_validation() {
        let selector = RoundRobin;
        let (keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let pol_round = Round::new(0);
        let parts_round = Round::new(2);

        let pol_round_proposer = selector.select_proposer(&validator_set, height, pol_round);
        let parts_round_proposer = selector.select_proposer(&validator_set, height, parts_round);

        assert_ne!(pol_round_proposer.address, parts_round_proposer.address);

        let signing_key = keys
            .iter()
            .find(|k| Address::from_public_key(&k.public_key()) == pol_round_proposer.address)
            .unwrap();

        // Parts with round=parts_round, pol_round set, signed by pol_round proposer
        let parts = make_signed_parts(
            height,
            parts_round,
            pol_round,
            signing_key.public_key(),
            signing_key,
        )
        .await;

        // resolve_expected_proposer uses parts_round, not pol_round
        let resolved = resolve_expected_proposer(&selector, &validator_set, &parts);
        assert_eq!(resolved.address, parts_round_proposer.address);

        // Validation fails: parts.proposer() is pol_round proposer, but expected is parts_round proposer
        let provider = LocalSigningProvider::new(signing_key.clone());
        assert!(!validate_proposal_parts(&parts, resolved, &provider).await);
    }

    /// A block carrying a non-Nil valid_round validates when restreamed: the parts
    /// keep the round and proposer the block was stored with.
    #[tokio::test]
    async fn restreamed_parts_with_valid_round_pass_validation() {
        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use arbitrary::{Arbitrary, Unstructured};

        let mut u = Unstructured::new(&[0u8; 512]);
        let payload = ExecutionPayloadV3::arbitrary(&mut u).unwrap();

        let selector = RoundRobin;
        let (keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let round = Round::new(2);
        let valid_round = Round::new(0);

        let round_proposer = selector.select_proposer(&validator_set, height, round);
        let valid_round_proposer = selector.select_proposer(&validator_set, height, valid_round);

        assert_ne!(
            round_proposer.address, valid_round_proposer.address,
            "Test requires different proposers for round and valid_round"
        );

        let signing_key = keys
            .iter()
            .find(|k| Address::from_public_key(&k.public_key()) == round_proposer.address)
            .unwrap();
        let provider = LocalSigningProvider::new(signing_key.clone());

        let mut block = ConsensusBlock {
            height,
            round,
            valid_round,
            proposer: round_proposer.address,
            validity: Validity::Valid,
            execution_payload: payload,
            signature: None,
            lean_payload: None,
        };

        // Original stream signs the block
        let (_, signature) = make_proposal_parts(&provider, &block, false).await.unwrap();
        block.signature = Some(signature);

        // Restream reuses the stored signature, round and proposer
        let (raw_parts, _) = make_proposal_parts(&provider, &block, false).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        assert_eq!(parts.init().pol_round, valid_round);

        let resolved = resolve_expected_proposer(&selector, &validator_set, &parts);
        assert_eq!(resolved.address, round_proposer.address);
        assert!(validate_proposal_parts(&parts, resolved, &provider).await);
    }

    /// assemble_block_from_parts must preserve pol_round as valid_round.
    #[tokio::test]
    async fn assemble_block_preserves_valid_round_from_pol_round() {
        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use arbitrary::{Arbitrary, Unstructured};

        let mut u = Unstructured::new(&[0u8; 512]);
        let payload = ExecutionPayloadV3::arbitrary(&mut u).unwrap();

        let (keys, _) = make_validator_set(1);
        let signing_key = &keys[0];
        let proposer = Address::from_public_key(&signing_key.public_key());

        let pol_round = Round::new(1);

        let block = ConsensusBlock {
            height: Height::new(10),
            round: Round::new(3),
            valid_round: pol_round,
            proposer,
            validity: Validity::Valid,
            execution_payload: payload,
            signature: None,
            lean_payload: None,
        };

        let provider = LocalSigningProvider::new(signing_key.clone());
        let (raw_parts, _sig) = make_proposal_parts(&provider, &block, false).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        // Sanity: Init carries the pol_round we set
        assert_eq!(parts.init().pol_round, pol_round);

        let assembled = assemble_block_from_parts(&parts, false).unwrap();
        assert_eq!(
            assembled.valid_round, pol_round,
            "assemble_block_from_parts must propagate pol_round as valid_round"
        );
    }

    #[tokio::test]
    async fn assemble_block_preserves_nil_valid_round() {
        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use arbitrary::{Arbitrary, Unstructured};

        let mut u = Unstructured::new(&[0u8; 512]);
        let payload = ExecutionPayloadV3::arbitrary(&mut u).unwrap();

        let (keys, _) = make_validator_set(1);
        let signing_key = &keys[0];
        let proposer = Address::from_public_key(&signing_key.public_key());

        let block = ConsensusBlock {
            height: Height::new(5),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer,
            validity: Validity::Valid,
            execution_payload: payload,
            signature: None,
            lean_payload: None,
        };

        let provider = LocalSigningProvider::new(signing_key.clone());
        let (raw_parts, _sig) = make_proposal_parts(&provider, &block, false).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        assert_eq!(parts.init().pol_round, Round::Nil);

        let assembled = assemble_block_from_parts(&parts, false).unwrap();
        assert_eq!(assembled.valid_round, Round::Nil);
    }

    fn data_bytes(parts: &ProposalParts) -> Vec<u8> {
        parts.data().iter().flat_map(|d| d.bytes.to_vec()).collect()
    }

    /// Flag off, the proposal data is the payload's SSZ bytes, as upstream.
    #[tokio::test]
    async fn flag_off_proposal_data_is_the_stock_ssz_payload() {
        use ssz::Encode;

        let (keys, _) = make_validator_set(1);
        let provider = LocalSigningProvider::new(keys[0].clone());
        let block = crate::lean_lane::test_lane::block(7, alloy_primitives::B256::ZERO, None);

        let (raw_parts, _) = make_proposal_parts(&provider, &block, false).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        assert_eq!(data_bytes(&parts), block.execution_payload.as_ssz_bytes());
        assert_eq!(
            assemble_block_from_parts(&parts, false)
                .unwrap()
                .lean_payload,
            None
        );
    }

    /// A lean block streams and assembles back byte-identical, under the same
    /// voted EVM hash. The undecided store drops the lean bytes, so a stored
    /// row re-framed under its original signature does not verify until it is
    /// rehydrated from the lean node, after which it is byte-identical again.
    #[tokio::test]
    async fn a_lean_block_round_trips_and_a_stored_row_needs_rehydrating() {
        use crate::lean_lane::test_lane::{block, lean_head, test_lane};
        use arc_consensus_types::lean::LeanLanePayload;

        let lane = test_lane(lean_head(0), 1, vec![]);
        let bytes = lane.chain[&1].clone();
        let commitment = LeanLanePayload::new(bytes.clone()).unwrap().commitment();
        let (keys, validator_set) = make_validator_set(1);
        let provider = LocalSigningProvider::new(keys[0].clone());
        let proposer = Address::from_public_key(&keys[0].public_key());
        let mut built = ConsensusBlock {
            proposer,
            ..block(1, commitment, Some(bytes))
        };

        let (raw, signature) = make_proposal_parts(&provider, &built, true).await.unwrap();
        let original = ProposalParts::new(raw).unwrap();
        let assembled = assemble_block_from_parts(&original, true).unwrap();
        assert_eq!(assembled.lean_payload, built.lean_payload);
        assert_eq!(
            assembled.self_reported_block_hash(),
            built.self_reported_block_hash()
        );

        built.signature = Some(signature);
        let mut stored = ConsensusBlock {
            lean_payload: None,
            ..built.clone()
        };
        let verifies = |parts: &ProposalParts| {
            let expected = resolve_expected_proposer(&RoundRobin, &validator_set, parts).clone();
            let parts = parts.clone();
            let provider = provider.clone();
            async move { validate_proposal_parts(&parts, &expected, &provider).await }
        };

        let (raw, _) = make_proposal_parts(&provider, &stored, true).await.unwrap();
        let bare = ProposalParts::new(raw).unwrap();
        assert_ne!(bare.hash(), original.hash());
        assert!(!verifies(&bare).await);

        assert!(rehydrate_lean_payload(&mut stored, &lane).await.unwrap());
        let (raw, _) = make_proposal_parts(&provider, &stored, true).await.unwrap();
        let rehydrated = ProposalParts::new(raw).unwrap();
        assert_eq!(rehydrated.hash(), original.hash());
        assert!(verifies(&rehydrated).await);
    }
}
