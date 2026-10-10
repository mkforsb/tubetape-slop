//! Long-running work: polite downloading, announcement synthesis and the
//! final ffmpeg render. Runs as a stream of [`JobEvent`]s so the UI can follow
//! along; dropping the stream cancels everything (children are kill-on-drop).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use iced::futures::channel::mpsc::Sender;
use iced::futures::{SinkExt, Stream};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::model::{Settings, fmt_duration};
use crate::tools::{self, FailureKind};

#[derive(Debug, Clone)]
pub enum JobEvent {
    Status(String),
    DownloadStarted(String),
    DownloadProgress(String, f32),
    Downloaded(String, PathBuf),
    /// A download failed and will be retried; the text says when.
    DownloadRetrying(String, String),
    DownloadFailed(String, String),
    /// Overall progress of the current stage, 0.0..=1.0.
    Progress(f32),
    Log(String),
    Finished(Result<Option<PathBuf>, String>),
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub announcement: String,
    pub audio: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Render {
    pub output: PathBuf,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub settings: Settings,
    pub audio_dir: PathBuf,
    pub tts_dir: PathBuf,
    pub items: Vec<Item>,
    /// When the previous download (from any job) started or finished, so
    /// back-to-back jobs still respect the delay between downloads.
    pub last_download: Option<Instant>,
    /// `None` = just download the audio.
    pub render: Option<Render>,
}

type Events = Sender<JobEvent>;

pub fn run(plan: Plan) -> impl Stream<Item = JobEvent> {
    iced::stream::channel(64, async move |mut tx: Events| {
        let result = execute(plan, &mut tx).await;
        let _ = tx.send(JobEvent::Finished(result)).await;
    })
}

async fn send(tx: &mut Events, event: JobEvent) {
    let _ = tx.send(event).await;
}

async fn execute(mut plan: Plan, tx: &mut Events) -> Result<Option<PathBuf>, String> {
    download_missing(&mut plan, tx).await?;
    let Some(render) = plan.render.take() else {
        send(tx, JobEvent::Status("All selected tracks are downloaded.".into())).await;
        return Ok(None);
    };
    render_mixtape(&plan, &render, tx).await?;
    Ok(Some(render.output))
}

/// Cheap jitter without an RNG dependency: a value in `min..=max`.
fn jitter(min: f32, max: f32) -> f32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let unit = (nanos.wrapping_mul(2_654_435_761) % 10_000) as f32 / 10_000.0;
    min + (max - min) * unit
}

/// Backoff before retrying a download that failed for no particular reason.
const RETRY_BACKOFF_SECS: [f32; 3] = [15.0, 45.0, 120.0];
/// Backoff after YouTube says "slow down". If it's still unhappy after these,
/// the whole job stops rather than pushing on.
const THROTTLE_BACKOFF_SECS: [f32; 2] = [300.0, 900.0];

/// Sleeps for `secs`, updating the status line with a countdown.
async fn countdown(tx: &mut Events, secs: f32, what: &str) {
    let until = Instant::now() + Duration::from_secs_f32(secs);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        send(
            tx,
            JobEvent::Status(format!("{what} in {}…", fmt_duration(left.as_secs_f64().ceil()))),
        )
        .await;
        tokio::time::sleep(left.min(Duration::from_secs(1))).await;
    }
}

