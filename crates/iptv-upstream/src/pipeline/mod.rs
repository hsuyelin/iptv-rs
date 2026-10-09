use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
};

use bytes::Bytes;
use iptv_media::{
    decrypt_and_remux, parse_media_playlist, playable_window, render_local_playlist,
    segment_id, MuxState, PayloadCipher, RemuxStats, SegmentRef, VideoState,
    WindowPolicy,
};
use iptv_wasm::{AssetBundle, CmgSession};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

use crate::{
    channel::Channel,
    constants::{
        ACTIVE_URL, CHANNEL_QUEUE_CAPACITY, MEDIA_HISTORY_MAX_SEGMENTS,
        MEDIA_LIVE_EDGE_HOLDBACK_SEGMENTS, MEDIA_PLAYLIST_WINDOW_SEGMENTS,
        PUBLISHED_SEGMENTS_MAX, USER_AGENT,
    },
    error::{body_head, Result, UpstreamError},
    live::LiveClient,
    transport::{HttpRequest, HttpTransport},
};

/// Starts the decryptor for a channel.
///
/// The pipeline asks for a new cipher when a channel first needs one and again when the
/// stream jumps and the old state is useless.
pub trait CipherFactory: Send + Sync {
    /// Creates a primed cipher for the channel with live program id `livepid`.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] when the decryptor cannot be started.
    fn start(&self, livepid: &str) -> Result<Box<dyn PayloadCipher + Send>>;
}

/// [`CipherFactory`] backed by the CMG WASM module.
pub struct CmgCipherFactory {
    assets: Arc<AssetBundle>,
}

impl CmgCipherFactory {
    /// Creates a factory that instantiates the CMG module from `assets`.
    pub fn new(assets: Arc<AssetBundle>) -> Self {
        Self { assets }
    }
}

impl CipherFactory for CmgCipherFactory {
    fn start(&self, livepid: &str) -> Result<Box<dyn PayloadCipher + Send>> {
        let page_url = format!("{ACTIVE_URL}/tv/home?pid={livepid}");
        let session = CmgSession::start(
            &self.assets,
            &page_url,
            new_media_tag_id(),
            ACTIVE_URL.to_string(),
        )?;
        Ok(Box::new(session))
    }
}

/// Size limits of the pipeline.
#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    /// Window shown to players.
    pub window: WindowPolicy,
    /// Segments remembered per channel.
    pub history_max: usize,
    /// Published segment ids remembered overall.
    pub published_max: usize,
    /// Pending segment requests per channel.
    pub queue_capacity: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            window: WindowPolicy {
                window: MEDIA_PLAYLIST_WINDOW_SEGMENTS,
                holdback: MEDIA_LIVE_EDGE_HOLDBACK_SEGMENTS,
            },
            history_max: MEDIA_HISTORY_MAX_SEGMENTS,
            published_max: PUBLISHED_SEGMENTS_MAX,
            queue_capacity: CHANNEL_QUEUE_CAPACITY,
        }
    }
}

#[derive(Clone)]
struct Published {
    ch: String,
    livepid: String,
    segment: SegmentRef,
}

/// Insertion-ordered map that forgets its oldest entries beyond a capacity.
struct Fifo<V> {
    map: HashMap<String, V>,
    order: VecDeque<String>,
    capacity: usize,
}

impl<V> Fifo<V> {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn insert(&mut self, key: String, value: V) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&oldest);
        }
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }
}

struct State {
    published: Fifo<Published>,
    history: HashMap<String, Vec<SegmentRef>>,
    workers: HashMap<String, mpsc::Sender<Job>>,
}

struct Shared {
    live: LiveClient,
    transport: Arc<dyn HttpTransport>,
    ciphers: Arc<dyn CipherFactory>,
    config: PipelineConfig,
    state: Mutex<State>,
}

impl Shared {
    /// Bookkeeping only: the guard is never held across an await.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Job {
    segment: Published,
    reply: oneshot::Sender<Result<Bytes>>,
}

/// Turns upstream playlists into local ones and serves remuxed segments.
///
/// Segments of one channel are processed strictly in order by one worker task that owns
/// the channel's decryptor, so no lock is held while a segment is being processed.
#[derive(Clone)]
pub struct MediaPipeline {
    shared: Arc<Shared>,
}

