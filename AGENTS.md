# Notflix — Agent Guide & Complete Feature Inventory

This document is the authoritative reference for future AI agents and contributors working on **Notflix** — a lightweight, single-binary local TV show player written in Rust for plain folders of DVD-ripped video files.

---

## 1. Architecture & Tech Stack

- **Language & MSRV**: Rust 2021 edition, **MSRV 1.80** (`Cargo.toml`). Avoid post-1.80 std APIs (e.g., `Option::is_none_or`). Zero `unsafe` code.
- **Backend**: `axum 0.8` + `tokio` async HTTP server, bundled SQLite via `rusqlite` (no external DB or system SQLite needed), pure-Rust TLS HTTP client via `ureq` (`rustls` + `webpki-roots`, no OpenSSL).
- **Frontend**: Vanilla HTML/CSS/ES modules in `static/`, embedded directly into the release binary via `rust-embed` (with `--static-dir ./static` override for live dev). Zero JS build step or npm dependencies.
- **Cross-Platform**: Builds natively and passes CI on **Linux (`x86_64`)**, **macOS (`aarch64` & `x86_64`)**, and **Windows (`x86_64-pc-windows-msvc`)**.
- **Build Metadata**: `build.rs` embeds `NOTFLIX_BUILD_SHA` (from `GITHUB_SHA` or `git rev-parse --short=7 HEAD`, fallback `"dev"`) exposed as `notflix::BUILD_SHA`.

### Module Map
| File | Responsibility |
| --- | --- |
| `build.rs` | Embeds 7-char `NOTFLIX_BUILD_SHA` at compile time |
| `src/lib.rs` | Crate exports (`VERSION`, `BUILD_SHA`, `open_browser`, `strip_windows_verbatim_prefix`) |
| `src/main.rs` | CLI (`clap`), startup `.old` cleanup, startup auto-update + seamless self-relaunch, auto-open browser, Axum server bootstrap |
| `src/util.rs` | Path canonicalization, Windows `\\?\` stripping, `~` expansion, timeout-safe subprocess runner (non-blocking on grandchild pipes), cross-platform `open_browser` |
| `src/error.rs` | `NotflixError` & HTTP error responses |
| `src/intelligence.rs` | Recursive folder scanning & intelligent DVD-rip episode ordering |
| `src/linked_list.rs` | Arena-backed `EpisodeLinkedList` doubly linked list (`head`, `tail`, `current`, `prev`/`next`, `free_list` O(1) reuse) |
| `src/storage.rs` | SQLite WAL persistence, non-destructive `ALTER TABLE` migrations, lock-free disk rescanning, portable JSON watch-history export/import |
| `src/media.rs` | HTTP 206 byte-range video streaming (256 KiB chunks), `ffprobe` audio stream inspection, on-the-fly `ffmpeg` fragmented-MP4 audio remux streaming, show poster & episode thumbnail discovery/caching |
| `src/subtitles.rs` | Subtitle `.srt`/`.vtt` discovery (same folder + `Subs/`/`Subtitles/` + nested stem folders), language/tag parsing, pure-Rust SRT→WebVTT conversion (UTF-8 & Latin-1) |
| `src/chapters.rs` | Pure-Rust MKV (EBML) and MP4 (`chpl`/`trak`) chapter parser + intro & outro chapter classifiers |
| `src/intro_detector/` | Modular intro & outro fingerprinting (`mod.rs`, `audio.rs`, `matching.rs`): 16-band Goertzel + CMN audio vectors, Kadane gap-debt hard split (`MAX_INTERIOR_MISMATCH_SEC = 3.0`), `best_contiguous_subrun()`, multi-episode consensus trimming, tail outro matching |
| `src/updater.rs` | Dual-mode offline-safe updater: Git fast-forward updater + GitHub Releases standalone binary updater, WAL-safe SQLite backups, ELF/PE/Mach-O magic-byte validation, atomic binary swap & `.old` cleanup |
| `src/server/` | Modular Axum router (`mod.rs`, `shows.rs`, `episodes.rs`, `assets.rs`, `system.rs`) |
| `static/` | Embedded web UI (`index.html`, `styles.css`, `app.js`, `js/{state,utils,api,library,sidebar,player,intro,modals,updater}.js`) |

---

## 2. Complete Feature Inventory

### A. Plain-Folder Library & Intelligent Episode Ordering
- Add any local folder via the built-in server-side filesystem browser (`GET /api/fs/browse`) or by pasting a path (`POST /api/shows`).
- Supports flat folders and nested `Season` / `Disc` / `Bonus` directory layouts.
- Intelligent filename/folder parser (`src/intelligence.rs`) handles:
  - `S01E02`, `1x02`, `Season 1 Episode 2`, `Ep 02`, `Part 2`
  - MakeMKV disc rips (`Disc 1/title_t00.mkv`, `D1_T02`), DVD `VTS_01_1.VOB`
  - 3-digit `SEE` codes (`101`, `201`) with sibling consensus
  - Natural numeric sorting (`ep2` before `ep10`, arbitrary-length digits)
  - Constant show-title number suppression (`24`, `The 100`) via sibling consensus
  - Roman numerals, `Pilot` priority, and `Extras` / `Bonus` / `Featurettes` separation
  - Ignores hidden dotfiles/dirs and non-video files
- Non-destructive folder rescanning (`POST /api/shows/{id}/rescan`): scans disk lock-free outside the DB lock, picks up newly added/removed files, and preserves watch positions, watched flags, manual ordering, and intro/outro markers.
- Show rename (`PATCH /api/shows/{id}`), show delete (`DELETE /api/shows/{id}`), bulk mark watched/unwatched (`POST /api/shows/{id}/watched`).

### B. Persisted Doubly Linked List Episode Sequence
- Every show's episode order is maintained in `EpisodeLinkedList` (`head_id`, `tail_id`, `current_id`, per-node `prev_id` and `next_id`) persisted in SQLite.
- Supports $O(1)$ pointer traversal (`POST /api/shows/{id}/next`, `POST /api/shows/{id}/prev`, `POST /api/shows/{id}/select`), drag-and-drop / manual episode reordering (`POST /api/shows/{id}/reorder`), and reset-to-smart-order.

### C. Async Video Streaming, Audio Track Selection & On-the-Fly Remuxing
- Direct HTTP 206 byte-range streaming (`GET/HEAD /api/episodes/{id}/stream`) with 256 KiB chunked streaming, MIME detection, instant scrubbing, and silent disconnect handling.
- Optional `ffprobe` audio stream probing (`GET /api/episodes/{id}/tracks`) detects codec, channels, language, title, and `browser_compatible` status (flagging DVD codecs like AC3, EAC3, DTS, TrueHD, MLP).
- On-the-fly fragmented-MP4 audio remuxing (`GET /api/episodes/{id}/stream?remux=1&start=<sec>&audio=<idx>`): copies video losslessly (`-c:v copy`) while transcoding audio to stereo AAC (`-c:a aac -ac 2 -b:a 192k`) via `tokio::process::Command::kill_on_drop(true)` so `ffmpeg` terminates immediately on client disconnect.
- Watch UI includes an audio track switcher (`#ctrlAudioBtn`, when multiple tracks or incompatible audio are detected), a one-click **Fix Audio (Remux)** button (`#btnFixAudioRemux`) in the playback error banner, and transparent remux time-offset handling for seeking and progress persistence.

