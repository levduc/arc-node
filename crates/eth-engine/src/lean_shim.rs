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

//! Client for a lean payment lane node (`ARC_PAYMENT_LEAN_LANE`).
//!
//! Not the Engine API: the lean node has no forkchoice and no payload ids. It
//! serves five idempotent JSON-RPC verbs with by-name params, and block bytes
//! travel as standard padded base64:
//!
//! - `arc_buildBlock{parentCommitment, number, timestampMs, budgetGas}`
//! - `arc_stageBlock{blockBytes}`
//! - `arc_newBlock{blockBytes}` or `arc_newBlock{commitment}`
//! - `arc_getHead{}`
//! - `arc_getBlockBytes{number}` or `arc_getBlockBytes{commitment}`
//!
//! Lag is never an error: the node answers `SYNCING` and heals itself.

use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use eyre::{eyre, Context};
use serde_json::{json, Value};

use arc_consensus_types::BlockHash;

/// Transport attempts per call. With [`RETRY_DELAY`] this covers a lean node
/// restart (~15 s) before the node is reported unreachable.
const ATTEMPTS: u32 = 30;
const RETRY_DELAY: Duration = Duration::from_millis(500);
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The lean node did not answer at all, even after the transport retries.
///
/// This is the one error the consensus layer treats as "away, not wrong": the
/// decide anchor keeps waiting through it. Every other error means the node
/// answered something unusable.
#[derive(Debug, thiserror::Error)]
#[error("lean node unreachable: {method} failed after {ATTEMPTS} attempts: {detail}")]
pub struct LeanNodeUnreachable {
    pub method: String,
    pub detail: String,
}

/// Whether `report` is, or wraps, a [`LeanNodeUnreachable`].
pub fn is_unreachable(report: &eyre::Report) -> bool {
    report
        .chain()
        .any(|e| e.downcast_ref::<LeanNodeUnreachable>().is_some())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeanHead {
    pub commitment: BlockHash,
    pub number: u64,
    pub timestamp_ms: u64,
}

/// Answer to `arc_newBlock`: appended (or already known) with this
/// commitment, or the node is behind and backfilling itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewBlockStatus {
    Valid(BlockHash),
    Syncing,
}

/// What the consensus layer needs from the lean lane: this validator's lean
/// node, plus the other validators' nodes as a catch-up source.
///
/// Callers never trust a returned commitment or bytes on their own: every
/// caller recomputes the commitment from the bytes.
#[async_trait]
pub trait LeanNode: Send + Sync {
    async fn get_head(&self) -> eyre::Result<LeanHead>;

