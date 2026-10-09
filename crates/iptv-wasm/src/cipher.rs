use std::sync::Arc;

use iptv_media::{CipherError, PayloadCipher};

use crate::{assets::AssetBundle, cmg::CmgRuntime, error::Result};

/// A primed CMG instance bound to one media tag, usable as a [`PayloadCipher`].
pub struct CmgSession {
    runtime: CmgRuntime,
    media_tag_id: String,
    active_url: String,
    page_url: String,
}

impl CmgSession {
    /// Creates a runtime for `page_url` and primes it with `media_tag_id`.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when instantiation or priming fails.
    pub fn start(
        bundle: &Arc<AssetBundle>,
        page_url: &str,
        media_tag_id: String,
        active_url: String,
    ) -> Result<Self> {
        let mut runtime = bundle.new_cmg_runtime(page_url)?;
        runtime.prime(&media_tag_id)?;
        Ok(Self {
            runtime,
            media_tag_id,
            active_url,
            page_url: page_url.to_string(),
        })
    }

    /// Page URL this session was created for.
    pub fn page_url(&self) -> &str {
        &self.page_url
    }

    /// Media tag id used by this session.
    pub fn media_tag_id(&self) -> &str {
        &self.media_tag_id
    }

    /// Current player tag reported by the guest, for diagnostics.
    pub fn vmp_tag(&self) -> &str {
        self.runtime.vmp_tag()
    }
}

impl PayloadCipher for CmgSession {
    fn tick(&mut self) -> std::result::Result<(), CipherError> {
        self.runtime
            .update(&self.media_tag_id)
            .map(|_| ())
            .map_err(CipherError::new)
    }

    fn decode(&mut self, nal: &[u8]) -> std::result::Result<Vec<u8>, CipherError> {
        self.runtime
            .module_dec_live(&self.media_tag_id, nal, &self.active_url)
            .map_err(CipherError::new)
    }
}
