//! Persisted state, settings and small text helpers.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Video {
    pub id: String,
    pub title: String,
    /// Seconds, as reported by the channel listing.
    pub duration: Option<f64>,
}

pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// One track in the pending mixtape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub duration: Option<f64>,
    /// User-edited announcement; `None` means "derive from the template".
    pub announce: Option<String>,
}

impl Entry {
    pub fn announcement(&self, template: &str) -> String {
        self.announce
            .clone()
            .unwrap_or_else(|| announcement_from_title(template, &self.title))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    Opus,
    Vorbis,
}

impl Codec {
    pub const ALL: [Codec; 2] = [Codec::Opus, Codec::Vorbis];

    pub fn extension(self) -> &'static str {
        match self {
            Codec::Opus => "opus",
            Codec::Vorbis => "ogg",
        }
    }

    pub fn encoder(self) -> &'static str {
        match self {
            Codec::Opus => "libopus",
            Codec::Vorbis => "libvorbis",
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Codec::Opus => write!(f, "Opus (.opus)"),
            Codec::Vorbis => write!(f, "Vorbis (.ogg)"),
        }
    }
}

/// User settings. Numeric fields are kept as strings so they can be bound
/// directly to text inputs; use the accessor methods to read them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub fetch_limit: String,

    // Being nice to YouTube.
    pub rate_limit: String,
    pub min_delay: String,
    pub max_delay: String,
    pub sleep_requests: String,
    pub ytdlp_extra_args: String,

    // External tools.
    pub ytdlp: String,
    pub ffmpeg: String,
    pub ffprobe: String,
    /// Text is written to stdin; `{out}` is replaced with the WAV path.
    pub tts_command: String,
    /// `{input}` is a file path or stream URL, `{start}` is seconds.
    pub player_command: String,

    // Mixtape rendering.
    pub announce_template: String,
    pub voice_lufs: String,
    pub gap_before_voice: String,
    pub gap_after_voice: String,
    pub codec: Codec,
    pub bitrate_kbps: String,
    pub output_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        let output_dir = directories::UserDirs::new()
            .and_then(|u| u.audio_dir().map(Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        Settings {
            fetch_limit: "200".into(),
            rate_limit: "1M".into(),
            min_delay: "8".into(),
            max_delay: "20".into(),
            sleep_requests: "1".into(),
            ytdlp_extra_args: String::new(),
            ytdlp: "yt-dlp".into(),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            tts_command: "espeak-ng -v en-us -s 155 --stdin -w {out}".into(),
            player_command: "ffplay -nodisp -autoexit -loglevel error -ss {start} {input}".into(),
            announce_template: "{artist}. {track}.".into(),
            voice_lufs: "-14".into(),
            gap_before_voice: "0.8".into(),
            gap_after_voice: "0.6".into(),
            codec: Codec::Opus,
            bitrate_kbps: "128".into(),
            output_dir: output_dir.to_string_lossy().into_owned(),
        }
    }
}

fn parse_or<T: std::str::FromStr>(s: &str, default: T) -> T {
    s.trim().parse().unwrap_or(default)
}

impl Settings {
    pub fn fetch_limit(&self) -> u32 {
        parse_or(&self.fetch_limit, 200).max(1)
    }
    /// Seconds to wait between consecutive downloads, as (min, max).
    pub fn delay_range(&self) -> (f32, f32) {
        let min = parse_or(&self.min_delay, 8.0f32).max(0.0);
        let max = parse_or(&self.max_delay, 20.0f32).max(min);
        (min, max)
    }
    pub fn sleep_requests(&self) -> f32 {
        parse_or(&self.sleep_requests, 1.0f32).max(0.0)
    }
    pub fn voice_lufs(&self) -> f32 {
        parse_or(&self.voice_lufs, -14.0f32).clamp(-40.0, -5.0)
    }
    pub fn gap_before_voice(&self) -> f64 {
        parse_or(&self.gap_before_voice, 0.8f64).clamp(0.0, 30.0)
    }
    pub fn gap_after_voice(&self) -> f64 {
        parse_or(&self.gap_after_voice, 0.6f64).clamp(0.0, 30.0)
    }
    pub fn bitrate_kbps(&self) -> u32 {
        parse_or(&self.bitrate_kbps, 128u32).clamp(32, 512)
    }
    pub fn extra_args(&self) -> Vec<String> {
        shlex::split(&self.ytdlp_extra_args).unwrap_or_default()
    }
}

/// Everything that survives a restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub channel_url: String,
    pub channel_title: String,
    pub videos: Vec<Video>,
    pub mixtape: Vec<Entry>,
    pub output_name: String,
    pub settings: Settings,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            channel_url: "https://www.youtube.com/@ballacid/videos".into(),
            channel_title: String::new(),
            videos: Vec::new(),
            mixtape: Vec::new(),
            output_name: String::new(),
            settings: Settings::default(),
        }
    }
}

impl Saved {
    pub fn load(path: &Path) -> Saved {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn store(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)
    }
}

/// Splits "Artist -- Track" (and the usual dash variants) into its parts.
pub fn split_title(title: &str) -> Option<(&str, &str)> {
    [" -- ", " — ", " – ", " - ", " ~ "]
        .iter()
        .find_map(|sep| title.split_once(sep))
        .map(|(a, t)| (a.trim(), t.trim()))
        .filter(|(a, t)| !a.is_empty() && !t.is_empty())
}

pub fn announcement_from_title(template: &str, title: &str) -> String {
    match split_title(title) {
        Some((artist, track)) => template
            .replace("{artist}", artist)
            .replace("{track}", track)
            .replace("{title}", title),
        None => format!("{title}."),
    }
}

/// Rough guess of how long the TTS will take to say `text`, in seconds.
pub fn estimate_speech_secs(text: &str) -> f64 {
    0.4 + text.chars().count() as f64 * 0.075
}

pub fn fmt_duration(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Today's date as YYYY-MM-DD (UTC), without pulling in a date crate.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Turns arbitrary text into something safe to use as a file name.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned.trim().trim_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_ballacid_titles() {
        assert_eq!(
            split_title("DJ Muscles -- Vacuum Fever"),
            Some(("DJ Muscles", "Vacuum Fever"))
        );
        assert_eq!(
            split_title("M-Paths -- Emotivated (Moon Mix)"),
            Some(("M-Paths", "Emotivated (Moon Mix)"))
        );
        assert_eq!(split_title("Artist - Song"), Some(("Artist", "Song")));
        assert_eq!(split_title("No separator here"), None);
    }

    #[test]
    fn builds_announcements() {
        assert_eq!(
            announcement_from_title("{artist}. {track}.", "M-Paths -- Sirens"),
            "M-Paths. Sirens."
        );
        assert_eq!(announcement_from_title("{artist}. {track}.", "Untitled"), "Untitled.");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_duration(59.4), "0:59");
        assert_eq!(fmt_duration(463.0), "7:43");
        assert_eq!(fmt_duration(3725.0), "1:02:05");
    }

    #[test]
    fn today_looks_like_a_date() {
        let t = today();
        assert_eq!(t.len(), 10);
        assert_eq!(&t[4..5], "-");
    }
}
