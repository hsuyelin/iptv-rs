//! The compatibility rendition: a lighter stream for devices that cannot keep up.
//!
//! The relay's normal stream is 1080p High profile with B-frames and one keyframe per
//! 5-second segment. Older Apple devices decode that as a slideshow (audio plays on, the
//! picture freezes until the next keyframe) or not at all. This module re-encodes a segment
//! to 720p Main profile without B-frames and with a keyframe at least every 2 seconds, and
//! leaves everything else alone: the audio is copied untouched and every timestamp is kept,
//! so consecutive segments still join to the millisecond. Each encode is a separate process,
//! and every process numbers its transport stream packets from zero, so [`Compat`] renumbers
//! them to carry on from the previous segment of the channel, as the relay's own muxer does.
//!
//! Encoding is done by an external `ffmpeg`, one process per segment, behind the
//! [`Transcoder`] trait so the rest of the server never needs the binary to be present.
//! [`Compat`] adds a small cache with single-flight: however many players ask for the same
//! segment, it is encoded once.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use bytes::Bytes;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{OnceCell, Semaphore},
};

/// Longest tail of ffmpeg's error output kept for a log line.
const STDERR_KEEP: usize = 2048;
/// First byte of every transport stream packet.
const TS_SYNC_BYTE: u8 = 0x47;

/// Length of a transport stream packet.
const TS_PACKET: usize = 188;

/// Renumbers the continuity counters of `ts` so that each PID carries on from `next`.
///
/// `next` holds, per PID, the counter the next packet with a payload must carry. A PID seen
/// for the first time keeps the number it came with. Packets without a payload do not count
/// and repeat the number before them, as the standard asks.
pub(crate) fn carry_continuity(ts: &mut [u8], next: &mut HashMap<u16, u8>) {
    let (packets, _partial_tail) = ts.as_chunks_mut::<TS_PACKET>();
    for packet in packets {
        let [sync, high, low, flags, ..] = packet;
        if *sync != TS_SYNC_BYTE {
            continue;
        }
        let pid = (u16::from(*high & 0x1f) << 8) | u16::from(*low);
        if *flags & 0x10 != 0 {
            let counter = next.get(&pid).copied().unwrap_or(*flags & 0x0f);
            *flags = (*flags & 0xf0) | counter;
            next.insert(pid, (counter + 1) & 0x0f);
        } else if let Some(upcoming) = next.get(&pid) {
            *flags = (*flags & 0xf0) | (upcoming.wrapping_sub(1) & 0x0f);
        }
    }
}

/// A boxed future, so [`Transcoder`] can be used as a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a segment could not be re-encoded.
#[derive(Debug, Error)]
pub enum CompatError {
    /// The encoder program could not be started.
    #[error("cannot start {program}: {source}")]
    Spawn {
        /// The program that was run.
        program: String,
        /// What the operating system said.
        source: io::Error,
    },
    /// Reading or writing the encoder's pipes failed.
    #[error("encoder I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The encoder ran past its time limit and was stopped.
    #[error("encoder did not finish within {0:?}")]
    Timeout(Duration),
    /// The encoder exited with an error.
    #[error("encoder failed ({status}): {stderr}")]
    Failed {
        /// Exit status as text.
        status: String,
        /// The end of its error output.
        stderr: String,
    },
    /// The encoder finished but did not produce a transport stream.
    #[error("encoder produced no usable transport stream")]
    BadOutput,
    /// The encoder lacks the H.264 encoder this module needs.
    #[error("the encoder program has no libx264 support")]
    MissingEncoder,
}

/// How the compatibility rendition is encoded.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Largest picture height; taller pictures are scaled down, shorter ones are kept.
    pub height: u32,
    /// Target video bit rate, in kilobits per second.
    pub video_kbps: u32,
    /// Frames between keyframes: 50 is 2 seconds at 25 frames per second.
    pub gop_frames: u32,
    /// Threads one encode may use.
    pub threads: u32,
    /// Longest one segment may take.
    pub timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            height: 720,
            video_kbps: 2500,
            gop_frames: 50,
            threads: 2,
            timeout: Duration::from_secs(30),
        }
    }
}

impl Settings {
    /// The H.264 level that fits `height` at up to 30 frames per second.
    fn level(&self) -> &'static str {
        match self.height {
            0..=576 => "3.0",
            577..=720 => "3.1",
            _ => "4.0",
        }
    }
}