/// Downloads one track, retrying transient failures with exponential
/// backoff (plus jitter) and backing off for minutes when throttled.
async fn download_with_retries(plan: &mut Plan, i: usize, tx: &mut Events) -> Result<PathBuf, tools::ToolError> {
    let settings = plan.settings.clone();
    let (id, title) = (plan.items[i].id.clone(), plan.items[i].title.clone());
    let (mut retries, mut throttles) = (0, 0);

    loop {
        send(tx, JobEvent::DownloadStarted(id.clone())).await;
        // Progress callbacks are sync; forward them through a cloned sender.
        let mut progress_tx = tx.clone();
        let progress_id = id.clone();
        let result = tools::download_audio(&settings, &id, &plan.audio_dir, |p| {
            let _ = progress_tx.try_send(JobEvent::DownloadProgress(progress_id.clone(), p));
        })
        .await;
        plan.last_download = Some(Instant::now());

        let error = match result {
            Ok(path) => return Ok(path),
            Err(e) => e,
        };
        let backoff = match error.kind {
            FailureKind::Permanent => None,
            FailureKind::Transient => RETRY_BACKOFF_SECS.get(retries).inspect(|_| retries += 1),
            FailureKind::Throttled => THROTTLE_BACKOFF_SECS.get(throttles).inspect(|_| throttles += 1),
        };
        let Some(&base) = backoff else { return Err(error) };

        let wait = base * jitter(1.0, 1.5);
        let reason = match error.kind {
            FailureKind::Throttled => "YouTube asked us to slow down",
            _ => "download failed",
        };
        send(
            tx,
            JobEvent::Log(format!(
                "{title}: {reason}, retrying in {}. ({})",
                fmt_duration(wait.into()),
                error.message
            )),
        )
        .await;
        send(
            tx,
            JobEvent::DownloadRetrying(id.clone(), format!("retry in {}", fmt_duration(wait.into()))),
        )
        .await;
        countdown(tx, wait, &format!("{title}: {reason}. Retrying")).await;
    }
}

async fn download_missing(plan: &mut Plan, tx: &mut Events) -> Result<(), String> {
    let (min_delay, max_delay) = plan.settings.delay_range();
    let missing: Vec<usize> = (0..plan.items.len())
        .filter(|&i| plan.items[i].audio.is_none())
        .collect();
    let mut failures = Vec::new();

    for (n, &i) in missing.iter().enumerate() {
        if let Some(last) = plan.last_download {
            let delay = Duration::from_secs_f32(jitter(min_delay, max_delay));
            let elapsed = last.elapsed();
            if elapsed < delay {
                countdown(
                    tx,
                    (delay - elapsed).as_secs_f32(),
                    "Being nice to YouTube: next download",
                )
                .await;
            }
        }

        let title = plan.items[i].title.clone();
        send(
            tx,
            JobEvent::Status(format!("Downloading {}/{}: {title}", n + 1, missing.len())),
        )
        .await;
        send(tx, JobEvent::Progress(n as f32 / missing.len() as f32)).await;

        match download_with_retries(plan, i, tx).await {
            Ok(path) => {
                plan.items[i].audio = Some(path.clone());
                send(tx, JobEvent::Downloaded(plan.items[i].id.clone(), path)).await;
            }
            Err(e) => {
                send(
                    tx,
                    JobEvent::DownloadFailed(plan.items[i].id.clone(), e.message.clone()),
                )
                .await;
                if e.kind == FailureKind::Throttled {
                    return Err(format!(
                        "YouTube is still asking us to slow down, so downloading has stopped. \
                         Try again later, or raise the pauses in Settings.\n{}",
                        e.message
                    ));
                }
                send(tx, JobEvent::Log(format!("Giving up on {title}: {}", e.message))).await;
                failures.push(title);
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} download(s) failed: {}. Hover \"failed\" for details, or use \"Remove failed\".",
            failures.len(),
            failures.join(", ")
        ))
    }
}

