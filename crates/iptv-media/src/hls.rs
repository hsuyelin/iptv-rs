use std::fmt::Write as _;

use sha1::{Digest, Sha1};
use url::Url;

use crate::error::MediaError;

/// A media segment announced by an upstream playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSegment {
    /// Absolute segment URL.
    pub url: String,
    /// Duration in seconds.
    pub duration: f64,
    /// Media sequence number.
    pub sequence: i64,
}

/// Result of parsing an upstream M3U8.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPlaylist {
    /// Value of `EXT-X-MEDIA-SEQUENCE`.
    pub media_sequence: i64,
    /// Segments in playlist order.
    pub segments: Vec<ParsedSegment>,
    /// Child playlist URLs (master playlists).
    pub playlists: Vec<String>,
}

/// A segment the relay has published under a local id.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentRef {
    /// Local id used in `/segment/{ch}/{id}.ts`.
    pub id: String,
    /// Upstream segment URL.
    pub url: String,
    /// Duration in seconds.
    pub duration: f64,
    /// Media sequence number.
    pub sequence: i64,
}

/// Size limits for the playlist window served to players.
#[derive(Debug, Clone, Copy)]
pub struct WindowPolicy {
    /// Maximum number of segments listed.
    pub window: usize,
    /// Newest segments withheld from the list.
    pub holdback: usize,
}

/// Parses an upstream M3U8, joining relative URLs against `base_url`.
///
/// # Errors
/// Returns [`MediaError::PlaylistUrl`] when `base_url` or a playlist line is not a valid
/// URL.
pub fn parse_media_playlist(
    text: &str,
    base_url: &str,
) -> Result<ParsedPlaylist, MediaError> {
    let url_error = |url: &str, reason: url::ParseError| MediaError::PlaylistUrl {
        url: url.to_string(),
        reason: reason.to_string(),
    };
    let base = Url::parse(base_url).map_err(|e| url_error(base_url, e))?;
    let mut media_sequence = 0i64;
    let mut pending_duration = None;
    let mut segments = Vec::new();
    let mut playlists = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value.trim().parse::<i64>().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration = value
                .split(',')
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok());
        } else if line.starts_with('#') {
            continue;
        } else {
            let url = base.join(line).map_err(|e| url_error(line, e))?.to_string();
            if line.contains(".m3u8") {
                playlists.push(url);
            } else {
                let offset = i64::try_from(segments.len()).unwrap_or(i64::MAX);
                segments.push(ParsedSegment {
                    url,
                    duration: pending_duration.unwrap_or(5.0),
                    sequence: media_sequence.saturating_add(offset),
                });
            }
            pending_duration = None;
        }
    }
    Ok(ParsedPlaylist {
        media_sequence,
        segments,
        playlists,
    })
}

/// Stable local id for a segment: first 20 hex chars of SHA-1 over `"{scope}:{sequence}"`.
pub fn segment_id(scope: &str, sequence: i64) -> String {
    let digest = Sha1::digest(format!("{scope}:{sequence}").as_bytes());
    let mut id = hex::encode(digest);
    id.truncate(20);
    id
}

/// Selects the segments to publish: drop the newest `holdback`, keep at most `window`,
/// and keep only the newest unbroken run of consecutive sequence numbers.
pub fn playable_window(segments: &[SegmentRef], policy: WindowPolicy) -> &[SegmentRef] {
    let keep = segments.len().saturating_sub(policy.holdback);
    let eligible = segments.get(..keep).unwrap_or(&[]);
    let from = eligible.len().saturating_sub(policy.window);
    let tail = eligible.get(from..).unwrap_or(&[]);
    let mut start = tail.len();
    while start > 0 {
        let fits = match (tail.get(start - 1), tail.get(start)) {
            (Some(previous), Some(next)) => {
                previous.sequence.saturating_add(1) == next.sequence
            }
            _ => true,
        };
        if !fits {
            break;
        }
        start -= 1;
    }
    tail.get(start..).unwrap_or(&[])
}

