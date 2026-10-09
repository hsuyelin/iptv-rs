//! WASM hosts for the IPTV relay.
//!
//! [`AssetBundle`] loads and verifies the runtime assets once, compiles every module
//! once, and hands out cheap per-use instances: [`CmgSession`] for stream decryption,
//! [`TicketSigner`] and [`KeygenSigner`] for request signing.

/// Builds a [`WasmError::Runtime`] from a format string.
macro_rules! wasm_err {
    ($($arg:tt)*) => {
        $crate::WasmError::Runtime(format!($($arg)*))
    };
}

mod assets;
mod cipher;
mod cmg;
mod error;
mod keygen;
mod ticket;

pub use assets::{write_manifest, AssetBundle, AssetError, LoadReport, REQUIRED_FILES};
pub use cipher::CmgSession;
pub use cmg::CmgRuntime;
pub use error::{Result, WasmError};
pub use keygen::{KeygenInput, KeygenSigner};
pub use ticket::{TicketRequest, TicketSigner};

#[cfg(test)]
mod tests;
