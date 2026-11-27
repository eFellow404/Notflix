# Notflix

A lightweight, single-binary local TV show player written in **Rust**, built for plain folders of DVD-ripped video files.

## Features

- **Single Standalone Executable**: Compiles into one native binary for **Linux, macOS, and Windows** with the entire web UI embedded (`rust-embed`) and SQLite bundled (`rusqlite`) — zero Python, runtime, or external database setup needed.
- **Plain Folder Library**: Add any local folder containing TV episodes (flat folders or nested `Season` / `Disc` subfolders) using the built-in folder picker or by pasting a path.
- **Intelligent Episode Ordering**: Automatically inspects folder structure and messy DVD rip filenames—handling `S01E02`, `1x02`, `Season 1 Episode 2`, MakeMKV `Disc 1/title_t00.mkv`, `VTS_01_1.VOB`, 3-digit `101/201` season codes, natural numbering (`ep2` before `ep10`), constant show-title numbers (`24`, `The 100`), `Pilot` detection, and `Extras`/`Bonus` separation.
- **Persisted Doubly Linked List**: Stores every show's episode sequence in a genuine doubly linked list (`EpisodeLinkedList` with `head`, `tail`, `current`, and `prev`/`next` pointers persisted in SQLite).
- **Async HTTP 206 Video Streaming**: Powered by `tokio` + `axum` with 256 KiB chunked streaming, instant scrubbing, and silent disconnect handling.
- **Automatic Timeplace Saving**: Saves your exact playback second periodically, on pause/seek/episode switch, and on tab close (`sendBeacon`), resuming right where you stopped.
- **Instant Next / Previous Episode**: Click **Next Episode** right at the start of any episode (via the top bar, quick overlay, or `N` shortcut) to step along `current.next` in $O(1)$.
- **Automatic Intro & Outro Detection**: Finds repeating opening sequences across episodes (via pure-Rust MKV/MP4 chapters, optional `ffmpeg` audio fingerprinting, and built-in zero-dependency browser visual frame matching) for fullscreen **Skip Intro**, and detects closing credits/outros to trigger the **Up Next** card right when credits roll.
- **Automatic Subtitles (`.srt` & `.vtt`)**: Auto-discovers subtitle files beside episodes or inside `Subs/` / `Subtitles/` subfolders, converts `.srt` (including Latin-1/UTF-8) to WebVTT on the fly in pure Rust, and provides a fullscreen **CC** toggle (`C` shortcut).
- **On-the-Fly Audio Remuxing & Track Selection**: When `ffmpeg`/`ffprobe` is installed, probes audio tracks (flagging browser-incompatible DVD codecs like AC3/DTS/TrueHD) and streams lossless video + AAC audio remuxes with audio track switching and a one-click **Fix Audio (Remux)** fallback.
- **Local Posters, Thumbnails & Continue Watching**: Automatically renders local show artwork (`poster.jpg`, `folder.png`, `cover.webp`, etc.), sidecar or lazy-cached episode thumbnails, and a horizontal **Continue Watching** strip for shows in progress.
- **Portable Watch-History Export / Import**: Export and restore your watch progress, watched flags, and intro/outro markers as portable JSON matched across operating systems by folder path or show name.
- **Auto-Open Browser on Launch**: Automatically opens Notflix in your default browser on startup (disable with `--no-open` or `NOTFLIX_NO_OPEN=1`).
- **Non-Destructive Rescan & Manual Overrides**: Re-check folders anytime to pick up new or removed files without losing saved watch progress, or customize episode playback order.
- **Automatic Git & GitHub Releases Updates (Offline-Safe)**: Updates seamlessly from either a Git checkout or standalone GitHub Releases binaries (pure-Rust TLS, magic-byte integrity verification, WAL-safe SQLite backup before swap, and automatic `.old` cleanup on Windows).

## Quickstart

### Build & Run from Source

Requires **Rust 1.80+** (`rustup` / `cargo`). No system SQLite or C library installation is required.

```bash
cargo run --release
```

Or build the standalone binary once and run it directly:

```bash
cargo build --release
./target/release/notflix
```

Then open **http://127.0.0.1:8080** in your browser.

### Options

```bash
notflix --host 127.0.0.1 --port 8080 --db ./my_notflix.db
notflix --static-dir ./static          # Serve live frontend files from disk (dev mode)
notflix --no-open                      # Do not auto-open the web browser on startup
notflix --quiet                        # Suppress HTTP logs
notflix --no-auto-update               # Skip startup remote update check
notflix --check-update                 # Check remote status and exit
notflix --update-timeout 5             # Network timeout in seconds (default: 5)
```

## Cross-Platform Builds (Linux, macOS, Windows)

Notflix builds natively on **Linux** (`x86_64`), **macOS** (Apple Silicon `aarch64` & Intel `x86_64`), and **Windows** (`x86_64-pc-windows-msvc`):

```bash
cargo build --release
```

GitHub Actions automatically tests and compiles release binaries for all three operating systems (`.github/workflows/ci.yml` and `.github/workflows/release.yml`), publishing a fresh GitHub Release on every commit pushed to `main` (and on version tags).

## Keyboard Shortcuts (Watch View)

- `Space` / `K` — Play / Pause
- `←` / `→` — Seek backward / forward 10 seconds
- `S` — Skip Intro (when active)
- `C` — Toggle / cycle Subtitles
- `M` — Mute / Unmute
- `N` — Next episode (`current.next`)
- `P` — Previous episode (`current.prev`)
- `R` — Restart current episode from `0:00`
- `F` — Toggle fullscreen (keeps Skip Intro and Up Next overlays visible)

## Running Tests & Lints

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Security Note

Notflix is designed as a **local personal media player** bound to `127.0.0.1` by default and includes a local directory browser without authentication. Avoid exposing it directly to untrusted networks (e.g. `--host 0.0.0.0` on a public network).

## License

MIT — see [LICENSE](LICENSE).