/// Renders a live media playlist for `segments`; `segment_url` maps a segment to the URL
/// players should request. Returns `None` when `segments` is empty.
pub fn render_local_playlist(
    segments: &[SegmentRef],
    segment_url: impl Fn(&SegmentRef) -> String,
) -> Option<String> {
    let first = segments.first()?;
    let target = segments
        .iter()
        .map(|segment| segment.duration.max(1.0).ceil() as u64)
        .max()
        .unwrap_or(5)
        .max(1);
    let mut out = String::with_capacity(160 + segments.len() * 140);
    out.push_str(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-ALLOW-CACHE:NO\n#EXT-X-INDEPENDENT-SEGMENTS\n",
    );
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{target}");
    let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{}", first.sequence);
    for segment in segments {
        let _ = writeln!(out, "#EXTINF:{:.3},", segment.duration);
        out.push_str(&segment_url(segment));
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(sequence: i64) -> SegmentRef {
        SegmentRef {
            id: segment_id("p", sequence),
            url: format!("https://u/{sequence}.ts"),
            duration: 2.0,
            sequence,
        }
    }

    const POLICY: WindowPolicy = WindowPolicy {
        window: 12,
        holdback: 1,
    };

    #[test]
    fn parses_segments_and_joins_urls() {
        let parsed = parse_media_playlist(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:2.000,\na.ts\n#EXTINF:3.500,\nb.ts\n",
            "https://example.com/live/index.m3u8?x=1",
        )
        .unwrap();
        assert_eq!(parsed.media_sequence, 10);
        assert_eq!(parsed.segments[0].sequence, 10);
        assert_eq!(parsed.segments[1].sequence, 11);
        assert_eq!(parsed.segments[0].url, "https://example.com/live/a.ts");
        assert!((parsed.segments[1].duration - 3.5).abs() < f64::EPSILON);
    }

    #[test]
    fn master_playlist_lists_children() {
        let parsed =
            parse_media_playlist("#EXTM3U\nlow/index.m3u8\n", "https://h/x/m.m3u8")
                .unwrap();
        assert!(parsed.segments.is_empty());
        assert_eq!(parsed.playlists, vec!["https://h/x/low/index.m3u8"]);
    }

    #[test]
    fn bad_base_url_is_an_error() {
        assert!(matches!(
            parse_media_playlist("a.ts", "not a url"),
            Err(MediaError::PlaylistUrl { .. })
        ));
    }

    #[test]
    fn window_is_bounded_and_holds_back_the_live_edge() {
        let segments: Vec<_> = (100..130).map(seg).collect();
        let window = playable_window(&segments, POLICY);
        assert_eq!(window.len(), 12);
        assert_eq!(window.last().map(|s| s.sequence), Some(128));
    }

    #[test]
    fn window_stops_at_a_sequence_gap() {
        let mut segments: Vec<_> = (1..=5).map(seg).collect();
        segments.extend((8..=12).map(seg));
        let window = playable_window(&segments, POLICY);
        assert_eq!(window.first().map(|s| s.sequence), Some(8));
        assert_eq!(window.last().map(|s| s.sequence), Some(11));
    }

    #[test]
    fn window_of_nothing_is_empty() {
        assert!(playable_window(&[], POLICY).is_empty());
        assert!(playable_window(&[seg(1)], POLICY).is_empty());
    }

    #[test]
    fn local_playlist_has_target_duration_and_sequence() {
        let segments = vec![seg(7), seg(8)];
        let text =
            render_local_playlist(&segments, |s| format!("/segment/x/{}.ts", s.id))
                .unwrap();
        assert!(text.contains("#EXT-X-TARGETDURATION:2\n"));
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:7\n"));
        assert!(text.contains("#EXTINF:2.000,\n/segment/x/"));
        assert!(render_local_playlist(&[], |_| String::new()).is_none());
    }

    #[test]
    fn segment_id_is_stable_and_20_chars() {
        assert_eq!(segment_id("a", 1), segment_id("a", 1));
        assert_ne!(segment_id("a", 1), segment_id("a", 2));
        assert_eq!(segment_id("a", 1).len(), 20);
    }
}