/// FNV-1a; stable across builds, unlike `DefaultHasher`, so it's fine for cache keys.
fn fnv1a(data: &str) -> u64 {
    data.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Synthesizes (or reuses a cached copy of) an announcement.
pub async fn announcement_wav(settings: &Settings, tts_dir: &Path, text: &str) -> Result<PathBuf, String> {
    let key = fnv1a(&format!("{}\n{}", settings.tts_command, text));
    let path = tts_dir.join(format!("{key:016x}.wav"));
    if !path.exists() {
        let tmp = tts_dir.join(format!("{key:016x}.tmp.wav"));
        tools::synthesize(settings, text, &tmp).await?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

struct Segment {
    voice: PathBuf,
    voice_secs: f64,
    /// dB to apply to the voice to hit the configured loudness.
    voice_gain: f64,
    music: PathBuf,
    music_secs: f64,
}

async fn render_mixtape(plan: &Plan, render: &Render, tx: &mut Events) -> Result<(), String> {
    let s = &plan.settings;
    let (pre, post) = (s.gap_before_voice(), s.gap_after_voice());
    let count = plan.items.len();

    // 1. Announcements + exact durations (needed for chapters and progress).
    let mut segments = Vec::with_capacity(count);
    for (n, item) in plan.items.iter().enumerate() {
        send(
            tx,
            JobEvent::Status(format!("Preparing announcement {}/{count}…", n + 1)),
        )
        .await;
        send(tx, JobEvent::Progress(n as f32 / count as f32)).await;
        let music = item
            .audio
            .clone()
            .ok_or_else(|| format!("{} isn't downloaded", item.title))?;
        let voice = announcement_wav(s, &plan.tts_dir, &item.announcement).await?;
        segments.push(Segment {
            voice_secs: tools::probe_duration(s, &voice).await? + pre + post,
            voice_gain: f64::from(s.voice_lufs()) - tools::measure_lufs(s, &voice).await?,
            music_secs: tools::probe_duration(s, &music).await?,
            voice,
            music,
        });
    }
    let total: f64 = segments.iter().map(|g| g.voice_secs + g.music_secs).sum();

    // 2. Chapters (one per track, starting at its announcement) + tracklist.
    let mut metadata = String::from(";FFMETADATA1\n");
    let _ = writeln!(metadata, "title={}", ffmeta_escape(&render.title));
    let mut tracklist = format!("{}\n\n", render.title);
    let mut at = 0.0;
    for (item, seg) in plan.items.iter().zip(&segments) {
        let end = at + seg.voice_secs + seg.music_secs;
        let _ = write!(
            metadata,
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle={}\n",
            (at * 1000.0) as u64,
            (end * 1000.0) as u64,
            ffmeta_escape(&item.title)
        );
        let _ = writeln!(tracklist, "{}  {}", fmt_duration(at), item.title);
        at = end;
    }
    let work_dir = plan.tts_dir.parent().unwrap_or(&plan.tts_dir).join("work");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;
    let metadata_path = work_dir.join("chapters.ffmeta");
    std::fs::write(&metadata_path, metadata).map_err(|e| e.to_string())?;

    // 3. One ffmpeg pass: bring each announcement to the target loudness
    //    (measured gain + limiter; single-pass loudnorm is unreliable on
    //    clips this short), add the gaps, resample everything to 48 kHz
    //    stereo and concatenate.
    let mut filter = String::new();
    let mut labels = String::new();
    let fmt = "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo";
    for (i, seg) in segments.iter().enumerate() {
        let (v, m) = (2 * i, 2 * i + 1);
        let _ = write!(
            filter,
            "[{v}:a]aresample=48000,{fmt},volume={gain:.2}dB,alimiter=limit=0.89:level=false,\
             adelay=delays={pre_ms}:all=1,apad=pad_dur={post}[v{i}];\
             [{m}:a]aresample=48000,{fmt}[m{i}];",
            gain = seg.voice_gain.clamp(-30.0, 30.0),
            pre_ms = (pre * 1000.0).round() as u64,
        );
        let _ = write!(labels, "[v{i}][m{i}]");
    }
    let _ = write!(filter, "{labels}concat=n={}:v=0:a=1[out]", 2 * count);

    let partial = render.output.with_extension(format!(
        "{}.partial",
        render.output.extension().and_then(|e| e.to_str()).unwrap_or("ogg")
    ));
    let mut cmd = Command::new(&s.ffmpeg);
    cmd.kill_on_drop(true).stdin(Stdio::null()).args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-loglevel",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
    ]);
    for seg in &segments {
        cmd.arg("-i").arg(&seg.voice).arg("-i").arg(&seg.music);
    }
    let meta_index = (2 * count).to_string();
    cmd.args(["-f", "ffmetadata", "-i"])
        .arg(&metadata_path)
        .args(["-filter_complex", &filter, "-map", "[out]"])
        .args(["-map_metadata", &meta_index, "-map_chapters", &meta_index])
        .args(["-c:a", s.codec.encoder(), "-b:a", &format!("{}k", s.bitrate_kbps())])
        .args(["-f", "ogg"])
        .arg(&partial)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    send(
        tx,
        JobEvent::Status(format!("Rendering {} of audio…", fmt_duration(total))),
    )
    .await;
    send(tx, JobEvent::Progress(0.0)).await;
    let mut child = cmd.spawn().map_err(|e| format!("Failed to run {}: {e}", s.ffmpeg))?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut out = String::new();
        let _ = stderr.read_to_string(&mut out).await;
        out
    });
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(us) = line
            .strip_prefix("out_time_us=")
            .and_then(|v| v.trim().parse::<f64>().ok())
        {
            send(
                tx,
                JobEvent::Progress((us / 1e6 / total.max(1.0)).clamp(0.0, 1.0) as f32),
            )
            .await;
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("ffmpeg failed:\n{}", stderr.trim()));
    }

    std::fs::rename(&partial, &render.output).map_err(|e| e.to_string())?;
    let tracklist_path = render.output.with_extension("txt");
    if let Err(e) = std::fs::write(&tracklist_path, tracklist) {
        send(tx, JobEvent::Log(format!("Couldn't write tracklist: {e}"))).await;
    }
    send(tx, JobEvent::Progress(1.0)).await;
    send(
        tx,
        JobEvent::Status(format!("Done: {} ({})", render.output.display(), fmt_duration(total))),
    )
    .await;
    Ok(())
}

