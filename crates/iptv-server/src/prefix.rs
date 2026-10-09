//! Externally visible origin of the relay, derived from the request.

use axum::http::{HeaderMap, Uri};
use url::Url;

/// Origin (`scheme://host[:port]`) clients used to reach the relay, from `?prefix=` or the
/// forwarding headers.
pub fn external_base(headers: &HeaderMap, uri: &Uri) -> String {
    if let Some(prefix) = explicit_prefix(uri) {
        return prefix;
    }

    let forwarded = parse_forwarded(first_header(headers, "forwarded"));
    let mut proto = forwarded
        .get("proto")
        .cloned()
        .or_else(|| first_header(headers, "x-forwarded-proto"))
        .unwrap_or_else(|| "http".to_string())
        .trim_end_matches(':')
        .to_ascii_lowercase();
    if proto != "http" && proto != "https" {
        proto = "http".to_string();
    }

    let mut host = forwarded
        .get("host")
        .cloned()
        .or_else(|| first_header(headers, "x-forwarded-host"))
        .or_else(|| first_header(headers, "host"))
        .unwrap_or_else(|| "127.0.0.1:8787".to_string());
    if let Some(port) = first_header(headers, "x-forwarded-port") {
        if !host.contains(':') {
            host = format!("{host}:{port}");
        }
    }
    format!("{proto}://{host}")
}

/// The origin given by a valid `?prefix=` query parameter.
pub fn explicit_prefix(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "prefix" {
            if let Ok(parsed) = Url::parse(&value) {
                if parsed.scheme() == "http" || parsed.scheme() == "https" {
                    return Some(parsed.origin().ascii_serialization());
                }
            }
        }
    }
    None
}

/// Absolute URL for `path` on the externally visible origin.
pub fn abs_url(headers: &HeaderMap, uri: &Uri, path: &str) -> String {
    let base = external_base(headers, uri)
        .trim_end_matches('/')
        .to_string();
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("{base}{path}")
}

/// Carries an explicit `?prefix=` over to `target` so nested playlists keep the origin.
pub fn append_recursive_prefix(uri: &Uri, target: &str) -> String {
    let Some(prefix) = explicit_prefix(uri) else {
        return target.to_string();
    };
    let Ok(mut parsed) = Url::parse(target) else {
        return target.to_string();
    };
    parsed.query_pairs_mut().append_pair("prefix", &prefix);
    parsed.to_string()
}

/// First comma-separated value of a header, trimmed; `None` when absent or empty.
pub fn first_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_forwarded(header: Option<String>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(header) = header else {
        return out;
    };
    let first = header.split(',').next().unwrap_or_default().trim();
    for part in first.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        out.insert(key.trim().to_ascii_lowercase(), value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn prefix_is_origin_only_and_recursive() {
        let uri: Uri = "/list.m3u?prefix=https%3A%2F%2Fexample.com%3A9443%2Fabc"
            .parse()
            .unwrap();
        assert_eq!(
            explicit_prefix(&uri),
            Some("https://example.com:9443".to_string())
        );
        assert_eq!(
            append_recursive_prefix(&uri, "https://example.com:9443/live/cctv1.m3u8"),
            "https://example.com:9443/live/cctv1.m3u8?prefix=https%3A%2F%2Fexample.com%3A9443"
        );
    }

    #[test]
    fn invalid_or_non_http_prefix_is_ignored() {
        for query in ["prefix=ftp%3A%2F%2Fx", "prefix=notaurl", "other=1", ""] {
            let uri: Uri = format!("/list.m3u?{query}").parse().unwrap();
            assert_eq!(explicit_prefix(&uri), None, "{query}");
        }
    }

    #[test]
    fn forwarded_header_takes_precedence_and_port_is_appended() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "forwarded",
            HeaderValue::from_static("for=1.2.3.4;proto=https;host=\"tv.example.com\""),
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        let uri: Uri = "/list.m3u".parse().unwrap();
        assert_eq!(external_base(&headers, &uri), "https://tv.example.com");

        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("relay.local"));
        headers.insert("x-forwarded-port", HeaderValue::from_static("8443"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("gopher"));
        assert_eq!(external_base(&headers, &uri), "http://relay.local:8443");
        assert_eq!(
            external_base(&HeaderMap::new(), &uri),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn abs_url_joins_without_double_slashes() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("h:1"));
        let uri: Uri = "/x".parse().unwrap();
        assert_eq!(
            abs_url(&headers, &uri, "live/a.m3u8"),
            "http://h:1/live/a.m3u8"
        );
        assert_eq!(
            abs_url(&headers, &uri, "/live/a.m3u8"),
            "http://h:1/live/a.m3u8"
        );
    }

    #[test]
    fn external_base_uses_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("tv.example.com"),
        );
        let uri: Uri = "/list.m3u".parse().unwrap();
        assert_eq!(external_base(&headers, &uri), "https://tv.example.com");
    }
}
