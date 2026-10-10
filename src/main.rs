//! tubetape: pick tracks from a YouTube channel and turn them into one long
//! announced mixtape.

mod job;
mod model;
mod tools;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::task::Handle;
use iced::widget::{
    button, checkbox, column, container, pick_list, progress_bar, row, rule, scrollable, space, text, text_input,
    tooltip,
};
use iced::{Alignment, Element, Fill, FillPortion, Subscription, Task, Theme};

use job::JobEvent;
use model::{Codec, Entry, Saved, Settings, Video, estimate_speech_secs, fmt_duration};

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .window_size((1320.0, 860.0))
        .run()
}

struct Paths {
    state: PathBuf,
    audio: PathBuf,
    tts: PathBuf,
}

impl Paths {
    fn new() -> Paths {
        let dirs = directories::ProjectDirs::from("", "", "tubetape");
        let (config, cache) = match &dirs {
            Some(d) => (d.config_dir().to_path_buf(), d.cache_dir().to_path_buf()),
            None => (PathBuf::from(".tubetape"), PathBuf::from(".tubetape/cache")),
        };
        let paths = Paths {
            state: config.join("state.json"),
            audio: cache.join("audio"),
            tts: cache.join("tts"),
        };
        for dir in [&config, &paths.audio, &paths.tts] {
            let _ = std::fs::create_dir_all(dir);
        }
        paths
    }
}

enum Download {
    Active(f32),
    /// Waiting to retry; the text says when.
    Retrying(String),
    Failed(String),
}

/// Something the preview player is (about to be) playing.
struct Preview {
    /// Video id, or a pseudo id for the voice test / finished mixtape.
    id: String,
    title: String,
    /// File path or stream URL; `None` while the stream URL is being resolved.
    source: Option<String>,
    duration: Option<f64>,
    start: f64,
    started_at: Option<Instant>,
    token: u64,
    /// Aborts the player task (and kills the player) when dropped.
    _player: Option<Handle>,
}

impl Preview {
    fn position(&self) -> f64 {
        self.start + self.started_at.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }
}

const VOICE_TEST_ID: &str = "\u{0}voice";
const RESULT_ID: &str = "\u{0}result";

struct App {
    saved: Saved,
    paths: Paths,
    cache: HashMap<String, PathBuf>,
    downloads: HashMap<String, Download>,
    filter: String,
    fetching: bool,
    show_settings: bool,
    tool_problems: Vec<String>,

    preview: Option<Preview>,
    next_token: u64,

    /// Aborts the running job when dropped.
    job: Option<Handle>,
    job_status: String,
    job_progress: Option<f32>,
    last_download: Option<Instant>,
    last_output: Option<PathBuf>,

    log: Vec<String>,
}

/// Declares the editable settings fields with shared and mutable accessors.
macro_rules! fields {
    ($($variant:ident => $field:ident),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Field {
            $($variant),*
        }

        impl Field {
            fn value(self, s: &Settings) -> &String {
                match self {
                    $(Field::$variant => &s.$field),*
                }
            }

            fn value_mut(self, s: &mut Settings) -> &mut String {
                match self {
                    $(Field::$variant => &mut s.$field),*
                }
            }
        }
    };
}

fields! {
    FetchLimit => fetch_limit,
    RateLimit => rate_limit,
    MinDelay => min_delay,
    MaxDelay => max_delay,
    SleepRequests => sleep_requests,
    ExtraArgs => ytdlp_extra_args,
    Ytdlp => ytdlp,
    Ffmpeg => ffmpeg,
    Ffprobe => ffprobe,
    Tts => tts_command,
    Player => player_command,
    Template => announce_template,
    VoiceLufs => voice_lufs,
    GapBefore => gap_before_voice,
    GapAfter => gap_after_voice,
    Bitrate => bitrate_kbps,
    OutputDir => output_dir,
}

