//! Client for the LEAN payment-lane node's CL shim (contract:
//! docs/lean-lane-integration.md; server: ~/reth-fork crates/lean-lane-node).
//!
//! NOT the Engine API: the lean lane has no Ethereum header, no forkchoice, no
//! payload IDs. Three verbs drive it: buildBlock (proposer), newBlock
//! (validate/decide/sync feed — idempotent by commitment), getHead (anchor);
//! plus getBlockBytes for serving value-sync. Params are by-name JSON objects;
//! block bytes travel base64 (standard alphabet, padded).

use base64::Engine as _;
use eyre::{eyre, Context};
use serde_json::{json, Value};

use arc_consensus_types::BlockHash;

#[derive(Clone, Debug)]
pub struct LeanShim {
    client: reqwest::Client,
    url: String,
    /// Other validators' lean node RPCs (ARC_PAYMENT_LEAN_PEER_RPCS) — the
    /// decide-time catch-up source when this node's lane is behind the
    /// certificate (missed round-1 proposals, restarts). Lean blocks are
    /// self-verifying (recomputed commitment chain + the certificate binding),
    /// so peers cannot forge.
    peers: Vec<LeanShim>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeanHead {
    pub commitment: BlockHash,
    pub number: u64,
    pub timestamp_ms: u64,
}

/// Outcome of feeding a block: appended (Valid) or the node is behind and
/// backfilling itself from its peers (Syncing — wait and re-poll the head).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewBlockStatus {
    Valid(BlockHash),
    Syncing,
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

    pub fn peers(&self) -> &[LeanShim] {
        &self.peers
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    async fn call(&self, method: &str, params: Value) -> eyre::Result<Value> {
        // Transport errors are retried: a lean-node restart (~10s snapshot
        // replay) must read as a brief stall, not an INVALID verdict — a
        // rejected certified value is re-proposed un-revalidated (valid-round
        // rule), so one transient outage otherwise deadlocks the height.
        // Every shim verb is idempotent (newBlock dedups by commitment).
        const ATTEMPTS: u32 = 30; // ~15s: covers a lean-node restart (snapshot replay ~10s)
        const DELAY: std::time::Duration = std::time::Duration::from_millis(500);
        let mut last_err = None;
        let mut sent = None;
        for attempt in 0..ATTEMPTS {
            match self
                .client
                .post(&self.url)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => {
                    sent = Some(resp);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < ATTEMPTS {
                        tokio::time::sleep(DELAY).await;
                    }
                }
            }
        }
        let response: Value = sent
            .ok_or_else(|| {
                // Past the retry budget the node is genuinely AWAY — mark it
                // transient so handlers skip / re-request instead of dying.
                eyre::Report::new(crate::transient::TransientDependencyError::new(
                    "lean lane node",
                    format!(
                        "lean shim: {method} request failed after {ATTEMPTS} attempts: {:#}",
                        last_err.expect("no response implies an error")
                    ),
                ))
            })?
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

    fn parse_commitment(v: &Value) -> eyre::Result<BlockHash> {
        v.get("commitment")
            .and_then(|c| c.as_str())
            .ok_or_else(|| eyre!("lean shim: missing commitment"))?
            .parse()
            .map_err(|e| eyre!("lean shim: bad commitment: {e}"))
    }

    /// Proposer: build (but do not append) the next block from the node's pool.
    /// Returns (commitment, canonical block bytes).
    pub async fn build_block(
        &self,
        parent_commitment: BlockHash,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> eyre::Result<(BlockHash, Vec<u8>)> {
        let r = self
            .call(
                "arc_buildBlock",
                json!({
                    "parentCommitment": format!("{parent_commitment}"),
                    "number": number,
                    "timestampMs": timestamp_ms,
                    "budgetGas": budget_gas,
                }),
            )
            .await?;
        let commitment = Self::parse_commitment(&r)?;
        let bytes = r
            .get("blockBytes")
            .and_then(|b| b.as_str())
            .ok_or_else(|| eyre!("lean shim: buildBlock missing blockBytes"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(bytes)
            .wrap_err("lean shim: buildBlock blockBytes bad base64")?;
        Ok((commitment, bytes))
    }

    /// Validate + execute + append (idempotent by commitment). The decide
    /// anchor, the vote-gap feed, and the sync feed all come through here.
    ///
    /// A node with self-backfill (--peers) answers an unknown-parent feed with
    /// {"status":"SYNCING"} and heals itself in the background — the caller
    /// waits and re-polls the head instead of treating it as failure (the
    /// Engine-API SYNCING semantic; an errored feed here used to become a
    /// "Decision failure, restarting height" loop that pinned cadence at
    /// 0.60 blk/s while the laggard role migrated between validators).
    pub async fn new_block(&self, block_bytes: &[u8]) -> eyre::Result<NewBlockStatus> {
        let r = self
            .call(
                "arc_newBlock",
                json!({
                    "blockBytes": base64::engine::general_purpose::STANDARD.encode(block_bytes),
                }),
            )
            .await?;
        if r.get("commitment").is_some() {
            return Ok(NewBlockStatus::Valid(Self::parse_commitment(&r)?));
        }
        match r.get("status").and_then(|s| s.as_str()) {
            Some("SYNCING") => Ok(NewBlockStatus::Syncing),
            other => Err(eyre!(
                "lean shim: newBlock response has neither commitment nor a \
                 known status (status={other:?})"
            )),
        }
    }

    /// Vote-gap execution (shim v1.3): ask the node to validate + execute the
    /// block into a STAGED entry without appending, so the decide-time
    /// new_block promotes instantly instead of executing on the critical path
    /// (~230ms/2.6MB block measured). Fire-and-forget semantics at call sites:
    /// staging is speculative — any failure (older node without the verb,
    /// SYNCING, transport) just means the anchor takes the full path.
    pub async fn stage_block(&self, block_bytes: &[u8]) -> eyre::Result<()> {
        let _ = self
            .call(
                "arc_stageBlock",
                json!({
                    "blockBytes": base64::engine::general_purpose::STANDARD.encode(block_bytes),
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn get_head(&self) -> eyre::Result<LeanHead> {
        let r = self.call("arc_getHead", json!({})).await?;
        Ok(LeanHead {
            commitment: Self::parse_commitment(&r)?,
            number: r
                .get("number")
                .and_then(|n| n.as_u64())
                .ok_or_else(|| eyre!("lean shim: head missing number"))?,
            timestamp_ms: r
                .get("timestampMs")
                .and_then(|n| n.as_u64())
                .ok_or_else(|| eyre!("lean shim: head missing timestampMs"))?,
        })
    }

    /// Canonical block bytes by number from the node's log (sync serving).
    pub async fn get_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        let r = self
            .call("arc_getBlockBytes", json!({ "number": number }))
            .await?;
        match r.get("blockBytes") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .wrap_err("lean shim: getBlockBytes bad base64")?,
            )),
            Some(other) => Err(eyre!("lean shim: getBlockBytes unexpected type: {other}")),
        }
    }
}
