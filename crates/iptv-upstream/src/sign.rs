use aes::Aes128;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use rand::{rngs::OsRng, RngCore};

use crate::{
    constants::{
        APP_VER, CKEY_AES_IV_HEX, CKEY_AES_KEY_HEX, PAGE_URL, PLATFORM, USER_AGENT,
    },
    error::{Result, UpstreamError},
};

type Aes128CbcEnc = cbc::Encryptor<Aes128>;

const BASE36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const RANDOM_ALPHABET: &[u8] =
    b"ABCDEFGHIJKlMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Lowercase hex MD5 of `text`.
pub fn md5_hex(text: impl AsRef<[u8]>) -> String {
    format!("{:x}", md5::compute(text))
}

/// Random alphanumeric string of `length` characters.
pub fn random_string(length: usize) -> String {
    pick_chars(RANDOM_ALPHABET, length)
}

fn pick_chars(alphabet: &[u8], length: usize) -> String {
    let mut bytes = vec![0u8; length];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| {
            alphabet
                .get(usize::from(byte) % alphabet.len())
                .copied()
                .map_or('0', char::from)
        })
        .collect()
}

/// Client guid: base36 millisecond clock, an underscore and 13 random base36 characters.
pub fn generate_guid() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{}_{}", base36(millis), random_base36(13))
}

fn random_base36(length: usize) -> String {
    pick_chars(BASE36, length)
}

fn base36(mut value: u128) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        let digit = usize::try_from(value % 36).unwrap_or(0);
        out.push(BASE36.get(digit).copied().map_or('0', char::from));
        value /= 36;
    }
    out.into_iter().rev().collect()
}

/// Canonical `k=v&...` string using locale-style key order.
pub fn canonical_sorted_string<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    canonical_locale_sorted_string(pairs)
}

