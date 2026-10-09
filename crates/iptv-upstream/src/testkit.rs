//! Scripted HTTP transport and cipher factory for tests and benchmarks.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use bytes::Bytes;
use iptv_media::{testkit::XorCipher, PayloadCipher};
use iptv_wasm::AssetBundle;
use tokio::sync::Semaphore;

use crate::{
    error::Result,
    pipeline::CipherFactory,
    transport::{BoxFuture, HttpRequest, HttpResponse, HttpTransport, TransportError},
};

struct Route {
    needle: String,
    responses: VecDeque<std::result::Result<HttpResponse, TransportError>>,
    gate: Option<Arc<Semaphore>>,
}

/// A transport that answers from a script and records every request.
///
/// A route matches when the request URL contains its needle. Each route answers with its
/// queued responses in order and keeps repeating the last one. Unmatched requests get 404.
#[derive(Default)]
pub struct ScriptedTransport {
    routes: Mutex<Vec<Route>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    /// An empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a 200 response with `body` for URLs containing `needle`.
    pub fn ok(&self, needle: &str, body: impl Into<Bytes>) -> &Self {
        self.respond(
            needle,
            Ok(HttpResponse {
                status: 200,
                body: body.into(),
            }),
        )
    }

    /// Adds a response with the given status.
    pub fn status(&self, needle: &str, status: u16, body: &str) -> &Self {
        self.respond(
            needle,
            Ok(HttpResponse {
                status,
                body: Bytes::copy_from_slice(body.as_bytes()),
            }),
        )
    }

    /// Replaces everything queued for `needle` with one 200 response.
    pub fn set_ok(&self, needle: &str, body: impl Into<Bytes>) -> &Self {
        self.replace(
            needle,
            Ok(HttpResponse {
                status: 200,
                body: body.into(),
            }),
        )
    }

    /// Replaces everything queued for `needle` with one response of the given status.
    pub fn set_status(&self, needle: &str, status: u16, body: &str) -> &Self {
        self.replace(
            needle,
            Ok(HttpResponse {
                status,
                body: Bytes::copy_from_slice(body.as_bytes()),
            }),
        )
    }

    /// Replaces everything queued for `needle` with `response`; a gate is kept.
    pub fn replace(
        &self,
        needle: &str,
        response: std::result::Result<HttpResponse, TransportError>,
    ) -> &Self {
        let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(route) = routes.iter_mut().find(|route| route.needle == needle) {
            route.responses = VecDeque::from([response]);
        } else {
            routes.push(Route {
                needle: needle.to_string(),
                responses: VecDeque::from([response]),
                gate: None,
            });
        }
        self
    }

    /// Adds a queued response or failure for URLs containing `needle`.
    pub fn respond(
        &self,
        needle: &str,
        response: std::result::Result<HttpResponse, TransportError>,
    ) -> &Self {
        let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(route) = routes.iter_mut().find(|route| route.needle == needle) {
            route.responses.push_back(response);
        } else {
            routes.push(Route {
                needle: needle.to_string(),
                responses: VecDeque::from([response]),
                gate: None,
            });
        }
        self
    }

    /// Makes the route for `needle` wait for permits before answering.
    ///
    /// Add permits to the returned semaphore to let that many requests through.
    pub fn gate(&self, needle: &str) -> Arc<Semaphore> {
        let semaphore = Arc::new(Semaphore::new(0));
        let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(route) = routes.iter_mut().find(|route| route.needle == needle) {
            route.gate = Some(Arc::clone(&semaphore));
        } else {
            routes.push(Route {
                needle: needle.to_string(),
                responses: VecDeque::new(),
                gate: Some(Arc::clone(&semaphore)),
            });
        }
        semaphore
    }

    /// All requests seen so far.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Number of requests whose URL contains `needle`.
    pub fn count(&self, needle: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.url.contains(needle))
            .count()
    }
}

impl HttpTransport for ScriptedTransport {
    fn send(
        &self,
        request: HttpRequest,
    ) -> BoxFuture<'_, std::result::Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let (answer, gate) = {
                let mut routes =
                    self.routes.lock().unwrap_or_else(PoisonError::into_inner);
                match routes
                    .iter_mut()
                    .find(|route| request.url.contains(&route.needle))
                {
                    Some(route) => {
                        let answer = if route.responses.len() > 1 {
                            route.responses.pop_front()
                        } else {
                            route.responses.front().cloned()
                        };
                        (answer, route.gate.clone())
                    }
                    None => (None, None),
                }
            };
            if let Some(gate) = gate {
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
            }
            answer.unwrap_or_else(|| {
                Ok(HttpResponse {
                    status: 404,
                    body: Bytes::from_static(b"no scripted response"),
                })
            })
        })
    }
}

/// [`CipherFactory`] that hands out [`XorCipher`]s and counts how often it was asked.
#[derive(Default)]
pub struct XorCipherFactory {
    starts: AtomicUsize,
}

impl XorCipherFactory {
    /// A factory with no starts yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many ciphers were started.
    pub fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
}

impl CipherFactory for XorCipherFactory {
    fn start(&self, _livepid: &str) -> Result<Box<dyn PayloadCipher + Send>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(XorCipher::default()))
    }
}

/// The repository's runtime assets, loaded once per test process.
///
/// # Panics
/// Panics when the assets directory or its manifest is missing, which fails the test.
#[allow(clippy::expect_used)]
pub fn shared_assets() -> Arc<AssetBundle> {
    static SHARED: std::sync::OnceLock<Arc<AssetBundle>> = std::sync::OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        AssetBundle::load(&dir).expect("repository assets load")
    }))
}