/// Escapes a value for ffmpeg's ffmetadata format.
fn ffmeta_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            let j = jitter(8.0, 20.0);
            assert!((8.0..=20.0).contains(&j));
        }
        assert_eq!(jitter(5.0, 5.0), 5.0);
    }

    #[test]
    fn escapes_ffmetadata() {
        assert_eq!(ffmeta_escape("a=b;c#d\\e"), "a\\=b\\;c\\#d\\\\e");
    }
}

/// End-to-end check against real YouTube: `cargo test -- --ignored`.
/// Set TUBETAPE_YTDLP if yt-dlp isn't on PATH.
#[cfg(test)]
mod e2e {
    use super::*;
    use iced::futures::StreamExt;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads from YouTube"]
    async fn builds_a_two_track_mixtape() {
        let root = std::env::temp_dir().join("tubetape-e2e");
        let (audio_dir, tts_dir) = (root.join("audio"), root.join("tts"));
        std::fs::create_dir_all(&audio_dir).unwrap();
        std::fs::create_dir_all(&tts_dir).unwrap();

        let mut settings = Settings {
            min_delay: "1".into(),
            max_delay: "2".into(),
            ..Settings::default()
        };
        if let Ok(ytdlp) = std::env::var("TUBETAPE_YTDLP") {
            settings.ytdlp = ytdlp;
        }
        let item = |id: &str, title: &str| Item {
            id: id.into(),
            title: title.into(),
            announcement: crate::model::announcement_from_title("{artist}. {track}.", title),
            audio: None,
        };
        let output = root.join("e2e mixtape.opus");
        let plan = Plan {
            settings,
            audio_dir,
            tts_dir,
            items: vec![
                item("DptNJPDmlB8", "M-Paths -- Sirens"),
                item("Xw5lgXg6T_I", "M-Paths -- Boy Cat's Nocturnal Stealth Mode"),
            ],
            last_download: None,
            render: Some(Render {
                output: output.clone(),
                title: "e2e mixtape".into(),
            }),
        };

        let events: Vec<JobEvent> = run(plan).collect().await;
        for e in &events {
            if !matches!(e, JobEvent::DownloadProgress(..) | JobEvent::Progress(_)) {
                eprintln!("{e:?}");
            }
        }
        match events.last() {
            Some(JobEvent::Finished(Ok(Some(path)))) => assert_eq!(path, &output),
            other => panic!("job didn't finish cleanly: {other:?}"),
        }
        assert!(output.exists());
        assert!(output.with_extension("txt").exists());
    }
}