impl MediaPipeline {
    /// Creates a pipeline.
    pub fn new(
        live: LiveClient,
        transport: Arc<dyn HttpTransport>,
        ciphers: Arc<dyn CipherFactory>,
        config: PipelineConfig,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                live,
                transport,
                ciphers,
                state: Mutex::new(State {
                    published: Fifo::new(config.published_max),
                    history: HashMap::new(),
                    workers: HashMap::new(),
                }),
                config,
            }),
        }
    }

    /// Current state of the upstream API flow limiter.
    pub fn flow_snapshot(&self) -> crate::flow::FlowSnapshot {
        self.shared.live.flow_snapshot()
    }

    /// Builds the media playlist for `channel`. `segment_url` maps a segment to the URL
    /// players should request.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] when the upstream source or playlist cannot be fetched,
    /// or when no segment is playable.
    pub async fn local_playlist(
        &self,
        channel: &Channel,
        segment_url: impl Fn(&SegmentRef) -> String,
    ) -> Result<String> {
        let live = &self.shared.live;
        debug!(ch = %channel.ch, livepid = %channel.livepid, "playlist requested");
        let source = live.fetch_source(channel.clone()).await?;
        let window = match self.fetch_media_playlist(&source.url).await {
            Ok(playlist) => self.update_history(channel, &playlist),
            Err(first_error) => {
                live.invalidate_source(&source.cache_key);
                let cached = self.history_window(&channel.livepid);
                if cached.is_empty() {
                    warn!(
                        ch = %channel.ch,
                        error = %first_error,
                        "media playlist failed and there is no history; refreshing the live source now"
                    );
                    let refreshed = live.refresh_source_now(channel.clone()).await?;
                    let playlist = self.fetch_media_playlist(&refreshed.url).await?;
                    self.update_history(channel, &playlist)
                } else {
                    live.refresh_source_background(channel.clone());
                    warn!(
                        ch = %channel.ch,
                        error = %first_error,
                        "serving cached media history while live source refreshes"
                    );
                    cached
                }
            }
        };
        let text = render_local_playlist(&window, segment_url).ok_or_else(|| {
            warn!(ch = %channel.ch, window = window.len(), "no playable segment in the window");
            UpstreamError::NoPlayableSegments
        })?;
        debug!(
            ch = %channel.ch,
            segments = window.len(),
            first_sequence = window.first().map(|segment| segment.sequence),
            last_sequence = window.last().map(|segment| segment.sequence),
            "rendered the local playlist"
        );
        let mut state = self.shared.state();
        for segment in window {
            state.published.insert(
                segment.id.clone(),
                Published {
                    ch: channel.ch.clone(),
                    livepid: channel.livepid.clone(),
                    segment,
                },
            );
        }
        Ok(text)
    }

    /// Returns the remuxed bytes of a segment previously listed by [`Self::local_playlist`].
    ///
    /// # Errors
    /// Returns [`UpstreamError::UnknownSegment`] or [`UpstreamError::SegmentNotInChannel`]
    /// for ids that do not match, [`UpstreamError::Overloaded`] when the channel queue is
    /// full, or the processing error.
    pub async fn segment(&self, channel: &Channel, id: &str) -> Result<Bytes> {
        let published = self
            .shared
            .state()
            .published
            .get(id)
            .cloned()
            .ok_or_else(|| {
                debug!(ch = %channel.ch, id, "segment id is not published (expired or never listed)");
                UpstreamError::UnknownSegment
            })?;
        if published.ch != channel.ch || published.livepid != channel.livepid {
            warn!(ch = %channel.ch, id, owner = %published.ch, "segment belongs to another channel");
            return Err(UpstreamError::SegmentNotInChannel);
        }
        trace!(ch = %channel.ch, id, sequence = published.segment.sequence, "segment queued");
        let sender = self.worker_for(&channel.livepid);
        let (reply, answer) = oneshot::channel();
        sender
            .try_send(Job {
                segment: published,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    UpstreamError::Overloaded(channel.ch.clone())
                }
                mpsc::error::TrySendError::Closed(_) => UpstreamError::WorkerStopped,
            })?;
        answer.await.map_err(|_| UpstreamError::WorkerStopped)?
    }

    fn worker_for(&self, livepid: &str) -> mpsc::Sender<Job> {
        let mut state = self.shared.state();
        if let Some(sender) = state.workers.get(livepid) {
            if !sender.is_closed() {
                return sender.clone();
            }
        }
        let capacity = self.shared.config.queue_capacity.max(1);
        info!(%livepid, capacity, "starting a worker for the channel");
        let (sender, receiver) = mpsc::channel(capacity);
        state.workers.insert(livepid.to_string(), sender.clone());
        tokio::spawn(worker_loop(
            Arc::downgrade(&self.shared),
            livepid.to_string(),
            receiver,
        ));
        sender
    }

    async fn fetch_media_playlist(&self, playback_url: &str) -> Result<UpstreamPlaylist> {
        let mut url = playback_url.to_string();
        for _depth in 0..3 {
            let response = self
                .shared
                .transport
                .send(HttpRequest::get(url.clone(), upstream_headers()))
                .await
                .map_err(|source| UpstreamError::Transport {
                    what: "upstream m3u8",
                    source,
                })?;
            let text = response.text();
            if !response.is_success() {
                return Err(UpstreamError::Status {
                    what: "upstream m3u8",
                    status: response.status,
                    body: body_head(&text, 300),
                });
            }
            let parsed = parse_media_playlist(&text, &url)?;
            if !parsed.segments.is_empty() {
                return Ok(UpstreamPlaylist {
                    segments: parsed.segments,
                });
            }
            let Some(next) = parsed.playlists.into_iter().next() else {
                return Err(UpstreamError::EmptyPlaylist(strip_query(&url)));
            };
            url = next;
        }
        Err(UpstreamError::PlaylistDepth(strip_query(playback_url)))
    }

    fn update_history(
        &self,
        channel: &Channel,
        playlist: &UpstreamPlaylist,
    ) -> Vec<SegmentRef> {
        let config = self.shared.config;
        let mut state = self.shared.state();
        let history = state.history.entry(channel.livepid.clone()).or_default();
        for segment in &playlist.segments {
            if history.iter().any(|item| item.sequence == segment.sequence) {
                continue;
            }
            history.push(SegmentRef {
                id: segment_id(&channel.livepid, segment.sequence),
                url: segment.url.clone(),
                duration: segment.duration,
                sequence: segment.sequence,
            });
        }
        history.sort_by_key(|segment| segment.sequence);
        if history.len() > config.history_max {
            let excess = history.len() - config.history_max;
            history.drain(..excess);
        }
        playable_window(history, config.window).to_vec()
    }

    fn history_window(&self, livepid: &str) -> Vec<SegmentRef> {
        let config = self.shared.config;
        self.shared
            .state()
            .history
            .get(livepid)
            .map(|history| playable_window(history, config.window).to_vec())
            .unwrap_or_default()
    }
}

