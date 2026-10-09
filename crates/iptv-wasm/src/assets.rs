use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wasmtime::{Engine, Module};

use crate::{cmg::CmgRuntime, error::Result, keygen::KeygenSigner, ticket::TicketSigner};

/// Files that must be listed in `manifest.json` and present in the assets directory.
pub const REQUIRED_FILES: [&str; 4] = [
    "keygen_bg.wasm",
    "ticket.wasm",
    "cmg.worker.js",
    "CMGPlayer.json",
];

const MANIFEST_FILE: &str = "manifest.json";
const WASM_MARKER: &str = "wasmBinaryFile=\"data:application/octet-stream;base64,";
const PRERUN_MARKER: &str = "__ATPRERUN__.push(function(){";
const RELOCATION_MARKER: &str =
    "for(var e=0;e<A.length;e++)HEAPU32[eb+A[e]>>2]=HEAPU32[eb+A[e]>>2]+eb";
const EB_BASE: usize = 6_309_392;

/// Errors raised while loading the asset directory.
#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    /// The assets directory does not exist or is not a directory.
    #[error("assets directory does not exist: {}", .0.display())]
    MissingDirectory(PathBuf),
    /// A file could not be read.
    #[error("cannot read asset {file}: {source}")]
    Read {
        /// File name inside the assets directory.
        file: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// `manifest.json` is not valid.
    #[error("invalid {MANIFEST_FILE}: {0}")]
    Manifest(String),
    /// A required file has no digest in the manifest.
    #[error("required asset {0} is not listed in {MANIFEST_FILE}")]
    NotListed(String),
    /// A file's SHA-256 differs from the manifest.
    #[error("asset {file} digest mismatch: expected {expected}, found {actual}")]
    DigestMismatch {
        /// File name inside the assets directory.
        file: String,
        /// Digest from the manifest.
        expected: String,
        /// Digest of the file on disk.
        actual: String,
    },
    /// The CMG worker script did not have the expected structure.
    #[error("cannot parse cmg.worker.js: {0}")]
    Script(String),
    /// A WASM module failed to compile.
    #[error("cannot compile {file}: {source}")]
    Compile {
        /// File the module came from.
        file: String,
        /// Compiler error.
        #[source]
        source: wasmtime::Error,
    },
}

/// How much work loading did. Instances created later never add to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadReport {
    /// WASM modules compiled.
    pub modules_compiled: usize,
    /// Worker scripts parsed.
    pub scripts_parsed: usize,
}

#[derive(Deserialize)]
struct Manifest {
    files: BTreeMap<String, String>,
}

/// The CMG module plus the data an instance needs, extracted once from the worker script.
pub struct CmgImage {
    module: Module,
    static_data: Vec<u8>,
    player_json: Vec<u8>,
}

impl CmgImage {
    pub(crate) fn module(&self) -> &Module {
        &self.module
    }

    pub(crate) fn static_data(&self) -> &[u8] {
        &self.static_data
    }

    pub(crate) fn player_json(&self) -> &[u8] {
        &self.player_json
    }
}

/// Verified, compiled runtime assets shared by every channel and request.
pub struct AssetBundle {
    engine: Engine,
    keygen: Module,
    ticket: Module,
    cmg: Arc<CmgImage>,
    report: LoadReport,
}

impl AssetBundle {
    /// Loads, verifies and compiles the assets in `dir`.
    ///
    /// # Errors
    /// Returns [`crate::WasmError::Asset`] when the directory, manifest or a file is
    /// missing or invalid, or when a module fails to compile.
    pub fn load(dir: &Path) -> Result<Arc<Self>> {
        if !dir.is_dir() {
            return Err(AssetError::MissingDirectory(dir.to_path_buf()).into());
        }
        let manifest = read_manifest(dir)?;
        let mut files: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
        for name in REQUIRED_FILES {
            let expected = manifest
                .files
                .get(name)
                .ok_or_else(|| AssetError::NotListed(name.to_string()))?;
            let bytes = read_file(dir, name)?;
            let actual = hex::encode(Sha256::digest(&bytes));
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(AssetError::DigestMismatch {
                    file: name.to_string(),
                    expected: expected.clone(),
                    actual,
                }
                .into());
            }
            files.insert(name, bytes);
        }

