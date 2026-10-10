//! Thin async wrappers around the external programs we drive:
//! yt-dlp, ffprobe, the TTS engine and the preview player.
//!
//! Every child is spawned with `kill_on_drop`, so aborting the iced task that
//! owns one of these futures also kills the process.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::model::{Settings, Video, watch_url};

/// Builds a command from a user-supplied template such as
/// `espeak-ng --stdin -w {out}`. The template is split shell-style first and
/// placeholders are substituted per argument, so values never get re-parsed
/// by a shell.
pub fn command_from_template(template: &str, vars: &[(&str, &str)]) -> Result<Command, String> {
    let parts = shlex::split(template).ok_or_else(|| format!("Can't parse command: {template}"))?;
    let mut parts = parts.into_iter().map(|mut part| {
        for (key, value) in vars {
            part = part.replace(&format!("{{{key}}}"), value);
        }
        part
    });
    let program = parts.next().ok_or("Empty command")?;
    let mut cmd = Command::new(program);
    cmd.args(parts).kill_on_drop(true).stdin(Stdio::null());
    Ok(cmd)
}

fn base_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.kill_on_drop(true).stdin(Stdio::null());
    cmd
}

fn spawn_error(program: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("`{program}` not found. Is it installed and on PATH?")
    } else {
        format!("Failed to run `{program}`: {e}")
    }
}

/// Last few meaningful lines of a tool's stderr, for error messages.
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join("\n")
}

/// Checks that the configured tools can be started. Returns the problems found.
pub async fn check_tools(settings: Settings) -> Vec<String> {
    let mut problems = Vec::new();
    let checks = [
        (settings.ytdlp.clone(), "--version"),
        (settings.ffmpeg.clone(), "-version"),
        (settings.ffprobe.clone(), "-version"),
    ];
    for (program, arg) in checks {
        if let Err(e) = base_command(&program).arg(arg).output().await {
            problems.push(spawn_error(&program, e));
        }
    }
    for template in [&settings.tts_command, &settings.player_command] {
        let program = shlex::split(template).and_then(|p| p.into_iter().next());
        match program {
            Some(program) => {
                if let Err(e) = base_command(&program).arg("--help").output().await {
                    problems.push(spawn_error(&program, e));
                }
            }
            None => problems.push(format!("Can't parse command: {template}")),
        }
    }
    problems
}

/// Accepts `@handle`, a bare channel URL or a specific tab, and points it at
/// the channel's "Videos" tab (newest first) when no tab was given.
pub fn normalize_channel_url(input: &str) -> String {
    let input = input.trim().trim_end_matches('/');
    if input.starts_with('@') {
        return format!("https://www.youtube.com/{input}/videos");
    }
    let is_channel_root = ["/@", "/channel/", "/c/", "/user/"].iter().any(|marker| {
        input
            .find(marker)
            .is_some_and(|i| !input[i + marker.len()..].contains('/'))
    });
    if is_channel_root {
        format!("{input}/videos")
    } else {
        input.to_string()
    }
}

/// Fetches the (flat, cheap) list of uploads for a channel tab or playlist.
/// Returns the channel name and the videos in the order YouTube lists them,
/// which for a channel's Videos tab is newest first.
pub async fn fetch_listing(settings: Settings, url: String) -> Result<(String, Vec<Video>), String> {
    let listing = with_retries(&[5, 20], || fetch_listing_once(&settings, &url)).await;
    listing.map_err(|e| match e.kind {
        FailureKind::Throttled => format!(
            "YouTube is rate limiting us. Give it a while before fetching again.\n{}",
            e.message
        ),
        _ => format!("Couldn't list {url}:\n{}", e.message),
    })
}

