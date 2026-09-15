//! Client for the LEAN payment-lane node's CL shim (contract:
//! docs/lean-lane-integration.md; server: the lean-lane repo's
//! `lean-lane-node`).
//!
//! NOT the Engine API: the lean lane has no Ethereum header, no forkchoice, no
//! payload IDs. Five verbs drive it: buildBlock (proposer), stageBlock
//! (vote-gap pre-execution), newBlock (decide anchor / sync feed — idempotent
//! by commitment), getHead (anchor), getBlockBytes (serving value-sync).
//! Params are by-name JSON objects; block bytes travel base64 (standard
//! alphabet, padded).

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

// ---------------------------------------------------------------------------
// Response parsing — ONE reading of each shim field.
//
// Every verb comes in two spellings (by-bytes and by-commitment), so each
// field used to be parsed twice with its own error text. These helpers are the
// single reading: the verbs differ only in the params they send.
// ---------------------------------------------------------------------------

/// Standard, padded base64 — the encoding the shim contract specifies for
/// block bytes in both directions.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

fn encode_block_bytes(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn decode_block_bytes(method: &str, s: &str) -> eyre::Result<Vec<u8>> {
    B64.decode(s)
        .wrap_err_with(|| format!("lean shim: {method} blockBytes bad base64"))
}

/// The `commitment` field, parsed as a hash. Never trusted on its own: every
/// caller recomputes the commitment from the bytes before believing it.
fn parse_commitment(v: &Value) -> eyre::Result<BlockHash> {
    v.get("commitment")
        .and_then(|c| c.as_str())
        .ok_or_else(|| eyre!("lean shim: missing commitment"))?
        .parse()
        .map_err(|e| eyre!("lean shim: bad commitment: {e}"))
}

/// An `arc_newBlock` reply: a commitment (appended) or `SYNCING` (the node is
/// backfilling itself — wait and ask again). Anything else fails closed.
fn parse_new_block_status(method: &str, r: &Value) -> eyre::Result<NewBlockStatus> {
    if r.get("commitment").is_some() {
        return Ok(NewBlockStatus::Valid(parse_commitment(r)?));
    }
    match r.get("status").and_then(|s| s.as_str()) {
        Some("SYNCING") => Ok(NewBlockStatus::Syncing),
        other => Err(eyre!(
            "lean shim: {method} response has neither commitment nor a \
             known status (status={other:?})"
        )),
    }
}

/// A `blockBytes` field that the node is allowed to omit ("I do not have it").
fn parse_optional_block_bytes(method: &str, r: &Value) -> eyre::Result<Option<Vec<u8>>> {
    match r.get("blockBytes") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(decode_block_bytes(method, s)?)),
        Some(other) => Err(eyre!("lean shim: {method} unexpected type: {other}")),
    }
}

/// A `blockBytes` field the node MUST supply.
fn parse_required_block_bytes(method: &str, r: &Value) -> eyre::Result<Vec<u8>> {
    let s = r
        .get("blockBytes")
        .and_then(|b| b.as_str())
        .ok_or_else(|| eyre!("lean shim: {method} missing blockBytes"))?;
    decode_block_bytes(method, s)
}

/// By-name params for the commitment-addressed verbs (v0.2).
fn commitment_params(commitment: BlockHash) -> Value {
    json!({ "commitment": format!("{commitment}") })
}