/// Canonical string with case-insensitive key order, as the browser sorted.
pub fn canonical_locale_sorted_string<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let mut entries: Vec<(String, String)> = pairs
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    entries.sort_by(|left, right| js_locale_compare_ascii(&left.0, &right.0));
    entries
        .into_iter()
        .map(|(key, value)| format!("{key}={}", decode_uri_compat(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Canonical string with plain code-point key order.
pub fn canonical_js_default_sorted_string<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let mut entries: Vec<(String, String)> = pairs
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
        .into_iter()
        .map(|(key, value)| format!("{key}={}", decode_uri_compat(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// MD5 over the locale-sorted canonical string followed by `secret`.
pub fn md5_sorted_with_secret<I, K, V>(pairs: I, secret: &str) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    md5_hex(format!("{}{}", canonical_sorted_string(pairs), secret))
}

/// MD5 over the code-point-sorted canonical string followed by `secret`.
pub fn md5_js_default_sorted_with_secret<I, K, V>(pairs: I, secret: &str) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    md5_hex(format!(
        "{}{}",
        canonical_js_default_sorted_string(pairs),
        secret
    ))
}

fn decode_uri_compat(value: &str) -> String {
    // The browser implementation used decodeURI(value), not decodeURIComponent.
    // Keep reserved URI separators escaped only where decodeURI would preserve them.
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while let Some(&current) = bytes.get(i) {
        if current == b'%' && i + 2 < bytes.len() {
            // `get` instead of slicing: the two bytes after `%` may split a character.
            let decoded = value
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .map(char::from)
                .filter(|ch| {
                    !matches!(
                        ch,
                        ';' | '/' | '?' | ':' | '@' | '&' | '=' | '+' | '$' | ',' | '#'
                    )
                });
            if let Some(ch) = decoded {
                out.push(ch);
                i += 3;
                continue;
            }
        }
        out.push(char::from(current));
        i += 1;
    }
    out
}

fn js_locale_compare_ascii(left: &str, right: &str) -> std::cmp::Ordering {
    // V8 localeCompare puts '_' before letters but compares case-insensitively
    // for the ASCII keys used by the signing body: app_version < appVer.
    let left_folded = left.to_ascii_lowercase();
    let right_folded = right.to_ascii_lowercase();
    left_folded.cmp(&right_folded).then_with(|| left.cmp(right))
}

/// JavaScript-style 32-bit string hash over UTF-16 code units.
pub fn js_int32_hash(text: &str) -> i32 {
    let mut hash: i32 = 0;
    for unit in text.encode_utf16() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(unit as i32);
    }
    hash
}

#[derive(Debug, Clone)]
/// An encrypted client key and the values it was derived from.
pub struct CKeyInfo {
    /// Encrypted key string sent to the service.
    pub ckey: String,
    /// Pipe-joined hash input.
    pub hash_source: String,
    /// Hash of `hash_source`.
    pub hash_prefix: i32,
    /// Plain text that was encrypted.
    pub plain: String,
}

/// Builds the client key for a channel.
///
/// # Errors
/// Returns [`UpstreamError::Key`] when the built-in AES material is malformed.
pub fn build_ckey(cnlid: &str, ts: i64, guid: &str) -> Result<CKeyInfo> {
    let hash_source = [
        "",
        cnlid,
        &ts.to_string(),
        "mg3c3b04ba",
        APP_VER,
        guid,
        PLATFORM,
        &PAGE_URL.chars().take(24).collect::<String>(),
        &USER_AGENT
            .to_lowercase()
            .chars()
            .take(24)
            .collect::<String>(),
        "",
        "Mozilla",
        "Netscape",
        "MacIntel",
        "",
    ]
    .join("|");
    let hash_prefix = js_int32_hash(&hash_source);
    let plain = format!("|{hash_prefix}{hash_source}");
    let key =
        hex::decode(CKEY_AES_KEY_HEX).map_err(|e| UpstreamError::Key(e.to_string()))?;
    let iv =
        hex::decode(CKEY_AES_IV_HEX).map_err(|e| UpstreamError::Key(e.to_string()))?;
    let encrypted = Aes128CbcEnc::new_from_slices(&key, &iv)
        .map_err(|e| UpstreamError::Key(e.to_string()))?
        .encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    Ok(CKeyInfo {
        ckey: format!("--01{}", hex::encode_upper(encrypted)),
        hash_source,
        hash_prefix,
        plain,
    })
}

/// Renders a JSON scalar the way the signing code expects.
pub(crate) fn json_scalar_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{AUTH_SECRET, LIVE_SECRET};

    #[test]
    fn canonical_body_md5_matches_fixture() {
        let body = [
            ("cnlid", "600099502"),
            ("livepid", "600099502"),
            ("stream", ""),
            ("guid", "moxfxpzd_dto2apb3j9j"),
            ("cKey", "test"),
            ("adjust", "1"),
            ("sphttps", "1"),
            ("platform", "5910204"),
            ("cmd", "2"),
            ("encryptVer", "8.1"),
            ("dtype", "1"),
            ("devid", "devid"),
            ("otype", "ojson"),
            ("appVer", "V1.0.0"),
            ("app_version", "V1.0.0"),
            ("channel", "ysp_tx"),
        ];
        assert_eq!(
            md5_hex(canonical_sorted_string(body)),
            "a9f1e4f4aa672cbed61eec91fcade54e"
        );
    }

    #[test]
    fn auth_signature_uses_js_default_sort() {
        let body = [
            ("pid", "600099502"),
            ("guid", "moygaemw_oj9xhxuw53"),
            ("appid", "ysp_pc"),
            ("rand_str", "ON275u4qcs"),
        ];
        assert_eq!(
            md5_js_default_sorted_with_secret(body, AUTH_SECRET),
            "440d72dd805d0e2acee3636b2bf3a0ff"
        );
    }

    #[test]
    fn live_signature_uses_js_default_sort() {
        let body = serde_json::json!({
            "cnlid": "2027249301",
            "livepid": "600099502",
            "stream": "2",
            "guid": "moygaemw_oj9xhxuw53",
            "cKey": "--01650649EF959941ACBA72036E22CB6A7006985DD5D7D61A6902FE2D15AD5AFEBC7B2BFDC9183F33CF72BAFF2C0555A3FFD9F9914042A00C0401C783475455839F76BEAC7B096B5B0E44481C9E6C0EC113EDC2247B8E5D67DD93E66AE327B71A9AFDFE8A73864EA1063B1BA4E1D2DBA9B3680A693E0F27E31CC63933808E97239C0EF8E686A3CD31178AC2DBD035DEFBD6D89348BB87CEC94F9D1490B76EFF3E39",
            "adjust": 1,
            "sphttps": "1",
            "platform": "5910204",
            "cmd": "2",
            "encryptVer": "8.1",
            "dtype": "1",
            "devid": "devid",
            "otype": "ojson",
            "appVer": "V1.0.0",
            "app_version": "V1.0.0",
            "channel": "ysp_tx",
            "defn": "fhd",
            "rand_str": "RhVWnrcdBx"
        })
        .as_object()
        .unwrap()
        .clone();
        let expected = "e185ca3f0fae957433ae96429acf8207";
        let pairs = body
            .iter()
            .map(|(key, value)| (key.clone(), json_scalar_to_string(value)));
        assert_eq!(
            md5_js_default_sorted_with_secret(pairs, LIVE_SECRET),
            expected
        );
    }

    #[test]
    fn ckey_matches_browser_sample_prefix_shape() {
        let ckey = build_ckey("2027249301", 1778337597, "moygaemw_oj9xhxuw53").unwrap();
        assert!(ckey.ckey.starts_with("--01"));
        assert_eq!(ckey.hash_prefix, 455352550);
        assert_eq!(ckey.ckey, "--01650649EF959941ACBA72036E22CB6A7006985DD5D7D61A6902FE2D15AD5AFEBC7B2BFDC9183F33CF72BAFF2C0555A3FFD9F9914042A00C0401C783475455839F76BEAC7B096B5B0E44481C9E6C0EC113EDC2247B8E5D67DD93E66AE327B71A9AFDFE8A73864EA1063B1BA4E1D2DBA9B3680A693E0F27E31CC63933808E97239C0EF8E686A3CD31178AC2DBD035DEFBD6D89348BB87CEC94F9D1490B76EFF3E39");
    }

    #[test]
    fn decode_uri_keeps_reserved_escapes_and_survives_multibyte_text() {
        assert_eq!(decode_uri_compat("a%20b"), "a b");
        assert_eq!(decode_uri_compat("a%2Fb%3Fc"), "a%2Fb%3Fc");
        // `%` followed by a multi-byte character must not panic.
        assert_eq!(decode_uri_compat("%é!"), "%Ã©!");
        assert_eq!(decode_uri_compat("%aé"), "%aÃ©");
        assert_eq!(decode_uri_compat("100%"), "100%");
        assert_eq!(decode_uri_compat("%zz1"), "%zz1");
    }

    #[test]
    fn guid_has_clock_prefix_and_random_suffix() {
        let guid = generate_guid();
        let (clock, random) = guid.split_once('_').unwrap();
        assert!(!clock.is_empty() && clock.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(random.len(), 13);
        assert_ne!(guid, generate_guid());
    }

    #[test]
    fn base36_round_numbers() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    #[test]
    fn locale_sort_puts_underscore_keys_before_camel_case() {
        let body = [("appVer", "1"), ("app_version", "2")];
        assert_eq!(canonical_sorted_string(body), "app_version=2&appVer=1");
        assert_eq!(
            canonical_js_default_sorted_string(body),
            "appVer=1&app_version=2"
        );
    }

    #[test]
    fn js_hash_matches_known_values() {
        assert_eq!(js_int32_hash(""), 0);
        assert_eq!(js_int32_hash("a"), 97);
        assert_eq!(js_int32_hash("hello"), 99_162_322);
    }
}