struct UpstreamPlaylist {
    segments: Vec<iptv_media::ParsedSegment>,
}

fn upstream_headers() -> Vec<(String, String)> {
    vec![
        ("referer".to_string(), format!("{ACTIVE_URL}/")),
        ("user-agent".to_string(), USER_AGENT.to_string()),
    ]
}

/// URL without its query string, which carries upstream credentials.
fn strip_query(url: &str) -> String {
    url.split('?').next().unwrap_or_default().to_string()
}

#[derive(Clone)]
struct Processed {
    bytes: Bytes,
}

struct ChannelRuntime {
    cipher: Box<dyn PayloadCipher + Send>,
    video: VideoState,
    mux: MuxState,
    processed: BTreeMap<i64, Processed>,
    last_processed: Option<i64>,
}

struct Worker {
    livepid: String,
    runtime: Option<ChannelRuntime>,
    reset_count: u64,
}

async fn worker_loop(
    shared: Weak<Shared>,
    livepid: String,
    mut jobs: mpsc::Receiver<Job>,
) {
    let mut worker = Worker {
        livepid,
        runtime: None,
        reset_count: 0,
    };
    while let Some(job) = jobs.recv().await {
        let Some(shared) = shared.upgrade() else {
            break;
        };
        let started = std::time::Instant::now();
        let sequence = job.segment.segment.sequence;
        let result = worker.process(&shared, &job.segment.segment).await;
        match &result {
            Ok(bytes) => debug!(
                livepid = %worker.livepid,
                sequence,
                bytes = bytes.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "segment processed"
            ),
            Err(error) => warn!(
                livepid = %worker.livepid,
                sequence,
                elapsed_ms = started.elapsed().as_millis(),
                error = %error,
                "segment processing failed"
            ),
        }
        // The requester may have gone away; the result stays cached for the next one.
        let _ = job.reply.send(result);
    }
}