        let engine = Engine::default();
        let take = |name: &str| files.get(name).map(Vec::as_slice).unwrap_or_default();
        let compile = |file: &str, bytes: &[u8]| {
            Module::from_binary(&engine, bytes).map_err(|source| AssetError::Compile {
                file: file.to_string(),
                source,
            })
        };
        let keygen = compile("keygen_bg.wasm", take("keygen_bg.wasm"))?;
        let ticket = compile("ticket.wasm", take("ticket.wasm"))?;

        let script = std::str::from_utf8(take("cmg.worker.js"))
            .map_err(|error| AssetError::Script(error.to_string()))?;
        let cmg_wasm = extract_wasm(script)?;
        let cmg = Arc::new(CmgImage {
            module: compile("cmg.worker.js", &cmg_wasm)?,
            static_data: extract_static_data(script)?,
            player_json: take("CMGPlayer.json").to_vec(),
        });
        Ok(Arc::new(Self {
            engine,
            keygen,
            ticket,
            cmg,
            report: LoadReport {
                modules_compiled: 3,
                scripts_parsed: 1,
            },
        }))
    }

    /// Work done while loading; constant for the life of the bundle.
    pub fn report(&self) -> LoadReport {
        self.report
    }

    /// Creates a fresh CMG instance whose guest sees `page_url` as its location.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when instantiation or initialization fails.
    pub fn new_cmg_runtime(&self, page_url: &str) -> Result<CmgRuntime> {
        CmgRuntime::new(&self.cmg, page_url)
    }

    /// Returns a signer for ticket strings.
    pub fn ticket_signer(&self) -> TicketSigner {
        TicketSigner::new(self.ticket.clone())
    }

    /// Returns a signer for SDK request signatures.
    pub fn keygen_signer(&self) -> KeygenSigner {
        KeygenSigner::new(self.keygen.clone())
    }

    /// The engine every module was compiled with.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

fn read_file(dir: &Path, name: &str) -> std::result::Result<Vec<u8>, AssetError> {
    fs::read(dir.join(name)).map_err(|source| AssetError::Read {
        file: name.to_string(),
        source,
    })
}

fn read_manifest(dir: &Path) -> std::result::Result<Manifest, AssetError> {
    let bytes = read_file(dir, MANIFEST_FILE)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AssetError::Manifest(error.to_string()))
}

fn script_error(message: &str) -> AssetError {
    AssetError::Script(message.to_string())
}

fn extract_wasm(script: &str) -> std::result::Result<Vec<u8>, AssetError> {
    let start = script
        .find(WASM_MARKER)
        .ok_or_else(|| script_error("wasm marker not found"))?
        + WASM_MARKER.len();
    let rest = script
        .get(start..)
        .ok_or_else(|| script_error("wasm marker split"))?;
    let end = rest
        .find('"')
        .ok_or_else(|| script_error("wasm terminator not found"))?;
    let encoded = rest
        .get(..end)
        .ok_or_else(|| script_error("wasm payload split"))?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| {
            AssetError::Script(format!("wasm payload is not base64: {error}"))
        })
}

fn extract_static_data(script: &str) -> std::result::Result<Vec<u8>, AssetError> {
    let prerun = script
        .find(PRERUN_MARKER)
        .ok_or_else(|| script_error("prerun block not found"))?;
    let mut cursor = prerun;
    let mut chunks: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut total_len = 0usize;
    while let Some(relative) = script.get(cursor..).and_then(|s| s.find("HEAPU8.set([")) {
        let array_start = cursor + relative + "HEAPU8.set([".len();
        let array_end = script
            .get(array_start..)
            .and_then(|s| s.find(']'))
            .ok_or_else(|| script_error("static data array terminator not found"))?
            + array_start;
        let after = script
            .get(array_end + 1..script.len().min(array_end + 64))
            .unwrap_or("");
        let Some(offset) = parse_eb_offset(after.trim_start()) else {
            cursor = array_end + 1;
            continue;
        };
        let body = script.get(array_start..array_end).unwrap_or("");
        let mut chunk = Vec::new();
        for raw in body.split(',') {
            let value = raw.trim();
            if value.is_empty() {
                continue;
            }
            chunk.push(value.parse::<u8>().map_err(|error| {
                AssetError::Script(format!("static byte {value}: {error}"))
            })?);
        }
        total_len = total_len.max(offset + chunk.len());
        chunks.push((offset, chunk));
        cursor = array_end + 1;
    }
    if chunks.is_empty() {
        return Err(script_error("static data arrays not found"));
    }
    let mut output = vec![0u8; total_len];
    for (offset, chunk) in chunks {
        output
            .get_mut(offset..offset + chunk.len())
            .ok_or_else(|| script_error("static data chunk out of range"))?
            .copy_from_slice(&chunk);
    }
    for offset in extract_relocations(script)? {
        let bytes = output.get_mut(offset..offset + 4).ok_or_else(|| {
            AssetError::Script(format!("relocation out of range offset={offset}"))
        })?;
        let current = u32::from_le_bytes(
            <[u8; 4]>::try_from(&*bytes).map_err(|_| script_error("relocation width"))?,
        );
        let base = u32::try_from(EB_BASE).map_err(|_| script_error("EB base"))?;
        bytes.copy_from_slice(&current.wrapping_add(base).to_le_bytes());
    }
    Ok(output)
}

