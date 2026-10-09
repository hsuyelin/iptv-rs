//! Upstream signing, API flow control and the per-channel segment pipeline.
//!
//! Everything that talks to the network goes through [`HttpTransport`], and the stream
//! decryptor comes from a [`CipherFactory`], so the whole crate runs in tests without a
//! network or a WASM engine.

mod channel;
mod constants;
mod error;
mod flow;
mod live;
mod pipeline;
mod sdk;
mod secret;
mod sign;
mod transport;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use channel::Channel;
pub use constants::{ACTIVE_URL, USER_AGENT};
pub use error::{Result, UpstreamError};
pub use flow::{ApiFlowLimiter, FlowOptions, FlowSnapshot, FlowStats};
pub use live::{LiveClient, SourceCacheEntry};
pub use pipeline::{CipherFactory, CmgCipherFactory, MediaPipeline, PipelineConfig};
pub use sdk::{
    build_input, build_request_id, canonical_body_md5, fetch_openapi_token,
    sign_with_token, OpenapiToken, SdkHeaders, SdkState,
};
pub use secret::Secret;
pub use sign::{
    build_ckey, canonical_js_default_sorted_string, canonical_locale_sorted_string,
    canonical_sorted_string, generate_guid, js_int32_hash, md5_hex,
    md5_js_default_sorted_with_secret, md5_sorted_with_secret, random_string, CKeyInfo,
};
pub use transport::{
    BoxFuture, HttpRequest, HttpResponse, HttpTransport, Method, RequestBody,
    ReqwestTransport, TransportError, TransportErrorKind,
};
