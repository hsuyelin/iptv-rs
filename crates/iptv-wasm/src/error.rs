use std::fmt::Display;

use crate::assets::AssetError;

/// Errors raised by the WASM hosts.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// A guest call or host-side check failed.
    #[error("{0}")]
    Runtime(String),
    /// The WASM engine reported an error (trap, link or instantiation failure).
    #[error("wasm engine error: {0}")]
    Engine(#[from] wasmtime::Error),
    /// The asset directory could not be loaded.
    #[error(transparent)]
    Asset(#[from] AssetError),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, WasmError>;

macro_rules! from_display {
    ($($ty:ty),*) => {$(
        impl From<$ty> for WasmError {
            fn from(error: $ty) -> Self {
                Self::Runtime(error.to_string())
            }
        }
    )*};
}

from_display!(
    std::string::FromUtf8Error,
    std::io::Error,
    std::num::TryFromIntError,
    std::array::TryFromSliceError
);

/// Adds context to an error, like `anyhow::Context`.
pub trait Context<T> {
    /// Prefixes the error with `context`.
    ///
    /// # Errors
    /// Returns the original error wrapped with the context text.
    fn context<C: Display>(self, context: C) -> Result<T>;
}

impl<T, E: Into<WasmError>> Context<T> for std::result::Result<T, E> {
    fn context<C: Display>(self, context: C) -> Result<T> {
        self.map_err(|error| WasmError::Runtime(format!("{context}: {}", error.into())))
    }
}