/// By-name params for the bytes-addressed verbs.
fn block_bytes_params(block_bytes: &[u8]) -> Value {
    json!({ "blockBytes": encode_block_bytes(block_bytes) })
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
                    if attempt.saturating_add(1) < ATTEMPTS {
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
        let commitment = parse_commitment(&r)?;
        let bytes = parse_required_block_bytes("buildBlock", &r)?;
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
            .call("arc_newBlock", block_bytes_params(block_bytes))
            .await?;
        parse_new_block_status("newBlock", &r)
    }

    /// Vote-gap execution (shim v1.3): ask the node to validate + execute the
    /// block into a STAGED entry without appending, so the decide-time
    /// new_block promotes instantly instead of executing on the critical path
    /// (~230ms/2.6MB block measured). Fire-and-forget semantics at call sites:
    /// staging is speculative — any failure (older node without the verb,
    /// SYNCING, transport) just means the anchor takes the full path.
    pub async fn stage_block(&self, block_bytes: &[u8]) -> eyre::Result<()> {
        let _ = self
            .call("arc_stageBlock", block_bytes_params(block_bytes))
            .await?;
        Ok(())
    }

    pub async fn get_head(&self) -> eyre::Result<LeanHead> {
        let r = self.call("arc_getHead", json!({})).await?;
        Ok(LeanHead {
            commitment: parse_commitment(&r)?,
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
        parse_optional_block_bytes("getBlockBytes", &r)
    }
}

impl LeanShim {
    /// v0.2: anchor by commitment. The node promotes its staged copy, applies a
    /// queued copy, or fetches the bytes from a peer; SYNCING means keep polling.
    pub async fn new_block_by_commitment(
        &self,
        commitment: BlockHash,
    ) -> eyre::Result<NewBlockStatus> {
        let r = self
            .call("arc_newBlock", commitment_params(commitment))
            .await?;
        parse_new_block_status("newBlock{commitment}", &r)
    }

    /// v0.2: canonical/staged/queued block bytes by commitment (sync serving).
    pub async fn get_block_bytes_by_commitment(
        &self,
        commitment: BlockHash,
    ) -> eyre::Result<Option<Vec<u8>>> {
        let r = self
            .call("arc_getBlockBytes", commitment_params(commitment))
            .await?;
        parse_optional_block_bytes("getBlockBytes{commitment}", &r)
    }
}

/// A freshly built lean block as the node returned it. The CL recomputes the
/// commitment from `bytes` before using it (never trust `commitment` alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeanBuilt {
    pub commitment: BlockHash,
    pub bytes: Vec<u8>,
}

/// The proposer's view of the lane: build the next block on `parent`.
/// Mockable so the payload pipeline can be unit-tested without a node.
// Same shape as `PayloadGenerator` (crates/malachite-app/src/payload.rs):
// used only within this workspace, so the `Send`-auto-trait caveat on
// `async fn` in public traits doesn't bite.
#[allow(async_fn_in_trait)]
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait LeanBuilder: Send + Sync {
    async fn build_lean_block(
        &self,
        parent: LeanHead,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> eyre::Result<LeanBuilt>;
}

impl LeanBuilder for LeanShim {
    async fn build_lean_block(
        &self,
        parent: LeanHead,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> eyre::Result<LeanBuilt> {
        let (commitment, bytes) = self
            .build_block(
                parent.commitment,
                parent.number.saturating_add(1),
                timestamp_ms,
                budget_gas,
            )
            .await?;
        Ok(LeanBuilt { commitment, bytes })
    }
}

/// "Can this node produce the lean block a header commits to?" — the one
/// question validation (a network block that arrives without its lean bytes)
/// and re-proposal (a store-loaded row, whose lean bytes were never stored)
/// both ask. Mockable so both paths are unit-testable without a lean node.
///
/// The answer is bytes, never a claim: every caller re-decodes them and
/// recomputes the commitment before believing the node.
// Same `Send`-auto-trait caveat as `LeanBuilder`: workspace-internal only.
#[allow(async_fn_in_trait)]
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait LeanBytesResolver: Send + Sync {
    async fn lean_bytes_by_commitment(
        &self,
        commitment: BlockHash,
    ) -> eyre::Result<Option<Vec<u8>>>;
}

impl LeanBytesResolver for LeanShim {
    async fn lean_bytes_by_commitment(
        &self,
        commitment: BlockHash,
    ) -> eyre::Result<Option<Vec<u8>>> {
        self.get_block_bytes_by_commitment(commitment).await
    }
}

/// The decide-time anchor: promote the lean block the certificate's EVM header
/// commits to. Mockable so the anchor loop (SYNCING polling, transient
/// tolerance, deadline) is unit-testable without a lean node.
// Same `Send`-auto-trait caveat as `LeanBuilder`: workspace-internal only.
#[allow(async_fn_in_trait)]
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait LeanAnchor: Send + Sync {
    async fn anchor_by_commitment(&self, commitment: BlockHash) -> eyre::Result<NewBlockStatus>;
}

impl LeanAnchor for LeanShim {
    async fn anchor_by_commitment(&self, commitment: BlockHash) -> eyre::Result<NewBlockStatus> {
        self.new_block_by_commitment(commitment).await
    }
}

/// Everything the validation-time catch-up touches: the LOCAL lean node, and
/// the peer lean nodes it pulls missing blocks from. Split out as a trait so
/// the budget rules — the per-peer slice, and "budget exhausted is a lag, not
/// a verdict" — are unit-testable against slow/fast peers without a node.
// Same `Send`-auto-trait caveat as `LeanBuilder`: workspace-internal only.
// Not `automock`ed: the tests need peers that are genuinely SLOW (awaiting a
// sleep inside the call), which mockall's `returning` (a ready value) cannot
// express; the test double lives next to the tests instead.
#[allow(async_fn_in_trait)]
pub trait LeanCatchup: Send + Sync {
    /// How many peer lean nodes are configured (`ARC_PAYMENT_LEAN_PEER_RPCS`).
    fn peer_count(&self) -> usize;
    /// Canonical bytes for lean block `number` from peer `peer`.
    async fn peer_block_bytes(&self, peer: usize, number: u64) -> eyre::Result<Option<Vec<u8>>>;
    /// Feed bytes to the LOCAL node (idempotent by commitment).
    async fn feed_local(&self, bytes: Vec<u8>) -> eyre::Result<NewBlockStatus>;
    /// The LOCAL node's head.
    async fn local_head(&self) -> eyre::Result<LeanHead>;
}

impl LeanCatchup for LeanShim {
    fn peer_count(&self) -> usize {
        self.peers.len()
    }

    async fn peer_block_bytes(&self, peer: usize, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        match self.peers.get(peer) {
            Some(p) => p.get_block_bytes(number).await,
            None => Ok(None),
        }
    }

    async fn feed_local(&self, bytes: Vec<u8>) -> eyre::Result<NewBlockStatus> {
        self.new_block(&bytes).await
    }

    async fn local_head(&self) -> eyre::Result<LeanHead> {
        self.get_head().await
    }
}

/// What the VOTE path asks of the lean lane beyond the catch-up: "is this
/// historic block ours?", and the speculative stage that keeps execution off
/// the decide anchor's critical path.
///
/// Split out on top of [`LeanCatchup`] for one reason: the verdict WIRING —
/// which lean failure abstains and which votes `Invalid` — is a safety rule
/// (an Invalid on a certified value sticks forever), and it was previously
/// only reachable with a live node. With this seam the whole of
/// `validate_consensus_block` runs against a test double.
// Same `Send`-auto-trait caveat as `LeanBuilder`: workspace-internal only.
#[allow(async_fn_in_trait)]
pub trait LeanValidation: LeanCatchup {
    /// Our OWN canonical bytes for lean block `number`, if we have it.
    async fn canonical_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>>;

    /// Stage a block for the decide anchor. Fire-and-forget by contract:
    /// staging is speculative, so it must never be awaited on the vote path.
    fn stage_detached(&self, bytes: Vec<u8>);
}

impl LeanValidation for LeanShim {
    async fn canonical_block_bytes(&self, number: u64) -> eyre::Result<Option<Vec<u8>>> {
        self.get_block_bytes(number).await
    }

    fn stage_detached(&self, bytes: Vec<u8>) {
        let shim = self.clone();
        tokio::spawn(async move {
            let _ = shim.stage_block(&bytes).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_params_are_by_name_hex() {
        let c = BlockHash::repeat_byte(0xab);
        let v = commitment_params(c);
        assert_eq!(v["commitment"].as_str().unwrap(), format!("{c}"));
        assert!(v.get("blockBytes").is_none());
    }

    #[test]
    fn block_bytes_params_are_by_name_padded_base64() {
        let v = block_bytes_params(&[0x50, 0x01]);
        assert_eq!(v["blockBytes"].as_str().unwrap(), "UAE=");
        assert!(v.get("commitment").is_none());
    }

    /// Both spellings of `arc_newBlock` read their reply through this one
    /// parser, so a commitment means appended, `SYNCING` means wait, and
    /// anything else fails closed rather than being read as success.
    #[test]
    fn new_block_status_reads_commitment_syncing_and_nothing_else() {
        let c = BlockHash::repeat_byte(0x5c);
        assert_eq!(
            parse_new_block_status("newBlock", &json!({ "commitment": format!("{c}") })).unwrap(),
            NewBlockStatus::Valid(c)
        );
        assert_eq!(
            parse_new_block_status("newBlock", &json!({ "status": "SYNCING" })).unwrap(),
            NewBlockStatus::Syncing
        );
        for bad in [
            json!({}),
            json!({ "status": "VALID" }),
            json!({ "status": 1 }),
        ] {
            assert!(parse_new_block_status("newBlock", &bad).is_err(), "{bad}");
        }
    }

    /// An omitted or null `blockBytes` is "I do not have it", not an error;
    /// a non-string is malformed; a string must be valid base64.
    #[test]
    fn optional_block_bytes_distinguishes_absent_from_malformed() {
        let m = "getBlockBytes";
        assert_eq!(parse_optional_block_bytes(m, &json!({})).unwrap(), None);
        assert_eq!(
            parse_optional_block_bytes(m, &json!({ "blockBytes": null })).unwrap(),
            None
        );
        assert_eq!(
            parse_optional_block_bytes(m, &json!({ "blockBytes": "UAE=" })).unwrap(),
            Some(vec![0x50, 0x01])
        );
        assert!(parse_optional_block_bytes(m, &json!({ "blockBytes": 7 })).is_err());
        assert!(parse_optional_block_bytes(m, &json!({ "blockBytes": "!!!" })).is_err());
    }

    /// `buildBlock` must supply the bytes: absent is a protocol error there,
    /// which is exactly what separates it from the optional reading above.
    #[test]
    fn required_block_bytes_rejects_an_absent_field() {
        let m = "buildBlock";
        assert_eq!(
            parse_required_block_bytes(m, &json!({ "blockBytes": "UAE=" })).unwrap(),
            vec![0x50, 0x01]
        );
        assert!(parse_required_block_bytes(m, &json!({})).is_err());
        assert!(parse_required_block_bytes(m, &json!({ "blockBytes": null })).is_err());
    }
}