impl Worker {
    async fn process(&mut self, shared: &Shared, segment: &SegmentRef) -> Result<Bytes> {
        self.ensure_runtime(shared).await?;
        let sequence = segment.sequence;
        if let Some(cached) = self.runtime_ref()?.processed.get(&sequence) {
            return Ok(cached.bytes.clone());
        }
        let last = self.runtime_ref()?.last_processed;
        match last {
            None => {
                for predecessor in self.contiguous(shared, sequence) {
                    self.process_payload(shared, &predecessor).await?;
                }
            }
            Some(last) if sequence < last => {
                return Err(UpstreamError::SegmentTooOld { sequence, last });
            }
            Some(last) if sequence > last + 1 => {
                let missing = self.missing(shared, last + 1, sequence);
                let expected = usize::try_from(sequence - last - 1).unwrap_or(usize::MAX);
                if missing.len() == expected {
                    for predecessor in missing {
                        self.process_payload(shared, &predecessor).await?;
                    }
                } else {
                    warn!(livepid = %self.livepid, last, sequence, "sequence gap; resetting channel runtime");
                    self.reset(shared).await?;
                    for predecessor in self.contiguous(shared, sequence) {
                        self.process_payload(shared, &predecessor).await?;
                    }
                }
            }
            Some(_) => {}
        }
        Ok(self.process_payload(shared, segment).await?.bytes)
    }

    fn runtime_ref(&self) -> Result<&ChannelRuntime> {
        self.runtime.as_ref().ok_or(UpstreamError::WorkerStopped)
    }

    fn contiguous(&self, shared: &Shared, target: i64) -> Vec<SegmentRef> {
        let state = shared.state();
        state
            .history
            .get(&self.livepid)
            .map(|history| {
                contiguous_predecessors(history, target, shared.config.window.window)
            })
            .unwrap_or_default()
    }

    fn missing(&self, shared: &Shared, start: i64, target: i64) -> Vec<SegmentRef> {
        let state = shared.state();
        state
            .history
            .get(&self.livepid)
            .map(|history| missing_predecessors(history, start, target))
            .unwrap_or_default()
    }

