//! Regenerates `manifest.json` for an assets directory: `manifest <dir>`.
#![allow(missing_docs)]

fn main() -> std::process::ExitCode {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: manifest <assets-dir>");
        return std::process::ExitCode::from(2);
    };
    match iptv_wasm::write_manifest(std::path::Path::new(&dir)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
