//! Channel file loading, indexing and hot reload.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError, RwLock,
    },
    time::{Duration, Instant, SystemTime},
};

use iptv_upstream::Channel;
use serde::Deserialize;
use tracing::{info, warn};

/// Errors raised while reading the channel file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read channels file {}: {source}", path.display())]
    Read {
        /// File path.
        path: PathBuf,
        /// I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid YAML of the expected shape.
    #[error("cannot parse channels file {}: {source}", path.display())]
    Parse {
        /// File path.
        path: PathBuf,
        /// YAML error.
        #[source]
        source: serde_yaml::Error,
    },
    /// The file holds no usable channel.
    #[error("channels file {} has no valid channels", path.display())]
    NoChannels {
        /// File path.
        path: PathBuf,
    },
}

#[derive(Deserialize)]
struct ChannelFile {
    #[serde(default)]
    channels: Vec<Channel>,
}

/// The channels of one version of the file, indexed by lowercase slug.
#[derive(Debug)]
pub struct ChannelIndex {
    channels: Vec<Arc<Channel>>,
    by_slug: HashMap<Box<str>, usize>,
}

impl ChannelIndex {
    /// Builds an index. Invalid entries are dropped; for a repeated slug the first wins.
    pub fn new(channels: Vec<Channel>) -> Self {
        let mut kept = Vec::with_capacity(channels.len());
        let mut by_slug: HashMap<Box<str>, usize> =
            HashMap::with_capacity(channels.len());
        for channel in channels.into_iter().filter(Channel::is_valid) {
            let key: Box<str> = channel.ch.trim().to_ascii_lowercase().into();
            if by_slug.contains_key(&key) {
                warn!(ch = %channel.ch, "duplicate channel slug ignored");
                continue;
            }
            by_slug.insert(key, kept.len());
            kept.push(Arc::new(channel));
        }
        Self {
            channels: kept,
            by_slug,
        }
    }

    /// Parses channel YAML text.
    ///
    /// # Errors
    /// Returns [`ConfigError::Parse`] for invalid YAML and [`ConfigError::NoChannels`]
    /// when no valid channel remains.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let file: ChannelFile =
            serde_yaml::from_str(text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let index = Self::new(file.channels);
        if index.is_empty() {
            return Err(ConfigError::NoChannels {
                path: path.to_path_buf(),
            });
        }
        Ok(index)
    }

    /// Finds a channel by slug, ignoring case and surrounding spaces.
    pub fn find_by_slug(&self, slug: &str) -> Option<&Arc<Channel>> {
        let slug = slug.trim();
        let position = if slug.bytes().any(|byte| byte.is_ascii_uppercase()) {
            self.by_slug.get(slug.to_ascii_lowercase().as_str())
        } else {
            self.by_slug.get(slug)
        }?;
        self.channels.get(*position)
    }

    /// All channels in file order.
    pub fn channels(&self) -> &[Arc<Channel>] {
        &self.channels
    }

    /// Number of channels.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Whether there are no channels.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

/// What `/health` reports about the channel file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStatus {
    /// Path of the file.
    pub path: PathBuf,
    /// Channels currently served.
    pub count: usize,
    /// Why the latest reload failed, if it did.
    pub reload_error: Option<String>,
}

struct Seen {
    modified: Option<SystemTime>,
    reload_error: Option<String>,
}

/// Serves channels from memory and reloads the file when it changes.
///
/// The file's modification time is checked at most once per `check_interval`. A reload
/// that fails keeps the previous channels and records the error.
pub struct ChannelStore {
    path: PathBuf,
    current: RwLock<Arc<ChannelIndex>>,
    seen: Mutex<Seen>,
    started: Instant,
    last_check_ms: AtomicU64,
    check_interval: Duration,
}

impl ChannelStore {
    /// Loads the file for the first time.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when the file is missing, invalid or has no channels.
    pub fn load(
        path: impl Into<PathBuf>,
        check_interval: Duration,
    ) -> Result<Self, ConfigError> {
        let path = path.into();
        let (index, modified) = read_index(&path)?;
        info!(path = %path.display(), count = index.len(), "loaded channel directory");
        Ok(Self {
            path,
            current: RwLock::new(Arc::new(index)),
            seen: Mutex::new(Seen {
                modified,
                reload_error: None,
            }),
            started: Instant::now(),
            // Zero means "never checked"; the first call always checks.
            last_check_ms: AtomicU64::new(0),
            check_interval,
        })
    }

    /// The channels to serve now, after a reload check when one is due.
    pub fn snapshot(&self) -> Arc<ChannelIndex> {
        self.reload_if_due();
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Current status for `/health`.
    pub fn status(&self) -> StoreStatus {
        let count = self.snapshot().len();
        let reload_error = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reload_error
            .clone();
        StoreStatus {
            path: self.path.clone(),
            count,
            reload_error,
        }
    }

    fn reload_if_due(&self) {
        let now_ms = u64::try_from(self.started.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let last = self.last_check_ms.load(Ordering::Relaxed);
        let interval_ms =
            u64::try_from(self.check_interval.as_millis()).unwrap_or(u64::MAX);
        if last != 0 && now_ms.saturating_sub(last) < interval_ms {
            return;
        }
        if self
            .last_check_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // Another request is checking right now.
            return;
        }
        self.reload();
    }

    fn reload(&self) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let modified = match fs::metadata(&self.path).and_then(|meta| meta.modified()) {
            Ok(modified) => Some(modified),
            Err(error) => {
                let message = format!("cannot stat {}: {error}", self.path.display());
                if seen.reload_error.as_deref() != Some(message.as_str()) {
                    warn!(error = %message, "channel file unavailable; keeping previous channels");
                }
                seen.reload_error = Some(message);
                return;
            }
        };
        if modified == seen.modified {
            return;
        }
        match read_index(&self.path) {
            Ok((index, modified)) => {
                info!(count = index.len(), "reloaded channel directory");
                *self.current.write().unwrap_or_else(PoisonError::into_inner) =
                    Arc::new(index);
                seen.modified = modified;
                seen.reload_error = None;
            }
            Err(error) => {
                warn!(error = %error, "channel reload failed; keeping previous channels");
                // Remember the version so a broken file is not parsed every second.
                seen.modified = modified;
                seen.reload_error = Some(error.to_string());
            }
        }
    }
}

