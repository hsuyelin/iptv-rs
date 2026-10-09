use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use iptv_media::PayloadCipher;

use crate::{
    AssetBundle, AssetError, CmgSession, KeygenInput, TicketRequest, WasmError,
    REQUIRED_FILES,
};

fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets")
}

/// Compiling the modules is the slow part, so read-only tests share one bundle.
fn bundle() -> Arc<AssetBundle> {
    static SHARED: OnceLock<Arc<AssetBundle>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| AssetBundle::load(&assets_dir()).unwrap()))
}

/// Copies the real assets and manifest into a scratch directory.
fn scratch_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in REQUIRED_FILES.iter().chain(&["manifest.json"]) {
        fs::copy(assets_dir().join(name), dir.path().join(name)).unwrap();
    }
    dir
}

fn asset_error(result: crate::Result<Arc<AssetBundle>>) -> AssetError {
    match result {
        Err(WasmError::Asset(error)) => error,
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("load unexpectedly succeeded"),
    }
}

#[test]
fn ticket_matches_browser_shape_for_sample() {
    let ticket = bundle()
        .ticket_signer()
        .sign(&TicketRequest {
            pid: "600099502",
            auth_ts: "1778337597",
            cnlid: "2027249301",
            guid: "moygaemw_oj9xhxuw53",
            app_id: "519748109",
            app_ver: "V1.0.0",
        })
        .unwrap();
    assert_eq!(ticket.len(), 122);
    assert!(ticket.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert!(ticket.starts_with(
        "5c40d99c3945f9087e0e99baca1a22edfc53707ceceed2181f99325fc5b5a3ffad9c8dd30779f69718b282fe1cb2211ec0e6cc"
    ));
}

#[test]
fn keygen_signature_matches_node_fixture() {
    let signature = bundle()
        .keygen_signer()
        .signature_hex(KeygenInput {
            guid: "moxfxpzd_dto2apb3j9j".to_string(),
            token: "372067b35cb4378ef8c86aab94a23a52".to_string(),
            app_id: "519748109".to_string(),
            input: "a9f1e4f4aa672cbed61eec91fcade54e-moxfxpzd_dto2apb3j9j-1-999999UWCr4euHl71778337551617"
                .to_string(),
            ts: "1778337551617".to_string(),
            version: "v1".to_string(),
            host: "www.yangshipin.cn".to_string(),
            protocol: "https:".to_string(),
        })
        .unwrap();
    assert_eq!(signature, "7a13aefe8715b8211f729059f36bc57c");
}

#[test]
fn load_reports_one_parse_and_three_compiles() {
    let bundle = bundle();
    let before = bundle.report();
    assert_eq!((before.modules_compiled, before.scripts_parsed), (3, 1));
    // Creating instances for two channels never recompiles or reparses.
    let first = bundle
        .new_cmg_runtime("https://www.yangshipin.cn/tv/home?pid=1")
        .unwrap();
    let second = bundle
        .new_cmg_runtime("https://www.yangshipin.cn/tv/home?pid=2")
        .unwrap();
    assert_eq!(bundle.report(), before);
    assert_eq!(first.vmp_tag(), "");
    assert_eq!(second.vmp_tag(), "");
}

#[test]
fn cmg_session_primes_and_ticks() {
    let bundle = bundle();
    let mut session = CmgSession::start(
        &bundle,
        "https://www.yangshipin.cn/tv/home?pid=600001859",
        "1778267239522".to_string(),
        "https://www.yangshipin.cn".to_string(),
    )
    .unwrap();
    session.tick().unwrap();
    assert_eq!(session.media_tag_id(), "1778267239522");
    assert!(session.page_url().ends_with("pid=600001859"));
}

#[test]
fn missing_directory_is_named() {
    let error = asset_error(AssetBundle::load(Path::new("/nonexistent/ysp-assets")));
    assert!(
        matches!(&error, AssetError::MissingDirectory(path) if path.ends_with("ysp-assets"))
    );
    assert!(error.to_string().contains("/nonexistent/ysp-assets"));
}

#[test]
fn tampered_file_reports_both_digests() {
    let dir = scratch_copy();
    let path = dir.path().join("ticket.wasm");
    let mut bytes = fs::read(&path).unwrap();
    bytes[10] ^= 0xff;
    fs::write(&path, bytes).unwrap();
    let error = asset_error(AssetBundle::load(dir.path()));
    match &error {
        AssetError::DigestMismatch {
            file,
            expected,
            actual,
        } => {
            assert_eq!(file, "ticket.wasm");
            assert_ne!(expected, actual);
            assert_eq!(expected.len(), 64);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(error.to_string().contains("ticket.wasm"));
}

#[test]
fn file_missing_from_manifest_is_rejected() {
    let dir = scratch_copy();
    let manifest = dir.path().join("manifest.json");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("ticket.wasm", "other.wasm");
    fs::write(&manifest, text).unwrap();
    assert!(matches!(
        asset_error(AssetBundle::load(dir.path())),
        AssetError::NotListed(name) if name == "ticket.wasm"
    ));
}

#[test]
fn missing_file_and_bad_manifest_are_typed() {
    let dir = scratch_copy();
    fs::remove_file(dir.path().join("keygen_bg.wasm")).unwrap();
    assert!(matches!(
        asset_error(AssetBundle::load(dir.path())),
        AssetError::Read { file, .. } if file == "keygen_bg.wasm"
    ));
    let dir = scratch_copy();
    fs::write(dir.path().join("manifest.json"), "not json").unwrap();
    assert!(matches!(
        asset_error(AssetBundle::load(dir.path())),
        AssetError::Manifest(_)
    ));
}

#[test]
fn manifest_written_by_helper_verifies() {
    let dir = scratch_copy();
    fs::remove_file(dir.path().join("manifest.json")).unwrap();
    crate::write_manifest(dir.path()).unwrap();
    AssetBundle::load(dir.path()).unwrap();
}

#[test]
fn bundle_is_not_in_the_binary() {
    // The library must not embed the assets: its source has no include macros.
    for file in ["assets.rs", "cmg.rs", "ticket.rs", "keygen.rs"] {
        let text = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file),
        )
        .unwrap();
        assert!(!text.contains(concat!("include_", "bytes!")), "{file}");
        assert!(!text.contains(concat!("include_", "str!")), "{file}");
    }
}
