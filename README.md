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
- **Automatic Intro Detection & Fullscreen Skip Intro**: Finds the repeating opening sequence across episodes (checking pure-Rust MKV/MP4 chapter markers, optional `ffmpeg` audio fingerprints, and built-in zero-dependency browser visual frame matching) and displays an overlay **Skip Intro** button over the video—including in fullscreen—plus a highlighted intro segment on the timeline and manual fine-tuning.
- **Non-Destructive Rescan & Manual Overrides**: Re-check folders anytime to pick up new or removed files without losing saved watch progress, or customize episode playback order.
- **Automatic Git Updates (Offline-Safe)**: Checks the configured Git remote on startup (and via the in-app **↻ Updates** button), automatically creates WAL-safe SQLite backups before updating, fast-forwards cleanly if behind, and skips gracefully without crashing when offline or running outside a Git checkout.

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

GitHub Actions automatically tests and compiles release binaries for all three operating systems (`.github/workflows/ci.yml` and `.github/workflows/release.yml`). Tagging a release (e.g. `git tag v1.1.0 && git push --tags`) publishes prebuilt binaries to GitHub Releases.

## Keyboard Shortcuts (Watch View)

- `Space` / `K` — Play / Pause
- `←` / `→` — Seek backward / forward 10 seconds
- `S` — Skip Intro (when active)
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
