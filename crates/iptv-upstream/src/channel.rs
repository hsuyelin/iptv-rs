use serde::{Deserialize, Serialize};

/// A channel from the configuration file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Channel {
    /// Short slug used in stream URLs.
    pub ch: String,
    /// Logo URL.
    #[serde(default)]
    pub logo: String,
    /// Display name.
    #[serde(default)]
    pub chinese: String,
    /// Upstream channel id.
    pub cnlid: String,
    /// Upstream live program id.
    pub livepid: String,
    /// Group title.
    #[serde(default)]
    pub group: String,
}

impl Channel {
    /// The display name, falling back to the slug when no name is set.
    pub fn display_name(&self) -> &str {
        if self.chinese.trim().is_empty() {
            &self.ch
        } else {
            &self.chinese
        }
    }

    /// Key under which the channel's live source is cached.
    pub fn cache_key(&self) -> String {
        format!("{}:{}", self.cnlid, self.livepid)
    }

    /// Whether the identifying fields are all non-blank.
    pub fn is_valid(&self) -> bool {
        !self.ch.trim().is_empty()
            && !self.cnlid.trim().is_empty()
            && !self.livepid.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(ch: &str, chinese: &str) -> Channel {
        Channel {
            ch: ch.into(),
            logo: String::new(),
            chinese: chinese.into(),
            cnlid: "2024078201".into(),
            livepid: "600001859".into(),
            group: String::new(),
        }
    }

    #[test]
    fn display_name_falls_back_to_slug() {
        assert_eq!(
            channel("cctv1", "CCTV-1 综合").display_name(),
            "CCTV-1 综合"
        );
        assert_eq!(channel("cctv1", "  ").display_name(), "cctv1");
    }

    #[test]
    fn cache_key_joins_ids() {
        assert_eq!(channel("a", "").cache_key(), "2024078201:600001859");
    }

    #[test]
    fn validity_requires_all_ids() {
        assert!(channel("a", "").is_valid());
        assert!(!channel(" ", "").is_valid());
        let mut missing = channel("a", "");
        missing.livepid = " ".into();
        assert!(!missing.is_valid());
    }
}
