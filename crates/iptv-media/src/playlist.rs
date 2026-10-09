use std::fmt::Write as _;

/// One channel as shown in the M3U channel list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// Short slug used in stream URLs.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Logo URL.
    pub logo: String,
    /// Group title.
    pub group: String,
}

/// Fixed text of the channel list: EPG source and the leading notice entry.
#[derive(Debug, Clone)]
pub struct ChannelListStyle<'a> {
    /// XMLTV guide URL.
    pub epg_url: &'a str,
    /// Name of the notice entry.
    pub notice_name: &'a str,
    /// Logo of the notice entry.
    pub notice_logo: &'a str,
    /// Stream URL of the notice entry.
    pub notice_url: &'a str,
}

/// Escapes a value for use inside a double-quoted M3U attribute.
///
/// Backslashes and quotes are escaped, line breaks become spaces, and the result is
/// trimmed so the entry always stays on one line.
pub fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' => out.push(' '),
            other => out.push(other),
        }
    }
    out.trim().to_string()
}

fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

/// Builds the M3U channel list. `stream_url` maps a slug to its absolute stream URL.
pub fn build_channel_list(
    entries: &[PlaylistEntry],
    style: &ChannelListStyle<'_>,
    stream_url: impl Fn(&str) -> String,
) -> String {
    let mut out = String::with_capacity(256 + entries.len() * 200);
    let _ = writeln!(
        out,
        "#EXTM3U x-tvg-url=\"{}\"",
        escape_attribute(style.epg_url)
    );
    let _ = writeln!(
        out,
        "#EXTINF:-1 tvg-name=\"{name}\" tvg-logo=\"{logo}\" group-title=\"{name}\",{name}",
        name = style.notice_name,
        logo = escape_attribute(style.notice_logo),
    );
    out.push_str(style.notice_url);
    out.push('\n');
    for entry in entries {
        let _ = writeln!(
            out,
            "#EXTINF:-1 tvg-name=\"{}\" tvg-logo=\"{}\" group-title=\"{}\",{}",
            escape_attribute(&entry.name),
            escape_attribute(&entry.logo),
            escape_attribute(&entry.group),
            single_line(&entry.name),
        );
        out.push_str(&stream_url(&entry.slug));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> ChannelListStyle<'static> {
        ChannelListStyle {
            epg_url: "https://epg.example/t.xml",
            notice_name: "Notice",
            notice_logo: "https://cdn/notice.jpg",
            notice_url: "https://cdn/notice.m3u8",
        }
    }

    #[test]
    fn escapes_quotes_backslashes_and_newlines() {
        assert_eq!(escape_attribute("  a\"b\\c\nd\r "), "a\\\"b\\\\c d");
    }

    #[test]
    fn list_has_notice_first_then_channels() {
        let entries = vec![PlaylistEntry {
            slug: "sd".into(),
            name: "山东卫视".into(),
            logo: "https://cdn/山东卫视.png".into(),
            group: "卫视".into(),
        }];
        let text = build_channel_list(&entries, &style(), |slug| {
            format!("http://h/live/{slug}.m3u8")
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "#EXTM3U x-tvg-url=\"https://epg.example/t.xml\"");
        assert_eq!(lines[2], "https://cdn/notice.m3u8");
        assert!(lines[3].contains("tvg-name=\"山东卫视\""));
        assert!(lines[3].ends_with("group-title=\"卫视\",山东卫视"));
        assert_eq!(lines[4], "http://h/live/sd.m3u8");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn hostile_names_stay_on_one_line() {
        let entries = vec![PlaylistEntry {
            slug: "x".into(),
            name: "a\nb\"c".into(),
            logo: String::new(),
            group: "g\r\nh".into(),
        }];
        let text = build_channel_list(&entries, &style(), |s| s.to_string());
        // 3 header lines + 2 lines per channel.
        assert_eq!(text.lines().count(), 5);
        assert!(text.contains("tvg-name=\"a b\\\"c\""));
    }
}