async fn fetch_listing_once(settings: &Settings, url: &str) -> Result<(String, Vec<Video>), ToolError> {
    let output = base_command(&settings.ytdlp)
        .args(["--flat-playlist", "-J", "--no-warnings"])
        .args(["--playlist-end", &settings.fetch_limit().to_string()])
        .args(["--sleep-requests", &settings.sleep_requests().to_string()])
        .args(settings.extra_args())
        .arg("--")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ToolError::permanent(spawn_error(&settings.ytdlp, e)))?;

    if !output.status.success() {
        return Err(ToolError::from_stderr(&String::from_utf8_lossy(&output.stderr)));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| ToolError::permanent(format!("Bad yt-dlp output: {e}")))?;
    let channel = ["channel", "uploader", "title"]
        .iter()
        .find_map(|k| json[k].as_str())
        .unwrap_or("YouTube")
        .trim_end_matches(" - Videos")
        .to_string();

    let videos = json["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                // Skip nested playlists (e.g. a channel root lists its tabs).
                .filter(|e| e["_type"].as_str() != Some("playlist"))
                .filter_map(|e| {
                    Some(Video {
                        id: e["id"].as_str()?.to_string(),
                        title: e["title"].as_str().unwrap_or("(untitled)").to_string(),
                        duration: e["duration"].as_f64(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if videos.is_empty() {
        return Err(ToolError::permanent(format!("No videos found at {url}")));
    }
    Ok((channel, videos))
}

/// How a failed yt-dlp run should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Network hiccup or similar; worth retrying after a short backoff.
    Transient,
    /// YouTube told us to back off (HTTP 429 or a bot check). Wait a long time.
    Throttled,
    /// Retrying won't help: the video is private, removed, blocked, ...
    Permanent,
}

/// Classifies yt-dlp's stderr.
pub fn classify_failure(stderr: &str) -> FailureKind {
    let lower = stderr.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if any(&["429", "too many requests", "not a bot", "rate-limited", "rate limited"]) {
        FailureKind::Throttled
    } else if any(&[
        "video unavailable",
        "video is unavailable",
        "is no longer available",
        "private video",
        "has been removed",
        "copyright",
        "not available in your country",
        "members-only",
        "join this channel",
        "confirm your age",
        "age-restricted",
        "premieres in",
        "this live event",
        "requested format is not available",
        "unsupported url",
        "is not a valid url",
        "incomplete youtube id",
    ]) {
        FailureKind::Permanent
    } else {
        FailureKind::Transient
    }
}

#[derive(Debug, Clone)]
pub struct ToolError {
    pub message: String,
    pub kind: FailureKind,
}

impl ToolError {
    fn permanent(message: String) -> ToolError {
        ToolError {
            message,
            kind: FailureKind::Permanent,
        }
    }

    fn from_stderr(stderr: &str) -> ToolError {
        let message = tail(stderr);
        let message = if message.is_empty() {
            "yt-dlp failed without saying why".into()
        } else {
            message
        };
        ToolError {
            message,
            kind: classify_failure(stderr),
        }
    }
}

/// Runs `attempt` until it succeeds, fails permanently or runs out of retries.
/// Used for quick interactive calls (listing, preview); downloads have their
/// own, more patient loop in `job` that reports countdowns to the UI.
async fn with_retries<T, F>(backoff_secs: &[u64], mut attempt: impl FnMut() -> F) -> Result<T, ToolError>
where
    F: Future<Output = Result<T, ToolError>>,
{
    let mut delays = backoff_secs.iter();
    loop {
        match attempt().await {
            Err(e) if e.kind == FailureKind::Transient => match delays.next() {
                Some(&secs) => tokio::time::sleep(std::time::Duration::from_secs(secs)).await,
                None => return Err(e),
            },
            result => return result,
        }
    }
}

/// Downloads the best audio-only stream of a video into `dir` as `<id>.<ext>`,
/// reporting progress (0.0..=1.0) through `on_progress`.
pub async fn download_audio(
    settings: &Settings,
    id: &str,
    dir: &Path,
    mut on_progress: impl FnMut(f32),
) -> Result<PathBuf, ToolError> {
    let fail = |message: String| ToolError {
        message,
        kind: FailureKind::Transient,
    };

    let mut child = base_command(&settings.ytdlp)
        // Audio only: no video bytes are ever fetched. Prefer Opus, which is
        // what the mixtape ends up as anyway.
        .args(["-f", "bestaudio[acodec=opus]/bestaudio"])
        .args(["--no-playlist", "--no-warnings", "--newline", "--progress"])
        .args(["--limit-rate", settings.rate_limit.trim()])
        .args(["--sleep-requests", &settings.sleep_requests().to_string()])
        .args(["--retries", "3", "--fragment-retries", "3"])
        .args(["--retry-sleep", "http:exp=2:60", "--retry-sleep", "fragment:exp=2:60"])
        .args([
            "--progress-template",
            "download:PROGRESS %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s",
        ])
        .args(["--print", "after_move:FILE %(filepath)s"])
        .arg("-o")
        .arg(dir.join("%(id)s.%(ext)s"))
        .args(settings.extra_args())
        .arg("--")
        .arg(watch_url(id))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::permanent(spawn_error(&settings.ytdlp, e)))?;

    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });

    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut path = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.strip_prefix("PROGRESS ") {
            let nums: Vec<f64> = rest.split_whitespace().map(|n| n.parse().unwrap_or(0.0)).collect();
            if let [done, total, estimate] = nums[..] {
                let total = if total > 0.0 { total } else { estimate };
                if total > 0.0 {
                    on_progress((done / total).clamp(0.0, 1.0) as f32);
                }
            }
        } else if let Some(file) = line.strip_prefix("FILE ") {
            path = Some(PathBuf::from(file.trim()));
        }
    }

    let status = child.wait().await.map_err(|e| fail(e.to_string()))?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        return Err(ToolError::from_stderr(&stderr));
    }

    path.filter(|p| p.exists())
        .or_else(|| find_cached(dir, id))
        .ok_or_else(|| fail("yt-dlp finished but the file is missing".into()))
}

/// Finds an already downloaded audio file for `id` in `dir`.
pub fn find_cached(dir: &Path, id: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(id) && !is_partial(p))
}

/// Lists every completed download in `dir` as (video id, path).
pub fn scan_cache(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| !is_partial(p))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
        .collect()
}

fn is_partial(path: &Path) -> bool {
    let name = path.to_string_lossy();
    name.ends_with(".part") || name.ends_with(".ytdl") || name.contains(".part-Frag") || name.ends_with(".temp")
}

