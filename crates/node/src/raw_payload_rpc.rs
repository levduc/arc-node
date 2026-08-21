//! `arc` RPC namespace: raw built-payload fetch for the remote builder-prebuild path.
//!
//! `engine_getPayload` returns JSON with 0x-hex transaction strings — 2x the raw
//! bytes plus field overhead (~3.6MB for one 300M-gas payload, ~7MB for the
//! refresher's dual-timestamp fetch). When the builder EL serves a CL on another
//! machine, that encoding tax is what blows the prebuild window. This method
//! serves the SAME built payload as SSZ -> gzip -> base64: SSZ removes the hex
//! doubling, gzip exploits the heavy structural redundancy across thousands of
//! same-shape transfers, base64 keeps it a JSON string (1.33x).
//!
//! Consumption semantics are identical to `engine_getPayload` (resolves the
//! payload job), so a prebuild cycle calls exactly one of the two.

use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV3, PayloadId};
use base64::Engine as _;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use reth_payload_builder::PayloadStore;
use reth_payload_primitives::PayloadTypes;
use ssz::Encode;
use std::io::Write;

#[rpc(server, namespace = "arc")]
pub trait ArcRawPayloadApi {
    /// Returns the built payload for `id` as base64(gzip(ssz(ExecutionPayloadV3))),
    /// or `null` if the id is unknown.
    #[method(name = "rawPayload")]
    async fn raw_payload(&self, id: PayloadId) -> RpcResult<Option<String>>;
}

pub struct ArcRawPayloadRpc<T: PayloadTypes> {
    store: PayloadStore<T>,
}

impl<T: PayloadTypes> ArcRawPayloadRpc<T> {
    pub const fn new(store: PayloadStore<T>) -> Self {
        Self { store }
    }
}

fn err(msg: String) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32000, msg, None::<()>)
}

#[async_trait]
impl<T> ArcRawPayloadApiServer for ArcRawPayloadRpc<T>
where
    T: PayloadTypes,
    T::BuiltPayload: TryInto<ExecutionPayloadEnvelopeV3>,
    <T::BuiltPayload as TryInto<ExecutionPayloadEnvelopeV3>>::Error: std::fmt::Display,
{
    async fn raw_payload(&self, id: PayloadId) -> RpcResult<Option<String>> {
        let Some(built) = self.store.resolve(id).await else {
            return Ok(None);
        };
        let built = built.map_err(|e| err(format!("payload {id} failed to build: {e}")))?;
        let envelope: ExecutionPayloadEnvelopeV3 = built
            .try_into()
            .map_err(|e| err(format!("payload {id} not convertible to V3: {e}")))?;
        let ssz_bytes = envelope.execution_payload.as_ssz_bytes();
        let mut gz = flate2::write::GzEncoder::new(
            Vec::with_capacity(ssz_bytes.len() / 2),
            flate2::Compression::fast(),
        );
        gz.write_all(&ssz_bytes)
            .and_then(|()| gz.finish())
            .map(|compressed| Some(base64::engine::general_purpose::STANDARD.encode(compressed)))
            .map_err(|e| err(format!("payload {id} compression failed: {e}")))
    }
}