### D. Subtitles Support (`.srt` & `.vtt`)
- Automatic discovery (`src/subtitles.rs`) alongside video files, inside case-insensitive `Subs/`, `Subtitles/`, `Sub/`, `CC/` folders, and inside nested `<sub_dir>/<video_stem>/` folders.
- Language & tag recognition (ISO-639-1/2 codes and full names, `(SDH)`, `(Forced)`, `(Commentary)`), deterministic English-first ordering.
- Pure-Rust `.srt` → WebVTT conversion (`GET /api/episodes/{id}/subtitles/{index}`) stripping UTF-8 BOM, normalizing CRLF/CR, decoding UTF-8 with ISO-8859-1 (Latin-1) fallback, and leaving dialogue commas untouched.
- Fullscreen **CC** toggle button (`#ctrlSubBtn`), `C` keyboard shortcut to cycle subtitle tracks/Off, dynamic `<track>` attachment, and persisted preference in `localStorage` (`notflix.subtitles`).

### E. Automatic Intro & Outro (Credits) Detection
- **3-Tier Intro Detection** (`POST /api/shows/{id}/detect-intros`):
  1. Pure-Rust embedded MKV/MP4 chapter markers (`Opening`, `Intro`, `OP`, `Theme Song`).
  2. Optional `ffmpeg` 16-band Goertzel + Cepstral Mean Normalization (CMN) audio fingerprinting.
  3. Zero-dependency browser visual frame fingerprinting (6×4 12%-cropped grid at 1.0s step via `static/js/intro.js`).
- Hard interior mismatch guard (`MAX_INTERIOR_MISMATCH_SEC = 3.0`, leaky gap debt + `best_contiguous_subrun()`) prevents opening studio logos/fanfares from bridging across cold opens into the intro; multi-episode consensus trimming + 2-frame edge trim ensure tight intro boundaries.
- Fullscreen **Skip Intro** overlay (`S` shortcut), highlighted seekbar intro segment, and manual intro start/end editor (`PATCH /api/episodes/{id}/intro`).
- **Outro / Credits Detection** for Smart **Up Next**:
  - Persists `outro_start_sec` and `outro_source` in SQLite via non-destructive migration.
  - Detects closing credits via embedded chapters (`End Credits`, `Credits`, `Ending`, `Outro`, `ED`, etc. in the final 45%), optional `ffmpeg -sseof -180` tail audio fingerprinting, or browser `tail_fingerprints` (last 120s visual scan when duration ≥ 240s).
  - Triggers `#upNextCard` when `currentTime >= outro_start_sec` (falling back to final 15s when duration > 45s) and draws `#seekOutroMarker` on the seekbar.