fn read_index(path: &Path) -> Result<(ChannelIndex, Option<SystemTime>), ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let modified = fs::metadata(path).and_then(|meta| meta.modified()).ok();
    Ok((ChannelIndex::parse(&text, path)?, modified))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const ONE: &str = r#"
channels:
  - ch: cctv1
    logo: "https://example/logo.png"
    chinese: "CCTV-1 综合"
    cnlid: "2024078201"
    livepid: "600001859"
    group: "央视"
"#;

    const TWO: &str = r#"
channels:
  - { ch: cctv1, cnlid: "1", livepid: "1", chinese: "One" }
  - { ch: CCTV2, cnlid: "2", livepid: "2" }
"#;

    fn write(dir: &tempfile::TempDir, text: &str, mtime_offset_secs: u64) -> PathBuf {
        let path = dir.path().join("channels.yaml");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.set_modified(
            SystemTime::UNIX_EPOCH
                + Duration::from_secs(1_700_000_000 + mtime_offset_secs),
        )
        .unwrap();
        path
    }

    #[test]
    fn parses_yaml_and_fills_defaults() {
        let index = ChannelIndex::parse(ONE, Path::new("c.yaml")).unwrap();
        let channel = index.find_by_slug("cctv1").unwrap();
        assert_eq!(channel.display_name(), "CCTV-1 综合");
        assert_eq!(channel.cache_key(), "2024078201:600001859");
        let defaults = ChannelIndex::parse(TWO, Path::new("c.yaml")).unwrap();
        assert_eq!(defaults.find_by_slug("cctv2").unwrap().logo, "");
    }

    #[test]
    fn lookup_ignores_case_and_spaces() {
        let index = ChannelIndex::parse(TWO, Path::new("c.yaml")).unwrap();
        assert_eq!(index.find_by_slug(" CCTV1 ").unwrap().ch, "cctv1");
        assert_eq!(index.find_by_slug("cctv2").unwrap().ch, "CCTV2");
        assert!(index.find_by_slug("nope").is_none());
        assert!(index.find_by_slug("").is_none());
    }

    #[test]
    fn invalid_entries_are_dropped_and_duplicates_keep_the_first() {
        let text = r#"
channels:
  - { ch: a, cnlid: "1", livepid: "1", chinese: first }
  - { ch: A, cnlid: "2", livepid: "2", chinese: second }
  - { ch: b, cnlid: "", livepid: "3" }
  - { ch: " ", cnlid: "4", livepid: "4" }
"#;
        let index = ChannelIndex::parse(text, Path::new("c.yaml")).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.find_by_slug("a").unwrap().chinese, "first");
    }

    #[test]
    fn empty_or_broken_files_are_errors() {
        assert!(matches!(
            ChannelIndex::parse("channels: []", Path::new("c.yaml")),
            Err(ConfigError::NoChannels { .. })
        ));
        assert!(matches!(
            ChannelIndex::parse("channels: [oops", Path::new("c.yaml")),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            ChannelIndex::parse("{}", Path::new("c.yaml")),
            Err(ConfigError::NoChannels { .. })
        ));
    }

    #[test]
    fn startup_failure_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.yaml");
        let error = ChannelStore::load(&missing, Duration::ZERO).err().unwrap();
        assert!(error.to_string().contains("absent.yaml"));
        let path = write(&dir, "channels: []", 0);
        let error = ChannelStore::load(&path, Duration::ZERO).err().unwrap();
        assert!(error.to_string().contains("channels.yaml"));
    }

    #[test]
    fn edits_are_picked_up_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, ONE, 0);
        let store = ChannelStore::load(&path, Duration::ZERO).unwrap();
        assert_eq!(store.snapshot().len(), 1);
        write(&dir, TWO, 10);
        assert_eq!(store.snapshot().len(), 2);
        assert!(store.status().reload_error.is_none());
    }

    #[test]
    fn a_broken_edit_keeps_the_previous_channels_and_reports_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, TWO, 0);
        let store = ChannelStore::load(&path, Duration::ZERO).unwrap();
        write(&dir, "channels: [oops", 10);
        assert_eq!(store.snapshot().len(), 2);
        let status = store.status();
        assert_eq!(status.count, 2);
        assert!(status.reload_error.unwrap().contains("cannot parse"));
        // Fixing the file clears the error.
        write(&dir, ONE, 20);
        assert_eq!(store.snapshot().len(), 1);
        assert!(store.status().reload_error.is_none());
    }

    #[test]
    fn a_removed_file_keeps_serving_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, ONE, 0);
        let store = ChannelStore::load(&path, Duration::ZERO).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(store.snapshot().len(), 1);
        assert!(store.status().reload_error.unwrap().contains("cannot stat"));
    }

    #[test]
    fn checks_are_throttled_by_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, ONE, 0);
        let store = ChannelStore::load(&path, Duration::from_secs(3600)).unwrap();
        // The very first call performs a check, later ones within the hour do not.
        assert_eq!(store.snapshot().len(), 1);
        write(&dir, TWO, 10);
        assert_eq!(store.snapshot().len(), 1);
    }
}