    async fn ensure_runtime(&mut self, shared: &Shared) -> Result<()> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let ciphers = Arc::clone(&shared.ciphers);
        let livepid = self.livepid.clone();
        let started = std::time::Instant::now();
        let cipher = tokio::task::spawn_blocking(move || ciphers.start(&livepid))
            .await
            .map_err(|error| UpstreamError::Join(error.to_string()))??;
        info!(
            livepid = %self.livepid,
            reset_count = self.reset_count,
            elapsed_ms = started.elapsed().as_millis(),
            "created the channel's decrypt runtime"
        );
        self.runtime = Some(ChannelRuntime {
            cipher,
            video: VideoState::default(),
            mux: MuxState::default(),
            processed: BTreeMap::new(),
            last_processed: None,
        });
        Ok(())
    }

    async fn reset(&mut self, shared: &Shared) -> Result<()> {
        self.runtime = None;
        self.reset_count += 1;
        self.ensure_runtime(shared).await
    }

    async fn process_payload(
        &mut self,
        shared: &Shared,
        segment: &SegmentRef,
    ) -> Result<Processed> {
        if let Some(cached) = self.runtime_ref()?.processed.get(&segment.sequence) {
            return Ok(cached.clone());
        }
        let fetch_started = std::time::Instant::now();
        let response = shared
            .transport
            .send(HttpRequest::get(segment.url.clone(), upstream_headers()))
            .await
            .map_err(|source| {
                warn!(
                    livepid = %self.livepid,
                    sequence = segment.sequence,
                    url = %strip_query(&segment.url),
                    error = %source,
                    "upstream segment request failed"
                );
                UpstreamError::Transport {
                    what: "upstream segment",
                    source,
                }
            })?;
        debug!(
            livepid = %self.livepid,
            sequence = segment.sequence,
            url = %strip_query(&segment.url),
            status = response.status,
            bytes = response.body.len(),
            elapsed_ms = fetch_started.elapsed().as_millis(),
            "fetched the upstream segment"
        );
        if !response.is_success() {
            warn!(
                livepid = %self.livepid,
                sequence = segment.sequence,
                status = response.status,
                "upstream segment answered with an error status"
            );
            return Err(UpstreamError::Status {
                what: "upstream segment",
                status: response.status,
                body: body_head(&response.text(), 300),
            });
        }
        let input = response.body;
        let (output, stats) = self.remux(input, segment.sequence).await?;
        debug!(
            livepid = %self.livepid,
            sequence = segment.sequence,
            output_bytes = output.len(),
            video_samples = stats.video_sample_count,
            audio_samples = stats.audio_sample_count,
            decoded_nals = stats.decoded_nals,
            reset_count = self.reset_count,
            "decrypted and remuxed TS segment"
        );
        let processed = Processed {
            bytes: Bytes::from(output),
        };
        let history_max = shared.config.history_max.max(1);
        let runtime = self.runtime.as_mut().ok_or(UpstreamError::WorkerStopped)?;
        runtime
            .processed
            .insert(segment.sequence, processed.clone());
        while runtime.processed.len() > history_max {
            runtime.processed.pop_first();
        }
        runtime.last_processed = Some(
            runtime
                .last_processed
                .map_or(segment.sequence, |last| last.max(segment.sequence)),
        );
        Ok(processed)
    }

    /// Runs the CPU-bound decrypt and remux off the async executor. The runtime is moved
    /// into the blocking task and back, so no lock is needed.
    async fn remux(
        &mut self,
        input: Bytes,
        sequence: i64,
    ) -> Result<(Vec<u8>, RemuxStats)> {
        let mut runtime = self.runtime.take().ok_or(UpstreamError::WorkerStopped)?;
        let joined = tokio::task::spawn_blocking(move || {
            let result = decrypt_and_remux(
                &mut runtime.cipher,
                &mut runtime.video,
                &mut runtime.mux,
                &input,
            );
            (runtime, result)
        })
        .await;
        match joined {
            Ok((runtime, result)) => {
                self.runtime = Some(runtime);
                result.map_err(|source| UpstreamError::Media { sequence, source })
            }
            // The runtime is lost with the failed task; the next job starts a new one.
            Err(error) => Err(UpstreamError::Join(error.to_string())),
        }
    }
}

/// Newest segments directly before `target`, oldest first, at most `window - 1` long and
/// without gaps.
fn contiguous_predecessors(
    history: &[SegmentRef],
    target: i64,
    window: usize,
) -> Vec<SegmentRef> {
    let mut result = Vec::new();
    let mut expected = target - 1;
    for segment in history.iter().rev() {
        if segment.sequence >= target {
            continue;
        }
        if segment.sequence == expected {
            result.push(segment.clone());
            expected -= 1;
            if result.len() >= window.saturating_sub(1) {
                break;
            }
        } else if segment.sequence < expected {
            break;
        }
    }
    result.reverse();
    result
}

/// Segments `start..target` in order, stopping at the first one the history lacks.
fn missing_predecessors(
    history: &[SegmentRef],
    start: i64,
    target: i64,
) -> Vec<SegmentRef> {
    let mut result = Vec::new();
    for sequence in start..target {
        let Some(segment) = history.iter().find(|segment| segment.sequence == sequence)
        else {
            break;
        };
        result.push(segment.clone());
    }
    result
}

fn new_media_tag_id() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

#[cfg(test)]
mod tests;