### F. Playback UX, Timeplace Persistence & Shortcuts
- Automatic watch-progress saving (`PATCH/POST /api/episodes/{id}/progress`) periodically during playback, on pause/seek/episode switch, and via `navigator.sendBeacon` on tab close; auto-marks watched near completion.
- Fullscreen-compatible custom overlay controls (Play/Pause, seekbar with intro/outro markers, volume/mute, playback speed, Skip Intro, Up Next countdown, CC subtitles, Audio track switcher, episode sidebar drawer).
- Keyboard shortcuts in Watch view: `Space`/`K` (play/pause), `←`/`→` (seek ±10s), `S` (skip intro), `C` (cycle subtitles), `M` (mute), `N` (next episode), `P` (previous episode), `R` (restart from 0:00), `F` (fullscreen).

### G. Local Show Posters, Episode Thumbnails & Continue Watching
- **Show Posters**: Auto-discovers local artwork in each show folder (`poster`, `folder`, `cover`, `show`, `fanart`, `backdrop` with `.jpg`/`.jpeg`/`.png`/`.webp`, case-insensitive), exposed via `has_poster` on `ShowSummary` and `GET /api/shows/{id}/poster` (`Cache-Control: public, max-age=3600`), with automatic fallback to gradient monogram cards on missing/broken art.
- **Episode Thumbnails**: Serves local sidecar `<stem>.(jpg|jpeg|png|webp)` / `<stem>-thumb.*` or lazily generates & caches 480px JPEG thumbnails via `ffmpeg` in `<db_dir>/thumbs/` (`GET /api/episodes/{id}/thumbnail`, `Cache-Control: public, max-age=86400`).
- **Continue Watching Strip**: Horizontal scroll row (`#continueSection` / `#continueStrip`) in the Library view displayed whenever ≥2 shows are in progress (`last_watched_at` set and `watched_count < episode_count`).

### H. Portable Watch-History Export & Import
- `GET /api/library/export` downloads a versioned JSON snapshot (`notflix-watch-history.json`) containing all shows, current episode pointers, last-watched timestamps, episode positions, durations, watched flags, and intro/outro markers.
- `POST /api/library/import` restores watch history across machines and operating systems by matching shows by normalized `folder_path` or case-insensitive `name`, and episodes by slash-normalized `relative_path` or unique `filename`.
- Wired to **Export** (`#btnExportLibrary`) and **Import** (`#btnImportLibrary`) buttons in the Library header.

### I. Auto-Open Browser on Launch
- Automatically opens the bound local URL in the user's default browser after `TcpListener::bind` succeeds (`xdg-open` on Linux, `open` on macOS, `cmd /C start` on Windows, detached with null stdio).
- Maps wildcard bind hosts (`0.0.0.0`, `::`) to `http://127.0.0.1:<port>`.
- Disable with `--no-open` CLI flag or `NOTFLIX_NO_OPEN=1`.

### J. Automatic Updates on Every Run (Git & GitHub Releases)
- **Runs automatically on every new program launch** (unless explicitly disabled via `--no-auto-update`, `NOTFLIX_AUTO_UPDATE=0`, or `NOTFLIX_SKIP_AUTO_UPDATE=1`).
- **Dual-Mode Resolution**:
  - Inside a Git checkout: checks remote via `git fetch`, skips safely on dirty tracked files or diverged history, creates a WAL-safe SQLite backup (`<db_dir>/backups/`), and applies `git merge --ff-only`.
  - Standalone prebuilt binary (outside `.git` or `NOTFLIX_UPDATE_MODE=release`): queries `https://api.github.com/repos/<slug>/releases/latest` (default `eFellow404/Notflix`, override `NOTFLIX_RELEASE_REPO`), compares against embedded `BUILD_SHA`, creates a WAL-safe SQLite backup, downloads the platform asset (`notflix-linux-x86_64`, `notflix-macos-aarch64`, `notflix-macos-x86_64`, `notflix-windows-x86_64.exe`, capped at 128 MiB), validates ELF/PE/Mach-O magic bytes, stages to `<exe>.new`, and atomically swaps (Unix `rename` with `0o755`; Windows `<exe>` → `<exe>.old` swap with rollback and startup cleanup via `cleanup_old_binary_artifacts()`).