/// The command line that turns one transport stream on stdin into another on stdout.
///
/// `-copyts` with a zero mux delay keeps the original timestamps, `passthrough` stops
/// frames being duplicated or dropped, and `-c:a copy` leaves the audio exactly as it was.
pub fn ffmpeg_args(settings: &Settings) -> Vec<String> {
    let kbps = settings.video_kbps.max(100);
    let strings =
        |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();
    let mut args = strings(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "mpegts",
        "-i",
        "pipe:0",
        "-map",
        "0:v:0",
        "-map",
        "0:a:0",
    ]);
    args.push("-vf".into());
    args.push(format!("scale=-2:'min({},ih)'", settings.height));
    args.extend(strings(&[
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-profile:v",
        "main",
        "-level",
        settings.level(),
        "-pix_fmt",
        "yuv420p",
        "-bf",
        "0",
    ]));
    for (flag, value) in [
        ("-g", settings.gop_frames.to_string()),
        ("-keyint_min", settings.gop_frames.to_string()),
        ("-sc_threshold", "0".to_string()),
        ("-b:v", format!("{kbps}k")),
        ("-maxrate", format!("{}k", kbps.saturating_mul(6) / 5)),
        ("-bufsize", format!("{}k", kbps.saturating_mul(2))),
        ("-threads", settings.threads.max(1).to_string()),
    ] {
        args.push(flag.into());
        args.push(value);
    }
    args.extend(strings(&[
        "-c:a",
        "copy",
        "-copyts",
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
        "-fps_mode",
        "passthrough",
        "-f",
        "mpegts",
        "pipe:1",
    ]));
    args
}

/// Re-encodes one transport stream segment.
pub trait Transcoder: Send + Sync {
    /// Returns the compatibility rendition of `input`.
    fn transcode(&self, input: Bytes) -> BoxFuture<'_, Result<Bytes, CompatError>>;
}

/// A [`Transcoder`] that runs `ffmpeg`.
#[derive(Debug, Clone)]
pub struct FfmpegTranscoder {
    program: PathBuf,
    settings: Settings,
}

impl FfmpegTranscoder {
    /// Uses the `ffmpeg` at `program` with `settings`.
    pub fn new(program: PathBuf, settings: Settings) -> Self {
        Self { program, settings }
    }

    /// Checks that the program starts and can encode H.264, so a wrong path or a build
    /// without libx264 is caught when the server starts, not on the first viewer.
    ///
    /// # Errors
    /// [`CompatError::Spawn`] when the program does not start, [`CompatError::Failed`] when
    /// it exits with an error, [`CompatError::MissingEncoder`] when it has no libx264.
    pub async fn check(&self) -> Result<(), CompatError> {
        let output = Command::new(&self.program)
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| self.spawn_error(source))?;
        if !output.status.success() {
            return Err(CompatError::Failed {
                status: output.status.to_string(),
                stderr: tail(&output.stderr),
            });
        }
        if String::from_utf8_lossy(&output.stdout).contains("libx264") {
            Ok(())
        } else {
            Err(CompatError::MissingEncoder)
        }
    }

    fn spawn_error(&self, source: io::Error) -> CompatError {
        CompatError::Spawn {
            program: self.program.display().to_string(),
            source,
        }
    }

    async fn run(&self, input: Bytes) -> Result<Bytes, CompatError> {
        let mut child = Command::new(&self.program)
            .args(ffmpeg_args(&self.settings))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| self.spawn_error(source))?;
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(CompatError::Io(io::Error::other(
                "encoder pipes are missing",
            )));
        };

        // Feed, collect and drain at the same time: a pipe that is only written to, or only
        // read, would stall the encoder as soon as its buffer filled.
        let feed = async move {
            match stdin.write_all(&input).await {
                // The encoder may stop reading when it fails; its exit status tells why.
                Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
                _ => Ok(()),
            }
        };
        let collect = async move {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).await.map(|_| out)
        };
        let diagnostics = async move {
            let mut kept = Vec::new();
            (&mut stderr).take(64 * 1024).read_to_end(&mut kept).await?;
            tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
            Ok::<_, io::Error>(kept)
        };
        let (fed, collected, noted) = tokio::join!(feed, collect, diagnostics);
        let status = child.wait().await?;
        if !status.success() {
            return Err(CompatError::Failed {
                status: status.to_string(),
                stderr: tail(&noted.unwrap_or_default()),
            });
        }
        fed?;
        let out = collected?;
        if out.first() != Some(&TS_SYNC_BYTE) {
            return Err(CompatError::BadOutput);
        }
        Ok(Bytes::from(out))
    }
}

