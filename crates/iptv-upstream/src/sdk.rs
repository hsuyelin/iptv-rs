use std::time::{SystemTime, UNIX_EPOCH};

use iptv_wasm::{KeygenInput, KeygenSigner};
use serde::Deserialize;

use crate::{
    constants::{ACTIVE_URL, USER_AGENT, YSPAPPID},
    error::{body_head, Result, UpstreamError},
    secret::Secret,
    sign::{canonical_sorted_string, md5_hex, random_string},
    transport::{HttpRequest, HttpTransport},
};

const TOKEN_ENDPOINT: &str = "https://h5access.yangshipin.cn/web/open/token";
const VAPPID: &str = "59306155";
const VSECRET: &str = "b42702bf7309a179d102f3d51b1add2fda0bc7ada64cb801";
const REQUEST_ID_PREFIX: &str = "999999";

/// State the signer reads while producing a signature.
#[derive(Debug, Clone)]
pub struct SdkState {
    /// Client guid.
    pub guid: String,
    /// Access token.
    pub token: Secret,
    /// Signing input string.
    pub input: String,
    /// Timestamp in milliseconds.
    pub ts: String,
}

impl SdkState {
    /// State with the current time.
    pub fn new(guid: impl Into<String>, token: Secret, input: impl Into<String>) -> Self {
        Self {
            guid: guid.into(),
            token,
            input: input.into(),
            ts: now_ms_string(),
        }
    }

    fn keygen_input(&self) -> KeygenInput {
        KeygenInput {
            guid: self.guid.clone(),
            token: self.token.expose().to_string(),
            app_id: YSPAPPID.to_string(),
            input: self.input.clone(),
            ts: self.ts.clone(),
            version: "v1".to_string(),
            host: "www.yangshipin.cn".to_string(),
            protocol: "https:".to_string(),
        }
    }
}

/// Headers produced by [`sign_with_token`].
#[derive(Debug, Clone)]
pub struct SdkHeaders {
    /// First segment of the input (the body digest).
    pub yspsdkinput: String,
    /// Signature header value.
    pub yspsdksign: Secret,
    /// Request sequence id.
    pub seq_id: u32,
    /// Request id.
    pub request_id: String,
}

/// A token granted by the open API.
#[derive(Debug, Clone)]
pub struct OpenapiToken {
    /// The token.
    pub token: Secret,
    /// Timestamp used for the challenge.
    pub ts: String,
    /// Seconds until expiry, when reported.
    pub expire: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenapiTokenResponse {
    #[serde(default)]
    data: Option<OpenapiTokenData>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    expire: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenapiTokenData {
    #[serde(default)]
    token: String,
    #[serde(default)]
    expire: Option<u64>,
}

/// MD5 of the canonical `k=v&...` form of a request body.
pub fn canonical_body_md5<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    md5_hex(canonical_sorted_string(pairs))
}

/// The signing input: `{body_md5}-{guid}-{seq_id}-{request_id}`.
pub fn build_input(body_md5: &str, guid: &str, seq_id: u32, request_id: &str) -> String {
    format!("{body_md5}-{guid}-{seq_id}-{request_id}")
}

/// A fresh request id.
pub fn build_request_id() -> String {
    format!(
        "{REQUEST_ID_PREFIX}{}{}",
        random_string(10),
        now_ms_string()
    )
}

/// Requests an access token for `guid`.
///
/// # Errors
/// Returns [`UpstreamError`] when the signer, the HTTP call or the response check fails.
pub async fn fetch_openapi_token(
    transport: &dyn HttpTransport,
    keygen: &KeygenSigner,
    guid: &str,
) -> Result<OpenapiToken> {
    let state = SdkState::new(guid, Secret::default(), "");
    let rnd = keygen.token_rnd(state.keygen_input())?;
    let url = url::Url::parse_with_params(
        TOKEN_ENDPOINT,
        [
            ("yspappid", YSPAPPID),
            ("guid", guid),
            ("vappid", VAPPID),
            ("vsecret", VSECRET),
            ("raw", "1"),
            ("version", "v1"),
            ("ts", state.ts.as_str()),
            ("rnd", rnd.as_str()),
        ],
    )
    .map_err(|error| UpstreamError::InvalidRequest(error.to_string()))?;
    let request = HttpRequest::get(
        url.to_string(),
        vec![
            ("accept".into(), "application/json, text/plain, */*".into()),
            ("origin".into(), ACTIVE_URL.into()),
            ("referer".into(), format!("{ACTIVE_URL}/")),
            ("user-agent".into(), USER_AGENT.into()),
        ],
    );
    let response =
        transport
            .send(request)
            .await
            .map_err(|source| UpstreamError::Transport {
                what: "h5access token",
                source,
            })?;
    let text = response.text();
    let parsed: OpenapiTokenResponse =
        serde_json::from_str(&text).map_err(|source| UpstreamError::Parse {
            what: "h5access token",
            source,
            body: body_head(&text, 500),
        })?;
    let token = parsed
        .data
        .as_ref()
        .map(|data| data.token.clone())
        .filter(|value| !value.is_empty())
        .or(parsed.token)
        .unwrap_or_default();
    if !response.is_success() || token.is_empty() {
        return Err(UpstreamError::Status {
            what: "h5access token",
            status: response.status,
            // Never echo a body that may hold the token we failed to extract.
            body: if token.is_empty() {
                body_head(&text, 500)
            } else {
                "<redacted>".to_string()
            },
        });
    }
    let expire = parsed.data.and_then(|data| data.expire).or(parsed.expire);
    Ok(OpenapiToken {
        token: Secret::new(token),
        ts: state.ts,
        expire,
    })
}

/// Signs `state` with the keygen module.
///
/// # Errors
/// Returns [`UpstreamError::Wasm`] when the signer fails.
pub fn sign_with_token(
    keygen: &KeygenSigner,
    state: SdkState,
    seq_id: u32,
    request_id: impl Into<String>,
) -> Result<SdkHeaders> {
    let input = state.input.clone();
    let signature_hex = keygen.signature_hex(state.keygen_input())?;
    let yspsdkinput = input.split('-').next().unwrap_or_default().to_string();
    Ok(SdkHeaders {
        yspsdkinput,
        yspsdksign: Secret::new(format!("{signature_hex}-{input}")),
        seq_id,
        request_id: request_id.into(),
    })
}

fn now_ms_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}