- **Seamless Self-Relaunch on Startup Update**: When a startup update replaces the running executable (GitHub Release binary update, or a Git fast-forward of a repo-root binary outside `target/`), `main.rs` immediately re-executes the updated binary with the original CLI arguments and `NOTFLIX_SKIP_AUTO_UPDATE=1` + `NOTFLIX_RELAUNCHED=1` (preventing loops), so the new version runs immediately without requiring a manual restart.
- **Offline-Safe**: Network errors, DNS failures, timeouts, and 5xx/rate-limits degrade cleanly to `status: "offline"` and never block startup.
- **In-App Controls**: `↻ Updates` button (`GET /api/update/status`, `POST /api/update/check`, `POST /api/update/apply`).

---

## 3. CLI Flags & Environment Variables

### CLI Flags (`notflix --help`)
- `--host <HOST>` (default `127.0.0.1`)
- `--port <PORT>` (default `8080`)
- `--db <PATH>` (default `~/.notflix/notflix.db` or platform data path)
- `--static-dir <DIR>` (optional disk override for `static/` in dev mode)
- `--no-open` (do not auto-open browser on startup)
- `--quiet` (suppress HTTP access logs)
- `--no-auto-update` (skip startup auto-update check)
- `--check-update` (check remote update status, print result, and exit)
- `--update-timeout <SEC>` (network timeout in seconds, default `5.0`)

### Environment Variables
- `NOTFLIX_NO_OPEN=1` — suppress auto-opening browser
- `NOTFLIX_AUTO_UPDATE=0|false|no|off` or `NOTFLIX_SKIP_AUTO_UPDATE=1` — disable startup auto-update
- `NOTFLIX_UPDATE_MODE=auto|git|release` — force Git or GitHub Releases update mode
- `NOTFLIX_RELEASE_REPO=owner/repo` — override GitHub Releases repository (default `eFellow404/Notflix`)
- `NOTFLIX_RELAUNCHED=1` — internal loop guard set automatically after startup self-relaunch

---

## 4. Complete REST API Route Map

- `GET /api/health` — `{ status, version, build_sha, commit, ffmpeg_available }`
- `GET /api/update/status` | `POST /api/update/check` | `POST /api/update/apply`
- `GET /api/fs/browse?path=...`
- `GET /api/library/export` | `POST /api/library/import`
- `GET /api/shows` | `POST /api/shows`
- `GET /api/shows/{id}` | `PATCH/POST /api/shows/{id}` | `DELETE /api/shows/{id}`
- `GET /api/shows/{id}/poster`
- `POST /api/shows/{id}/rescan`
- `POST /api/shows/{id}/next` | `POST /api/shows/{id}/prev` | `POST /api/shows/{id}/select`
- `POST /api/shows/{id}/reorder`
- `POST /api/shows/{id}/detect-intros` (detects intros + outros)
- `POST /api/shows/{id}/watched`
- `PATCH/POST /api/episodes/{id}/progress`
- `PATCH/POST /api/episodes/{id}/intro`
- `GET /api/episodes/{id}/tracks`
- `GET /api/episodes/{id}/subtitles/{index}`
- `GET /api/episodes/{id}/thumbnail`
- `GET/HEAD /api/episodes/{id}/stream` (supports `?remux=1&start=<sec>&audio=<idx>`)
- `GET/HEAD /` and `GET/HEAD /static/{*path}` (embedded frontend)

---

## 5. Development, Testing & Release Workflow

### Verification Commands
```bash
# JavaScript ES-module syntax check (Node 18 requires stdin redirection)
for f in static/app.js static/js/*.js; do node --input-type=module --check < "$f" || exit 1; done

# Formatting, Clippy, and full Rust test suite
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test

# Release build (embeds static/ via rust-embed)
cargo build --release
cp ./target/release/notflix ./notflix
```

### Important Invariants for Future Agents
1. **Static Embedding**: Always rebuild `--release` (and update `./notflix` if tracking the precompiled Linux binary) after editing any file in `static/`.
2. **Schema Backwards Compatibility**: Never drop/recreate SQLite tables destructively; always add new columns via `MIGRATIONS` (`ALTER TABLE`) in `src/storage.rs` with sensible defaults.
3. **Subprocess Timeout Safety**: `run_command_with_timeout` in `src/util.rs` intentionally returns without joining pipe reader threads on timeout so grandchild processes cannot hang cleanup.
4. **CI & Auto-Releases**: `.github/workflows/ci.yml` runs fmt, Clippy, tests, and release builds on Linux/macOS/Windows. `.github/workflows/release.yml` automatically creates a new GitHub Release with prebuilt binaries for all 4 targets on every push to `main` and on `v*` tags.
5. **Keep This File Current**: Whenever you add, change, or remove a feature, route, CLI flag, or env var, update `AGENTS.md` alongside your code changes.