fn extract_relocations(script: &str) -> std::result::Result<Vec<usize>, AssetError> {
    let marker_pos = script
        .find(RELOCATION_MARKER)
        .ok_or_else(|| script_error("relocation marker not found"))?;
    let head = script.get(..marker_pos).unwrap_or("");
    let block_start = head
        .rfind("var A=[];")
        .ok_or_else(|| script_error("relocation array start not found"))?
        + "var A=[];".len();
    let block = script.get(block_start..marker_pos).unwrap_or("");
    let mut cursor = 0usize;
    let mut offsets = Vec::new();
    while let Some(relative) = block.get(cursor..).and_then(|s| s.find("concat([")) {
        let array_start = cursor + relative + "concat([".len();
        let array_end = block
            .get(array_start..)
            .and_then(|s| s.find(']'))
            .ok_or_else(|| script_error("relocation concat terminator not found"))?
            + array_start;
        for raw in block.get(array_start..array_end).unwrap_or("").split(',') {
            let value = raw.trim();
            if !value.is_empty() {
                offsets.push(parse_js_usize(value)?);
            }
        }
        cursor = array_end + 1;
    }
    if offsets.is_empty() {
        for raw in block.trim().split(',') {
            let value = raw.trim();
            if !value.is_empty() {
                offsets.push(parse_js_usize(value)?);
            }
        }
    }
    if offsets.is_empty() {
        return Err(script_error("relocation array is empty"));
    }
    Ok(offsets)
}

fn parse_js_usize(value: &str) -> std::result::Result<usize, AssetError> {
    let bad = |error: std::num::ParseIntError| {
        AssetError::Script(format!("number {value}: {error}"))
    };
    if let Some((base, exp)) = value.split_once('e') {
        let base = base.parse::<usize>().map_err(bad)?;
        let exp = exp.parse::<u32>().map_err(bad)?;
        return 10usize
            .checked_pow(exp)
            .and_then(|scale| base.checked_mul(scale))
            .ok_or_else(|| AssetError::Script(format!("number {value} overflows")));
    }
    value.parse::<usize>().map_err(bad)
}

fn parse_eb_offset(text: &str) -> Option<usize> {
    let rest = text.strip_prefix(",eb+")?;
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit() && ch != 'e')
        .unwrap_or(rest.len());
    parse_js_usize(rest.get(..end)?).ok()
}

/// Writes `manifest.json` for the required files of `dir`.
///
/// # Errors
/// Returns [`AssetError::Read`] when a required file cannot be read.
pub fn write_manifest(dir: &Path) -> std::result::Result<(), AssetError> {
    let mut files = BTreeMap::new();
    for name in REQUIRED_FILES {
        files.insert(name, hex::encode(Sha256::digest(read_file(dir, name)?)));
    }
    let text = serde_json::json!({ "files": files });
    let pretty = serde_json::to_string_pretty(&text)
        .map_err(|error| AssetError::Manifest(error.to_string()))?;
    fs::write(dir.join(MANIFEST_FILE), format!("{pretty}\n")).map_err(|source| {
        AssetError::Read {
            file: MANIFEST_FILE.to_string(),
            source,
        }
    })
}
