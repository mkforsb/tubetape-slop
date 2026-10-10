# tubetape

Pick tracks from a YouTube channel and turn them into one long `.opus` / `.ogg`
mixtape, with a synthesized voice announcing artist and title before each track.
Built with Rust + [iced](https://iced.rs) 0.14.

## Requirements

Runtime tools (all configurable in Settings):

- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp): keep it current (`pip install -U yt-dlp`); YouTube changes often.
- `ffmpeg` + `ffprobe` with `libopus` (and `libvorbis` for .ogg output).
- A TTS engine that reads text on stdin and writes a WAV:
  - `espeak-ng` (default): `espeak-ng -v en-us -s 155 --stdin -w {out}`
  - [Piper](https://github.com/rhasspy/piper) sounds far better: `piper -m en_US-lessac-medium.onnx -f {out}`
- A player for previews: `ffplay` (default) or e.g. `mpv --no-video --start={start} {input}`.

```sh
cargo run --release
```

## Usage

1. Paste a channel URL or `@handle` and press **Fetch**. Uploads are listed newest first.
   **Load older uploads** at the bottom of the list fetches more.
2. **Play** previews a track starting a third of the way in. Use ±30s to skip around.
   Previews stream only what you listen to; nothing is downloaded.
3. Tick tracks to add them to the mixtape. Reorder them and edit what the voice says per track.
   The panel shows the estimated length and file size.
4. **Build mixtape** downloads what's missing, then renders. **Download only** prefetches.

The output gets one chapter per track, plus a `.txt` tracklist with timestamps next to it.

## Being nice to YouTube

- Audio-only formats, one download at a time, with a bandwidth cap (`--limit-rate`, default 1M).
- A randomized pause between downloads (default 8–20 s), and spacing between metadata requests.
- Downloads are cached, so a track is fetched at most once:
  `~/.cache/tubetape/audio` on Linux.

## Failure handling

Failed downloads are classified from yt-dlp's output:

| Kind | Examples | What happens |
| --- | --- | --- |
| Transient | timeouts, connection resets | retried after ~15 s, ~45 s, ~2 min (with jitter) |
| Throttled | HTTP 429, "confirm you're not a bot" | waits ~5 min, then ~15 min; if still throttled the job stops |
| Permanent | private, removed, region/age blocked | not retried |

yt-dlp also retries individual HTTP requests and fragments itself, with exponential backoff.
Waits show a countdown, and **Cancel** works at any point. Failed tracks are marked red
(hover for the error). **Remove failed** drops them from the mixtape. Channel listing and
preview lookups also retry transient errors.

If YouTube keeps asking for sign-in, add `--cookies-from-browser firefox` (or similar) under
*Extra yt-dlp args*.

## Tests

```sh
cargo test                                  # unit tests
cargo test -- --ignored                     # end-to-end: downloads 2 tracks and renders a mixtape
TUBETAPE_YTDLP=/path/to/yt-dlp cargo test -- --ignored
```
