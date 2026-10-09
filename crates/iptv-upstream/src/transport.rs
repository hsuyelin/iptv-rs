use std::{fmt, future::Future, pin::Pin, time::Duration};

use bytes::Bytes;

/// Boxed future returned by [`HttpTransport::send`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// GET
    Get,
    /// POST
    Post,
}

/// Request payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    /// No body.
    Empty,
    /// Pre-serialized JSON.
    Json(Vec<u8>),
    /// URL-encoded form pairs.
    Form(Vec<(String, String)>),
}

/// An outgoing request. Header values may be secret; this type does not print them.
#[derive(Clone)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Absolute URL.
    pub url: String,
    /// Header name and value pairs.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: RequestBody,
}

impl HttpRequest {
    /// A GET request with the given headers.
    pub fn get(url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers,
            body: RequestBody::Empty,
        }
    }

    /// Value of the first header named `name`, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url.split('?').next().unwrap_or_default())
            .field("headers", &self.headers.len())
            .finish()
    }
}

/// A fully buffered response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Body bytes.
    pub body: Bytes,
}

impl HttpResponse {
    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as lossy UTF-8 text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Why a request could not be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportErrorKind {
    /// The request timed out.
    Timeout,
    /// The connection could not be established.
    Connect,
    /// Any other failure.
    Other,
}

/// A transport-level failure.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct TransportError {
    /// Failure class.
    pub kind: TransportErrorKind,
    /// Human-readable cause without URLs' query strings.
    pub message: String,
}

impl TransportError {
    /// Builds an error of the given class.
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Sends HTTP requests. Implemented by [`ReqwestTransport`] and by test doubles.
pub trait HttpTransport: Send + Sync {
    /// Performs the request and buffers the whole response.
    fn send(
        &self,
        request: HttpRequest,
    ) -> BoxFuture<'_, Result<HttpResponse, TransportError>>;
}

/// Production transport backed by `reqwest`.
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Builds a client with the given user agent and request timeout.
    ///
    /// # Errors
    /// Returns [`TransportError`] when the TLS backend cannot be initialized.
    pub fn new(user_agent: &str, timeout: Duration) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(timeout)
            .build()
            .map_err(|error| classify(&error))?;
        Ok(Self { client })
    }
}

fn classify(error: &reqwest::Error) -> TransportError {
    let kind = if error.is_timeout() {
        TransportErrorKind::Timeout
    } else if error.is_connect() {
        TransportErrorKind::Connect
    } else {
        TransportErrorKind::Other
    };
    // `without_url` keeps query strings, which may hold tokens, out of messages.
    let message = error.to_string();
    TransportError::new(kind, message)
}

impl HttpTransport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
    ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let mut builder = match request.method {
                Method::Get => self.client.get(&request.url),
                Method::Post => self.client.post(&request.url),
            };
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            builder = match request.body {
                RequestBody::Empty => builder,
                RequestBody::Json(bytes) => builder.body(bytes),
                RequestBody::Form(pairs) => builder.form(&pairs),
            };
            let response = builder
                .send()
                .await
                .map_err(|error| classify(&error.without_url()))?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|error| classify(&error.without_url()))?;
            Ok(HttpResponse { status, body })
        })
    }
}