impl Field {
    fn label_and_help(self) -> (&'static str, &'static str) {
        match self {
            Field::FetchLimit => ("Videos to list", "How many of the newest uploads to fetch."),
            Field::RateLimit => ("Bandwidth limit", "Max download speed per track, e.g. 500K or 1M."),
            Field::MinDelay => ("Min pause (s)", "Shortest pause between two downloads."),
            Field::MaxDelay => ("Max pause (s)", "Longest pause; each pause is randomized in between."),
            Field::SleepRequests => ("Request spacing (s)", "Pause between yt-dlp's metadata requests."),
            Field::ExtraArgs => (
                "Extra yt-dlp args",
                "e.g. --cookies-from-browser firefox if YouTube starts asking for sign-in.",
            ),
            Field::Ytdlp => ("yt-dlp", "Path or command name."),
            Field::Ffmpeg => ("ffmpeg", "Path or command name."),
            Field::Ffprobe => ("ffprobe", "Path or command name."),
            Field::Tts => (
                "TTS command",
                "Text arrives on stdin, {out} is the WAV to write. Piper: piper -m voice.onnx -f {out}",
            ),
            Field::Player => (
                "Preview player",
                "{input} is a file or URL, {start} the offset in seconds.",
            ),
            Field::Template => ("Announcement", "Default text per track: {artist}, {track}, {title}."),
            Field::VoiceLufs => (
                "Voice loudness (LUFS)",
                "Higher (closer to 0) is louder. -14 suits most music.",
            ),
            Field::GapBefore => ("Silence before voice (s)", ""),
            Field::GapAfter => ("Silence after voice (s)", ""),
            Field::Bitrate => ("Bitrate (kbps)", ""),
            Field::OutputDir => ("Output folder", ""),
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    ChannelUrlChanged(String),
    Fetch,
    FetchMore,
    Fetched(Result<(String, Vec<Video>), String>),
    FilterChanged(String),
    Toggle(String, bool),

    Preview(String),
    PreviewResolved(u64, Result<String, String>),
    PreviewSeek(f64),
    PreviewStop,
    PreviewEnded(u64, Result<(), String>),
    TestVoice,
    VoiceReady(u64, Result<PathBuf, String>),
    PlayResult,
    Tick,

    MoveUp(usize),
    MoveDown(usize),
    Remove(usize),
    AnnounceChanged(usize, String),
    AnnounceReset(usize),
    ClearMixtape,
    RemoveFailed,
    OutputNameChanged(String),

    Prefetch,
    Build,
    CancelJob,
    Job(JobEvent),

    ToggleSettings,
    Setting(Field, String),
    CodecSelected(Codec),
    CheckTools,
    ToolsChecked(Vec<String>),
}

impl App {
    fn new() -> (App, Task<Message>) {
        let paths = Paths::new();
        let saved = Saved::load(&paths.state);
        let cache = tools::scan_cache(&paths.audio).into_iter().collect();
        let app = App {
            saved,
            paths,
            cache,
            downloads: HashMap::new(),
            filter: String::new(),
            fetching: false,
            show_settings: false,
            tool_problems: Vec::new(),
            preview: None,
            next_token: 0,
            job: None,
            job_status: String::new(),
            job_progress: None,
            last_download: None,
            last_output: None,
            log: Vec::new(),
        };
        let mut tasks = vec![app.check_tools()];
        if app.saved.videos.is_empty() {
            tasks.push(Task::done(Message::Fetch));
        }
        (app, Task::batch(tasks))
    }

    fn title(&self) -> String {
        if self.saved.channel_title.is_empty() {
            "tubetape".into()
        } else {
            format!("tubetape: {}", self.saved.channel_title)
        }
    }

    fn theme(&self) -> Theme {
        Theme::TokyoNightStorm
    }

    fn subscription(&self) -> Subscription<Message> {
        // Only tick while something is playing, to update the position readout.
        match &self.preview {
            Some(p) if p.started_at.is_some() => iced::time::every(Duration::from_millis(500)).map(|_| Message::Tick),
            _ => Subscription::none(),
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.saved.store(&self.paths.state) {
            self.push_log(format!("Couldn't save state: {e}"));
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 200 {
            self.log.drain(..100);
        }
    }

    fn check_tools(&self) -> Task<Message> {
        Task::perform(tools::check_tools(self.saved.settings.clone()), Message::ToolsChecked)
    }

    fn cached(&self, id: &str) -> Option<&PathBuf> {
        self.cache.get(id).filter(|p| p.exists())
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn default_output_name(&self) -> String {
        let channel = if self.saved.channel_title.is_empty() {
            "YouTube"
        } else {
            &self.saved.channel_title
        };
        format!("{channel} mixtape {}", model::today())
    }

    fn output_path(&self) -> PathBuf {
        let mut name = self.saved.output_name.trim().to_string();
        if name.is_empty() {
            name = self.default_output_name();
        }
        for ext in [".opus", ".ogg"] {
            if let Some(stripped) = name.strip_suffix(ext) {
                name = stripped.to_string();
            }
        }
        let name = model::sanitize_filename(&name);
        PathBuf::from(self.saved.settings.output_dir.trim())
            .join(format!("{name}.{}", self.saved.settings.codec.extension()))
    }

    /// (estimated seconds, number of tracks with unknown length)
    fn estimated_length(&self) -> (f64, usize) {
        let s = &self.saved.settings;
        let gaps = s.gap_before_voice() + s.gap_after_voice();
        let template = &s.announce_template;
        self.saved.mixtape.iter().fold((0.0, 0), |(secs, unknown), e| {
            let voice = estimate_speech_secs(&e.announcement(template)) + gaps;
            match e.duration {
                Some(d) => (secs + voice + d, unknown),
                None => (secs + voice, unknown + 1),
            }
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ChannelUrlChanged(url) => {
                self.saved.channel_url = url;
                self.save();
            }
            Message::Fetch => {
                if self.fetching {
                    return Task::none();
                }
                let url = tools::normalize_channel_url(&self.saved.channel_url);
                self.saved.channel_url = url.clone();
                self.fetching = true;
                self.push_log(format!("Listing {url}…"));
                return Task::perform(tools::fetch_listing(self.saved.settings.clone(), url), Message::Fetched);
            }
            Message::FetchMore => {
                let more = self.saved.settings.fetch_limit().max(self.saved.videos.len() as u32) + 200;
                self.saved.settings.fetch_limit = more.to_string();
                return self.update(Message::Fetch);
            }
            Message::Fetched(result) => {
                self.fetching = false;
                match result {
                    Ok((channel, videos)) => {
                        self.push_log(format!("Found {} videos on {channel}.", videos.len()));
                        if self.saved.channel_title != channel {
                            self.saved.output_name.clear();
                        }
                        self.saved.channel_title = channel;
                        self.saved.videos = videos;
                        self.save();
                    }
                    Err(e) => self.push_log(e),
                }
            }
            Message::FilterChanged(filter) => self.filter = filter,
            Message::Toggle(id, include) => {
                self.saved.mixtape.retain(|e| e.id != id);
                if include && let Some(v) = self.saved.videos.iter().find(|v| v.id == id) {
                    self.saved.mixtape.push(Entry {
                        id: v.id.clone(),
                        title: v.title.clone(),
                        duration: v.duration,
                        announce: None,
                    });
                }
                self.save();
            }

            Message::Preview(id) => {
                if self.preview.as_ref().is_some_and(|p| p.id == id) {
                    self.preview = None;
                    return Task::none();
                }
                let Some(video) = self.saved.videos.iter().find(|v| v.id == id).cloned() else {
                    return Task::none();
                };
                // Start a third of the way in: past the intro, into the groove.
                let start = video.duration.map_or(0.0, |d| (d / 3.0).floor());
                let token = self.token();
                let source = self.cached(&id).map(|p| p.to_string_lossy().into_owned());
                let resolve = source.is_none();
                self.preview = Some(Preview {
                    id: id.clone(),
                    title: video.title.clone(),
                    source,
                    duration: video.duration,
                    start,
                    started_at: None,
                    token,
                    _player: None,
                });
                if resolve {
                    return Task::perform(tools::resolve_stream_url(self.saved.settings.clone(), id), move |r| {
                        Message::PreviewResolved(token, r)
                    });
                }
                return self.start_player();
            }
            Message::PreviewResolved(token, result) => {
                let Some(preview) = self.preview.as_mut().filter(|p| p.token == token) else {
                    return Task::none();
                };
                match result {
                    Ok(url) => {
                        preview.source = Some(url);
                        return self.start_player();
                    }
                    Err(e) => {
                        self.preview = None;
                        self.push_log(format!("Preview failed: {e}"));
                    }
                }
            }
            Message::PreviewSeek(delta) => {
                if let Some(p) = self.preview.as_mut().filter(|p| p.source.is_some()) {
                    let mut target = (p.position() + delta).max(0.0);
                    if let Some(d) = p.duration {
                        target = target.min((d - 5.0).max(0.0));
                    }
                    p.start = target;
                    return self.start_player();
                }
            }
            Message::PreviewStop => self.preview = None,
            Message::PreviewEnded(token, result) => {
                if self.preview.as_ref().is_some_and(|p| p.token == token) {
                    self.preview = None;
                }
                if let Err(e) = result {
                    self.push_log(e);
                }
            }
            Message::TestVoice => {
                let text = self
                    .saved
                    .mixtape
                    .first()
                    .map(|e| e.announcement(&self.saved.settings.announce_template))
                    .unwrap_or_else(|| {
                        model::announcement_from_title(
                            &self.saved.settings.announce_template,
                            "DJ Muscles -- Vacuum Fever",
                        )
                    });
                let token = self.token();
                self.preview = Some(Preview {
                    id: VOICE_TEST_ID.into(),
                    title: format!("Voice test: \"{text}\""),
                    source: None,
                    duration: None,
                    start: 0.0,
                    started_at: None,
                    token,
                    _player: None,
                });
                let (settings, dir) = (self.saved.settings.clone(), self.paths.tts.clone());
                return Task::perform(
                    async move { job::announcement_wav(&settings, &dir, &text).await },
                    move |r| Message::VoiceReady(token, r),
                );
            }
            Message::VoiceReady(token, result) => {
                let Some(preview) = self.preview.as_mut().filter(|p| p.token == token) else {
                    return Task::none();
                };
                match result {
                    Ok(path) => {
                        preview.source = Some(path.to_string_lossy().into_owned());
                        return self.start_player();
                    }
                    Err(e) => {
                        self.preview = None;
                        self.push_log(e);
                    }
                }
            }
            Message::PlayResult => {
                if let Some(path) = self.last_output.clone() {
                    let token = self.token();
                    self.preview = Some(Preview {
                        id: RESULT_ID.into(),
                        title: path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        source: Some(path.to_string_lossy().into_owned()),
                        duration: None,
                        start: 0.0,
                        started_at: None,
                        token,
                        _player: None,
                    });
                    return self.start_player();
                }
            }
            Message::Tick => {}

            Message::MoveUp(i) => {
                if i > 0 && i < self.saved.mixtape.len() {
                    self.saved.mixtape.swap(i, i - 1);
                    self.save();
                }
            }
            Message::MoveDown(i) => {
                if i + 1 < self.saved.mixtape.len() {
                    self.saved.mixtape.swap(i, i + 1);
                    self.save();
                }
            }
            Message::Remove(i) => {
                if i < self.saved.mixtape.len() {
                    self.saved.mixtape.remove(i);
                    self.save();
                }
            }
            Message::AnnounceChanged(i, value) => {
                if let Some(e) = self.saved.mixtape.get_mut(i) {
                    e.announce = Some(value);
                    self.save();
                }
            }
            Message::AnnounceReset(i) => {
                if let Some(e) = self.saved.mixtape.get_mut(i) {
                    e.announce = None;
                    self.save();
                }
            }
            Message::ClearMixtape => {
                self.saved.mixtape.clear();
                self.save();
            }
            Message::RemoveFailed => {
                let downloads = &self.downloads;
                self.saved
                    .mixtape
                    .retain(|e| !matches!(downloads.get(&e.id), Some(Download::Failed(_))));
                self.save();
            }
            Message::OutputNameChanged(name) => {
                self.saved.output_name = name;
                self.save();
            }

            Message::Prefetch => return self.start_job(false),
            Message::Build => return self.start_job(true),
            Message::CancelJob => {
                if self.job.take().is_some() {
                    self.downloads.retain(|_, d| matches!(d, Download::Failed(_)));
                    self.job_status = "Cancelled.".into();
                    self.job_progress = None;
                    self.push_log("Cancelled.");
                }
            }
            Message::Job(event) => self.on_job_event(event),

            Message::ToggleSettings => {
                self.show_settings = !self.show_settings;
                if !self.show_settings {
                    return self.check_tools();
                }
            }
            Message::Setting(field, value) => {
                *field.value_mut(&mut self.saved.settings) = value;
                self.save();
            }
            Message::CodecSelected(codec) => {
                self.saved.settings.codec = codec;
                self.save();
            }
            Message::CheckTools => return self.check_tools(),
            Message::ToolsChecked(problems) => self.tool_problems = problems,
        }
        Task::none()
    }

    /// (Re)starts the player for the current preview at its `start` offset.
    fn start_player(&mut self) -> Task<Message> {
        let token = self.token();
        let settings = self.saved.settings.clone();
        let Some(p) = self.preview.as_mut() else {
            return Task::none();
        };
        let Some(source) = p.source.clone() else {
            return Task::none();
        };
        let (task, handle) = Task::perform(tools::play(settings, source, p.start), move |r| {
            Message::PreviewEnded(token, r)
        })
        .abortable();
        // Replacing the handle drops (and so stops) any previous player.
        p._player = Some(handle.abort_on_drop());
        p.token = token;
        p.started_at = Some(Instant::now());
        task
    }

    fn start_job(&mut self, render: bool) -> Task<Message> {
        if self.job.is_some() || self.saved.mixtape.is_empty() {
            return Task::none();
        }
        let template = &self.saved.settings.announce_template;
        let items: Vec<job::Item> = self
            .saved
            .mixtape
            .iter()
            .map(|e| job::Item {
                id: e.id.clone(),
                title: e.title.clone(),
                announcement: e.announcement(template),
                audio: self.cached(&e.id).cloned(),
            })
            .collect();

        let render = if render {
            let output = self.output_path();
            if let Some(dir) = output.parent()
                && let Err(e) = std::fs::create_dir_all(dir)
            {
                self.job_status = format!("Can't create {}: {e}", dir.display());
                return Task::none();
            }
            let title = output
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some(job::Render { output, title })
        } else {
            if items.iter().all(|i| i.audio.is_some()) {
                self.job_status = "All selected tracks are already downloaded.".into();
                return Task::none();
            }
            None
        };

        for item in items.iter().filter(|i| i.audio.is_none()) {
            self.downloads.remove(&item.id);
        }
        let plan = job::Plan {
            settings: self.saved.settings.clone(),
            audio_dir: self.paths.audio.clone(),
            tts_dir: self.paths.tts.clone(),
            items,
            last_download: self.last_download,
            render,
        };
        let (task, handle) = Task::run(job::run(plan), Message::Job).abortable();
        self.job = Some(handle.abort_on_drop());
        self.job_status = "Starting…".into();
        self.job_progress = Some(0.0);
        task
    }

    fn on_job_event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Status(status) => self.job_status = status,
            JobEvent::Progress(p) => self.job_progress = Some(p),
            JobEvent::Log(line) => self.push_log(line),
            JobEvent::DownloadStarted(id) => {
                self.last_download = Some(Instant::now());
                self.downloads.insert(id, Download::Active(0.0));
            }
            JobEvent::DownloadProgress(id, p) => {
                if let Some(Download::Active(progress)) = self.downloads.get_mut(&id) {
                    *progress = p;
                }
            }
            JobEvent::Downloaded(id, path) => {
                self.last_download = Some(Instant::now());
                self.downloads.remove(&id);
                self.cache.insert(id, path);
            }
            JobEvent::DownloadRetrying(id, when) => {
                self.downloads.insert(id, Download::Retrying(when));
            }
            JobEvent::DownloadFailed(id, error) => {
                self.last_download = Some(Instant::now());
                self.downloads.insert(id, Download::Failed(error));
            }
            JobEvent::Finished(result) => {
                self.job = None;
                self.job_progress = None;
                match result {
                    Ok(Some(path)) => {
                        self.push_log(format!("Saved {}", path.display()));
                        self.last_output = Some(path);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.job_status = e.clone();
                        self.push_log(e);
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- view

    fn view(&self) -> Element<'_, Message> {
        let header = row![
            text_input("Channel URL or @handle", &self.saved.channel_url)
                .on_input(Message::ChannelUrlChanged)
                .on_submit(Message::Fetch)
                .padding(8),
            button(text(if self.fetching { "Fetching…" } else { "Fetch" }))
                .on_press_maybe((!self.fetching).then_some(Message::Fetch))
                .padding([8, 16]),
            button(text(if self.show_settings {
                "Back to mixtape"
            } else {
                "Settings"
            }))
            .on_press(Message::ToggleSettings)
            .style(button::secondary)
            .padding([8, 16]),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut body = column![header].spacing(10).padding(12);

        if !self.tool_problems.is_empty() {
            body = body.push(
                container(
                    column![
                        text("Some tools are missing. Install them or fix their paths in Settings:")
                            .style(text::warning),
                        text(self.tool_problems.join("\n")).size(13),
                        button(text("Check again").size(13))
                            .on_press(Message::CheckTools)
                            .style(button::secondary),
                    ]
                    .spacing(6),
                )
                .padding(10)
                .width(Fill)
                .style(container::bordered_box),
            );
        }

        if let Some(preview) = &self.preview {
            body = body.push(self.view_preview(preview));
        }

        let right = if self.show_settings {
            self.view_settings()
        } else {
            self.view_mixtape()
        };
        body = body.push(
            row![
                container(self.view_videos()).width(FillPortion(3)).height(Fill),
                rule::vertical(1),
                container(right).width(FillPortion(2)).height(Fill),
            ]
            .spacing(12)
            .height(Fill),
        );

        let log = self
            .log
            .iter()
            .rev()
            .take(3)
            .rev()
            .fold(column![].spacing(2), |c, line| {
                c.push(text(line.clone()).size(12).style(text::secondary))
            });
        body.push(log).into()
    }

    fn view_preview(&self, p: &Preview) -> Element<'_, Message> {
        let state = match (&p.source, p.started_at) {
            (None, _) => "Loading…".to_string(),
            (Some(_), _) => match p.duration {
                Some(d) => format!("{} / {}", fmt_duration(p.position().min(d)), fmt_duration(d)),
                None => fmt_duration(p.position()),
            },
        };
        let seekable = p.source.is_some();
        let seek = |label: &'static str, delta: f64| {
            button(text(label).size(13))
                .on_press_maybe(seekable.then_some(Message::PreviewSeek(delta)))
                .style(button::secondary)
                .padding([4, 10])
        };
        container(
            row![
                text("Now playing").style(text::primary),
                text(p.title.clone()).width(Fill),
                text(state).size(14),
                seek("-30s", -30.0),
                seek("+30s", 30.0),
                seek("+60s", 60.0),
                button(text("Stop").size(13))
                    .on_press(Message::PreviewStop)
                    .style(button::danger)
                    .padding([4, 10]),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding(8)
        .width(Fill)
        .style(container::rounded_box)
        .into()
    }

    fn view_videos(&self) -> Element<'_, Message> {
        let selected: HashSet<&str> = self.saved.mixtape.iter().map(|e| e.id.as_str()).collect();
        let needle = self.filter.to_lowercase();
        let visible: Vec<&Video> = self
            .saved
            .videos
            .iter()
            .filter(|v| needle.is_empty() || v.title.to_lowercase().contains(&needle))
            .collect();

        let summary = if self.saved.videos.is_empty() {
            "No videos yet".to_string()
        } else if needle.is_empty() {
            format!("{} newest uploads", self.saved.videos.len())
        } else {
            format!("{} of {} uploads", visible.len(), self.saved.videos.len())
        };

        let top = row![
            text_input("Filter titles…", &self.filter)
                .on_input(Message::FilterChanged)
                .padding(6)
                .width(Fill),
            text(summary).size(13).style(text::secondary),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let list = visible
            .into_iter()
            .fold(column![].spacing(2).padding([0, 12]), |list, video| {
                list.push(self.view_video_row(video, selected.contains(video.id.as_str())))
            });

        let more = button(
            text(if self.fetching {
                "Fetching…"
            } else {
                "Load older uploads"
            })
            .size(13),
        )
        .on_press_maybe((!self.fetching && !self.saved.videos.is_empty()).then_some(Message::FetchMore))
        .style(button::secondary);

        column![top, scrollable(column![list, container(more).padding(12)]).height(Fill)]
            .spacing(8)
            .into()
    }

    fn view_video_row<'a>(&'a self, video: &'a Video, included: bool) -> Element<'a, Message> {
        let id = video.id.clone();
        let playing = self.preview.as_ref().is_some_and(|p| p.id == video.id);

        let status: Element<'a, Message> = match self.downloads.get(&video.id) {
            Some(Download::Active(p)) => text(format!("{:.0}%", p * 100.0)).size(12).style(text::primary).into(),
            Some(Download::Retrying(when)) => text(when.clone()).size(12).style(text::warning).into(),
            Some(Download::Failed(e)) => tooltip(
                text("failed").size(12).style(text::danger),
                container(text(e.clone()).size(12))
                    .padding(6)
                    .style(container::rounded_box),
                tooltip::Position::Left,
            )
            .into(),
            None if self.cache.contains_key(&video.id) => text("saved").size(12).style(text::success).into(),
            None => text("").size(12).into(),
        };

        row![
            button(text(if playing { "Stop" } else { "Play" }).size(12))
                .on_press(Message::Preview(id.clone()))
                .style(if playing { button::danger } else { button::secondary })
                .padding([2, 8])
                .width(46),
            checkbox(included)
                .label(video.title.as_str())
                .on_toggle(move |on| Message::Toggle(id.clone(), on))
                .width(Fill),
            container(status).width(48),
            text(video.duration.map(fmt_duration).unwrap_or_else(|| "?".into()))
                .size(13)
                .width(56),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    }

    fn view_mixtape(&self) -> Element<'_, Message> {
        let mixtape = &self.saved.mixtape;
        let template = &self.saved.settings.announce_template;
        let (secs, unknown) = self.estimated_length();
        let downloaded = mixtape.iter().filter(|e| self.cached(&e.id).is_some()).count();
        let size_mb = secs * f64::from(self.saved.settings.bitrate_kbps()) / 8.0 / 1000.0;

        let mut summary = format!(
            "{} tracks · ≈ {} · ~{size_mb:.0} MB · {downloaded}/{} downloaded",
            mixtape.len(),
            fmt_duration(secs),
            mixtape.len()
        );
        if unknown > 0 {
            summary.push_str(&format!(" · {unknown} of unknown length"));
        }

        let any_failed = mixtape
            .iter()
            .any(|e| matches!(self.downloads.get(&e.id), Some(Download::Failed(_))));
        let header = row![
            text("Mixtape").size(20),
            space().width(Fill),
            button(text("Remove failed").size(13))
                .on_press_maybe((any_failed && self.job.is_none()).then_some(Message::RemoveFailed))
                .style(button::secondary),
            button(text("Clear").size(13))
                .on_press_maybe((!mixtape.is_empty()).then_some(Message::ClearMixtape))
                .style(button::secondary),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let entries: Element<'_, Message> = if mixtape.is_empty() {
            container(
                text("Tick tracks on the left to add them. Each one will be announced before it plays.")
                    .style(text::secondary),
            )
            .padding(20)
            .into()
        } else {
            let list = mixtape
                .iter()
                .enumerate()
                .fold(column![].spacing(6).padding([0, 12]), |list, (i, e)| {
                    list.push(self.view_entry(i, e, template))
                });
            scrollable(list).height(Fill).into()
        };

        let busy = self.job.is_some();
        let can_run = !busy && !mixtape.is_empty();
        let output = self.output_path();
        let name_placeholder = self.default_output_name();

        let actions = if busy {
            row![
                button(text("Cancel"))
                    .on_press(Message::CancelJob)
                    .style(button::danger)
                    .padding([8, 16])
            ]
        } else {
            row![
                button(text("Download only"))
                    .on_press_maybe(can_run.then_some(Message::Prefetch))
                    .style(button::secondary)
                    .padding([8, 16]),
                button(text("Build mixtape"))
                    .on_press_maybe(can_run.then_some(Message::Build))
                    .padding([8, 16]),
                space().width(Fill),
                button(text("Play result"))
                    .on_press_maybe(self.last_output.as_ref().map(|_| Message::PlayResult))
                    .style(button::secondary)
                    .padding([8, 16]),
            ]
        }
        .spacing(8)
        .align_y(Alignment::Center);

        let mut footer = column![
            rule::horizontal(1),
            text(summary).size(14),
            row![
                text_input(&name_placeholder, &self.saved.output_name)
                    .on_input(Message::OutputNameChanged)
                    .padding(6)
                    .width(Fill),
                pick_list(Codec::ALL, Some(self.saved.settings.codec), Message::CodecSelected).padding(6),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(format!("→ {}", output.display())).size(12).style(text::secondary),
            actions,
        ]
        .spacing(8);

        if let Some(p) = self.job_progress {
            footer = footer.push(progress_bar(0.0..=1.0, p).girth(6));
        }
        if !self.job_status.is_empty() {
            footer = footer.push(text(self.job_status.clone()).size(13));
        }

        column![header, entries, footer].spacing(10).into()
    }

    fn view_entry<'a>(&'a self, i: usize, e: &'a Entry, template: &str) -> Element<'a, Message> {
        let last = i + 1 == self.saved.mixtape.len();
        let small = |label: &'static str, msg: Option<Message>| {
            button(text(label).size(12))
                .on_press_maybe(msg)
                .style(button::text)
                .padding([2, 6])
        };
        let mut voice = row![
            text("says").size(12).style(text::secondary),
            text_input("Announcement", &e.announcement(template))
                .on_input(move |s| Message::AnnounceChanged(i, s))
                .size(13)
                .padding(4)
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if e.announce.is_some() {
            voice = voice.push(small("reset", Some(Message::AnnounceReset(i))));
        }

        container(
            column![
                row![
                    text(format!("{}.", i + 1)).size(13).style(text::secondary).width(28),
                    text(e.title.as_str())
                        .width(Fill)
                        .style(match self.downloads.get(&e.id) {
                            Some(Download::Failed(_)) => text::danger,
                            Some(Download::Retrying(_)) => text::warning,
                            _ => text::default,
                        }),
                    text(e.duration.map(fmt_duration).unwrap_or_else(|| "?".into())).size(13),
                    small("up", (i > 0).then_some(Message::MoveUp(i))),
                    small("down", (!last).then_some(Message::MoveDown(i))),
                    small("remove", Some(Message::Remove(i))),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                voice,
            ]
            .spacing(4),
        )
        .padding(8)
        .width(Fill)
        .style(container::rounded_box)
        .into()
    }

    fn view_settings(&self) -> Element<'_, Message> {
        let settings = &self.saved.settings;
        let field = |f: Field| -> Element<'_, Message> {
            let (label, help) = f.label_and_help();
            let value = f.value(settings).as_str();
            let mut col = column![
                row![
                    text(label).size(14).width(180),
                    text_input("", value)
                        .on_input(move |v| Message::Setting(f, v))
                        .size(14)
                        .padding(5)
                        .width(Fill),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
            ]
            .spacing(2);
            if !help.is_empty() {
                col = col.push(row![space().width(190), text(help).size(12).style(text::secondary)]);
            }
            col.into()
        };
        let section = |title: &'static str| text(title).size(18).style(text::primary);

        let content = column![
            section("Being nice to YouTube"),
            text(
                "Downloads run one at a time, audio only, with a capped speed and a randomized pause \
                 between tracks. Previews stream just the part you listen to. Downloads are cached, \
                 so each track is fetched at most once."
            )
            .size(12)
            .style(text::secondary),
            field(Field::RateLimit),
            field(Field::MinDelay),
            field(Field::MaxDelay),
            field(Field::SleepRequests),
            field(Field::FetchLimit),
            field(Field::ExtraArgs),
            section("Voice"),
            field(Field::Tts),
            field(Field::Template),
            field(Field::VoiceLufs),
            field(Field::GapBefore),
            field(Field::GapAfter),
            row![
                space().width(190),
                button(text("Test voice").size(13))
                    .on_press(Message::TestVoice)
                    .style(button::secondary)
            ],
            section("Output"),
            field(Field::OutputDir),
            field(Field::Bitrate),
            section("Tools"),
            field(Field::Ytdlp),
            field(Field::Ffmpeg),
            field(Field::Ffprobe),
            field(Field::Player),
            text(format!(
                "Cache: {}",
                self.paths.audio.parent().unwrap_or(&self.paths.audio).display()
            ))
            .size(12)
            .style(text::secondary),
        ]
        .spacing(10)
        .padding([0, 12]);

        scrollable(content).height(Fill).into()
    }
}
