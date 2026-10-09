/// Error reported by a [`PayloadCipher`] implementation.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CipherError(pub String);

impl CipherError {
    /// Builds an error from any displayable cause.
    pub fn new(cause: impl std::fmt::Display) -> Self {
        Self(cause.to_string())
    }
}

/// Decrypts H.264 NAL payloads on behalf of the media layer.
///
/// The cipher is stateful: the media layer calls [`PayloadCipher::tick`] once per NAL
/// before [`PayloadCipher::decode`], in stream order.
pub trait PayloadCipher {
    /// Advances the cipher state for the next NAL.
    ///
    /// # Errors
    /// Returns [`CipherError`] when the underlying engine fails.
    fn tick(&mut self) -> Result<(), CipherError>;

    /// Decodes one NAL unit (without start code) and returns the decoded bytes.
    ///
    /// # Errors
    /// Returns [`CipherError`] when the underlying engine fails.
    fn decode(&mut self, nal: &[u8]) -> Result<Vec<u8>, CipherError>;
}

impl<C: PayloadCipher + ?Sized> PayloadCipher for Box<C> {
    fn tick(&mut self) -> Result<(), CipherError> {
        (**self).tick()
    }

    fn decode(&mut self, nal: &[u8]) -> Result<Vec<u8>, CipherError> {
        (**self).decode(nal)
    }
}