    /// Builds, without appending, the block `number` on `parent` from the
    /// node's pool. Returns the claimed commitment and the block bytes.
    async fn build_block(
        &self,
        parent: BlockHash,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> eyre::Result<(BlockHash, Vec<u8>)>;

    /// Validates, executes and appends a block (idempotent by commitment).
    async fn new_block(&self, bytes: Vec<u8>) -> eyre::Result<NewBlockStatus>;

    /// Appends the block with this commitment, which the node resolves itself
    /// (staged, queued, or fetched from its peers).
    async fn new_block_by_commitment(&self, commitment: BlockHash) -> eyre::Result<NewBlockStatus>;

    /// Pre-executes a block without appending it, so that a later `new_block`
    /// of the same block only promotes it. Fire-and-forget: never awaited,
    /// failures are ignored.
    fn stage_block(&self, bytes: Vec<u8>);

    /// This node's canonical block `number`, if it has it.
    async fn get_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>>;

    /// A canonical, staged or queued block with this commitment, if the node has it.
    async fn get_block_bytes_by_commitment(
        &self,
        commitment: BlockHash,
    ) -> eyre::Result<Option<Vec<u8>>>;

    /// Number of peer lean nodes configured as a catch-up source.
    fn peer_count(&self) -> usize;

    /// Canonical block `number` from peer `peer`.
    async fn peer_block_bytes(&self, peer: usize, number: u64) -> eyre::Result<Option<Vec<u8>>>;
}

/// JSON-RPC client for a lean node.
#[derive(Clone, Debug)]
pub struct LeanShim {
    client: reqwest::Client,
    url: String,
    peers: Vec<LeanShim>,
}

impl LeanShim {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.into(),
            peers: Vec::new(),
        }
    }

    pub fn with_peers(mut self, peer_urls: Vec<String>) -> Self {
        self.peers = peer_urls.into_iter().map(LeanShim::new).collect();
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC call. Transport failures are retried (every verb is
    /// idempotent); past the retries the error is [`LeanNodeUnreachable`].
    async fn call(&self, method: &str, params: Value) -> eyre::Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut attempt: u32 = 0;
        let response = loop {
            let sent = self
                .client
                .post(&self.url)
                .json(&body)
                .timeout(CALL_TIMEOUT)
                .send()
                .await;
            match sent {
                Ok(response) => break response,
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    if attempt >= ATTEMPTS {
                        return Err(LeanNodeUnreachable {
                            method: method.to_owned(),
                            detail: format!("{e:#}"),
                        }
                        .into());
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        };
        let response: Value = response
            .json()
            .await
            .wrap_err_with(|| format!("lean shim: {method} response not JSON"))?;
        if let Some(err) = response.get("error") {
            return Err(eyre!("lean shim: {method} error: {err}"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| eyre!("lean shim: {method} response has no result"))
    }
}

#[async_trait]
impl LeanNode for LeanShim {
    async fn get_head(&self) -> eyre::Result<LeanHead> {
        let r = self.call("arc_getHead", json!({})).await?;
        let field = |name: &str| {
            r.get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| eyre!("lean shim: head missing {name}"))
        };
        Ok(LeanHead {
            commitment: parse_commitment(&r)?,
            number: field("number")?,
            timestamp_ms: field("timestampMs")?,
        })
    }

    async fn build_block(
        &self,
        parent: BlockHash,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> eyre::Result<(BlockHash, Vec<u8>)> {
        let params = json!({
            "parentCommitment": format!("{parent}"),
            "number": number,
            "timestampMs": timestamp_ms,
            "budgetGas": budget_gas,
        });
        let r = self.call("arc_buildBlock", params).await?;
        let bytes = parse_block_bytes("arc_buildBlock", &r)?
            .ok_or_else(|| eyre!("lean shim: arc_buildBlock missing blockBytes"))?;
        Ok((parse_commitment(&r)?, bytes))
    }

    async fn new_block(&self, bytes: Vec<u8>) -> eyre::Result<NewBlockStatus> {
        let params = json!({ "blockBytes": B64.encode(bytes) });
        parse_new_block_status(&self.call("arc_newBlock", params).await?)
    }

    async fn new_block_by_commitment(&self, commitment: BlockHash) -> eyre::Result<NewBlockStatus> {
        let params = json!({ "commitment": format!("{commitment}") });
        parse_new_block_status(&self.call("arc_newBlock", params).await?)
    }

    fn stage_block(&self, bytes: Vec<u8>) {
        let shim = self.clone();
        tokio::spawn(async move {
            let params = json!({ "blockBytes": B64.encode(bytes) });
            let _ = shim.call("arc_stageBlock", params).await;
        });
    }

    async fn get_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        let r = self
            .call("arc_getBlockBytes", json!({ "number": number }))
            .await?;
        parse_block_bytes("arc_getBlockBytes", &r)
    }

    async fn get_block_bytes_by_commitment(
        &self,
        commitment: BlockHash,
    ) -> eyre::Result<Option<Vec<u8>>> {
        let params = json!({ "commitment": format!("{commitment}") });
        let r = self.call("arc_getBlockBytes", params).await?;
        parse_block_bytes("arc_getBlockBytes", &r)
    }

    fn peer_count(&self) -> usize {
        self.peers.len()
    }

    async fn peer_block_bytes(&self, peer: usize, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        match self.peers.get(peer) {
            Some(p) => p.get_block_bytes(number).await,
            None => Ok(None),
        }
    }
}

fn parse_commitment(r: &Value) -> eyre::Result<BlockHash> {
    r.get("commitment")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("lean shim: missing commitment"))?
        .parse()
        .map_err(|e| eyre!("lean shim: bad commitment: {e}"))
}

/// `VALID` carries a commitment, `SYNCING` means wait and ask again; anything
/// else fails closed.
fn parse_new_block_status(r: &Value) -> eyre::Result<NewBlockStatus> {
    if r.get("commitment").is_some() {
        return Ok(NewBlockStatus::Valid(parse_commitment(r)?));
    }
    match r.get("status").and_then(Value::as_str) {
        Some("SYNCING") => Ok(NewBlockStatus::Syncing),
        other => Err(eyre!(
            "lean shim: arc_newBlock answered neither a commitment nor SYNCING (status={other:?})"
        )),
    }
}

/// An absent or null `blockBytes` means the node does not have the block.
fn parse_block_bytes(method: &str, r: &Value) -> eyre::Result<Option<Vec<u8>>> {
    match r.get("blockBytes") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => B64
            .decode(s)
            .map(Some)
            .wrap_err_with(|| format!("lean shim: {method} blockBytes bad base64")),
        Some(other) => Err(eyre!("lean shim: {method} unexpected blockBytes: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_block_status_reads_commitment_syncing_and_nothing_else() {
        let c = BlockHash::repeat_byte(0x5c);
        assert_eq!(
            parse_new_block_status(&json!({ "status": "VALID", "commitment": format!("{c}") }))
                .unwrap(),
            NewBlockStatus::Valid(c)
        );
        assert_eq!(
            parse_new_block_status(&json!({ "status": "SYNCING", "number": 3 })).unwrap(),
            NewBlockStatus::Syncing
        );
        for bad in [
            json!({}),
            json!({ "status": "VALID" }),
            json!({ "status": 1 }),
        ] {
            assert!(parse_new_block_status(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn block_bytes_distinguishes_absent_from_malformed() {
        let m = "arc_getBlockBytes";
        assert_eq!(parse_block_bytes(m, &json!({})).unwrap(), None);
        assert_eq!(
            parse_block_bytes(m, &json!({ "blockBytes": null })).unwrap(),
            None
        );
        assert_eq!(
            parse_block_bytes(m, &json!({ "blockBytes": "UAE=" })).unwrap(),
            Some(vec![0x50, 0x01])
        );
        assert!(parse_block_bytes(m, &json!({ "blockBytes": 7 })).is_err());
        assert!(parse_block_bytes(m, &json!({ "blockBytes": "!!!" })).is_err());
    }

    #[test]
    fn only_an_unreachable_node_is_classified_unreachable() {
        let away: eyre::Report = LeanNodeUnreachable {
            method: "arc_getHead".into(),
            detail: "connection refused".into(),
        }
        .into();
        assert!(is_unreachable(&away.wrap_err("validation")));
        assert!(!is_unreachable(&eyre!(
            "lean shim: arc_newBlock error: bad"
        )));
    }

    /// A node that is not listening exhausts the retries and reports itself
    /// unreachable, not as a protocol error.
    #[tokio::test(start_paused = true)]
    async fn a_node_that_never_answers_is_unreachable() {
        let shim = LeanShim::new("http://127.0.0.1:1");
        let err = shim
            .get_head()
            .await
            .expect_err("nothing listens on port 1");
        assert!(is_unreachable(&err), "{err:#}");
    }
}