impl Transcoder for FfmpegTranscoder {
    fn transcode(&self, input: Bytes) -> BoxFuture<'_, Result<Bytes, CompatError>> {
        Box::pin(async move {
            tokio::time::timeout(self.settings.timeout, self.run(input))
                .await
                .map_err(|_| CompatError::Timeout(self.settings.timeout))?
        })
    }
}

/// The last [`STDERR_KEEP`] bytes of `bytes`, as text.
fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_KEEP);
    String::from_utf8_lossy(bytes.get(start..).unwrap_or(&[]))
        .trim()
        .to_string()
}

/// Why [`Compat::segment`] produced nothing.
#[derive(Debug)]
pub enum CompatFailure<E> {
    /// The original segment could not be had.
    Original(E),
    /// The original was had, but re-encoding it failed.
    Transcode(CompatError),
}

struct Cells {
    map: HashMap<String, Arc<OnceCell<Bytes>>>,
    order: VecDeque<String>,
    capacity: usize,
}

/// Re-encoded segments, remembered for a short while and encoded once however many ask.
pub struct Compat {
    transcoder: Arc<dyn Transcoder>,
    slots: Semaphore,
    cells: Mutex<Cells>,
    /// Per channel and PID: the continuity counter the next packet carries.
    counters: Mutex<HashMap<String, HashMap<u16, u8>>>,
}

impl Compat {
    /// Remembers up to `capacity` segments and runs at most `parallel` encodes at once.
    pub fn new(
        transcoder: Arc<dyn Transcoder>,
        parallel: usize,
        capacity: usize,
    ) -> Self {
        Self {
            transcoder,
            slots: Semaphore::new(parallel.max(1)),
            cells: Mutex::new(Cells {
                map: HashMap::new(),
                order: VecDeque::new(),
                capacity: capacity.max(1),
            }),
            counters: Mutex::new(HashMap::new()),
        }
    }

    /// Bookkeeping only: the guard is never held across an await.
    fn cells(&self) -> MutexGuard<'_, Cells> {
        self.cells.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cell(&self, key: &str) -> Arc<OnceCell<Bytes>> {
        let mut cells = self.cells();
        if let Some(existing) = cells.map.get(key) {
            return Arc::clone(existing);
        }
        let fresh = Arc::new(OnceCell::new());
        cells.map.insert(key.to_string(), Arc::clone(&fresh));
        cells.order.push_back(key.to_string());
        while cells.map.len() > cells.capacity {
            let Some(oldest) = cells.order.pop_front() else {
                break;
            };
            cells.map.remove(&oldest);
        }
        fresh
    }

    /// The compatibility rendition of segment `id` of `channel`.
    ///
    /// `original` fetches the normal segment and runs only when the segment is not
    /// remembered, so a cached segment costs nothing upstream. A failure is not remembered:
    /// the next request tries again.
    ///
    /// # Errors
    /// [`CompatFailure::Original`] with the error `original` gave, or
    /// [`CompatFailure::Transcode`] when re-encoding failed.
    pub async fn segment<E, F, Fut>(
        &self,
        channel: &str,
        id: &str,
        original: F,
    ) -> Result<Bytes, CompatFailure<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, E>>,
    {
        let cell = self.cell(&format!("{channel}:{id}"));
        cell.get_or_try_init(|| async {
            let input = original().await.map_err(CompatFailure::Original)?;
            let _slot = self.slots.acquire().await.map_err(|_| {
                CompatFailure::Transcode(CompatError::Io(io::Error::other(
                    "the encoder queue is closed",
                )))
            })?;
            let encoded = self
                .transcoder
                .transcode(input)
                .await
                .map_err(CompatFailure::Transcode)?;
            Ok(self.renumbered(channel, &encoded))
        })
        .await
        .cloned()
    }

    /// `encoded` with its continuity counters carried on from the channel's last segment.
    fn renumbered(&self, channel: &str, encoded: &Bytes) -> Bytes {
        let mut out = encoded.to_vec();
        let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        carry_continuity(&mut out, counters.entry(channel.to_string()).or_default());
        Bytes::from(out)
    }
}

#[cfg(test)]
mod tests;