/// Resolves a direct audio stream URL for previewing without downloading
/// the whole file: the player only fetches what it actually plays.
pub async fn resolve_stream_url(settings: Settings, id: String) -> Result<String, String> {
    with_retries(&[3], || resolve_stream_url_once(&settings, &id))
        .await
        .map_err(|e| e.message)
}

async fn resolve_stream_url_once(settings: &Settings, id: &str) -> Result<String, ToolError> {
    let output = base_command(&settings.ytdlp)
        .args(["-g", "-f", "bestaudio", "--no-playlist", "--no-warnings"])
        .args(settings.extra_args())
        .arg("--")
        .arg(watch_url(id))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ToolError::permanent(spawn_error(&settings.ytdlp, e)))?;
    if !output.status.success() {
        return Err(ToolError::from_stderr(&String::from_utf8_lossy(&output.stderr)));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("http"))
        .map(str::to_string)
        .ok_or_else(|| ToolError {
            message: "yt-dlp returned no stream URL".into(),
            kind: FailureKind::Transient,
        })
}

/// Plays `input` (a path or URL) from `start` seconds until it ends or the
/// task is aborted.
pub async fn play(settings: Settings, input: String, start: f64) -> Result<(), String> {
    let start = format!("{start:.1}");
    let mut cmd = command_from_template(&settings.player_command, &[("input", &input), ("start", &start)])?;
    let output = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| spawn_error(&settings.player_command, e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Player failed: {}",
            tail(&String::from_utf8_lossy(&output.stderr))
        ))
    }
}

/// Speaks `text` into a WAV file at `out` using the configured TTS command.
pub async fn synthesize(settings: &Settings, text: &str, out: &Path) -> Result<(), String> {
    let out_str = out.to_string_lossy();
    let mut cmd = command_from_template(&settings.tts_command, &[("out", &out_str)])?;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_error(&settings.tts_command, e))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(text.as_bytes()).await.map_err(|e| e.to_string())?;
    stdin.write_all(b"\n").await.map_err(|e| e.to_string())?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
    let produced = std::fs::metadata(out).map(|m| m.len() > 44).unwrap_or(false);
    if output.status.success() && produced {
        Ok(())
    } else {
        Err(format!(
            "TTS failed for \"{text}\": {}",
            tail(&String::from_utf8_lossy(&output.stderr))
        ))
    }
}

/// Integrated loudness (EBU R128) of a media file in LUFS.
pub async fn measure_lufs(settings: &Settings, path: &Path) -> Result<f64, String> {
    let output = base_command(&settings.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args(["-af", "ebur128=framelog=quiet", "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| spawn_error(&settings.ffmpeg, e))?;
    // The summary block at the end contains a line like "    I:         -19.7 LUFS".
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find_map(|l| {
            l.trim()
                .strip_prefix("I:")?
                .trim()
                .strip_suffix("LUFS")?
                .trim()
                .parse()
                .ok()
        })
        .filter(|lufs: &f64| lufs.is_finite() && *lufs > -70.0)
        .ok_or_else(|| format!("Couldn't measure loudness of {}", path.display()))
}

/// Exact duration of a media file in seconds.
pub async fn probe_duration(settings: &Settings, path: &Path) -> Result<f64, String> {
    let output = base_command(&settings.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| spawn_error(&settings.ffprobe, e))?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("Couldn't read duration of {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_channel_urls() {
        assert_eq!(
            normalize_channel_url("@ballacid"),
            "https://www.youtube.com/@ballacid/videos"
        );
        assert_eq!(
            normalize_channel_url("https://www.youtube.com/@ballacid/"),
            "https://www.youtube.com/@ballacid/videos"
        );
        assert_eq!(
            normalize_channel_url("https://www.youtube.com/@ballacid/videos"),
            "https://www.youtube.com/@ballacid/videos"
        );
        assert_eq!(
            normalize_channel_url("https://www.youtube.com/playlist?list=PL123"),
            "https://www.youtube.com/playlist?list=PL123"
        );
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(
            classify_failure("ERROR: [youtube] abc: HTTP Error 429: Too Many Requests"),
            FailureKind::Throttled
        );
        assert_eq!(
            classify_failure("ERROR: [youtube] abc: Sign in to confirm you're not a bot."),
            FailureKind::Throttled
        );
        assert_eq!(
            classify_failure("ERROR: [youtube] abc: Video unavailable"),
            FailureKind::Permanent
        );
        assert_eq!(
            classify_failure("ERROR: [youtube] aaaaaaaaaaa: This video is unavailable"),
            FailureKind::Permanent
        );
        assert_eq!(
            classify_failure("ERROR: [youtube] abc: Private video"),
            FailureKind::Permanent
        );
        assert_eq!(
            classify_failure("ERROR: unable to download video data: <urlopen error timed out>"),
            FailureKind::Transient
        );
    }

    #[test]
    fn templates_substitute_per_argument() {
        let cmd = command_from_template("espeak-ng --stdin -w {out}", &[("out", "/tmp/a b.wav")]).unwrap();
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--stdin", "-w", "/tmp/a b.wav"]);
    }
}
