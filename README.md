# Notflix

A lightweight local TV show player built for plain folders of DVD-ripped video files.

## Features

- **Plain Folder Library**: Add any local folder containing TV episodes (flat folders or nested `Season` / `Disc` subfolders) using the built-in folder picker or by pasting a path.
- **Intelligent Episode Ordering**: Automatically inspects folder structure and messy DVD rip filenames—handling `S01E02`, `1x02`, `Season 1 Episode 2`, MakeMKV `Disc 1/title_t00.mkv`, `VTS_01_1.VOB`, 3-digit `101/201` season codes, natural numbering (`ep2` before `ep10`), constant show-title numbers (`24`, `The 100`), `Pilot` detection, and `Extras`/`Bonus` separation.
- **Persisted Doubly Linked List**: Stores every show's episode sequence in a genuine doubly linked list (`EpisodeLinkedList` with `head`, `tail`, `current`, and `prev`/`next` pointers persisted in SQLite).
- **Automatic Timeplace Saving**: Saves your exact playback second periodically, on pause/seek/episode switch, and on tab close (`sendBeacon`), resuming right where you stopped.
- **Instant Next / Previous Episode**: Click **Next Episode** right at the start of any episode (via the top bar, quick overlay, or `N` shortcut) to step along `current.next` in $O(1)$.
- **Automatic Intro Detection & Fullscreen Skip Intro**: Finds the repeating opening sequence across episodes (checking MKV/MP4 chapter markers, optional `ffmpeg` audio fingerprints, and built-in zero-dependency browser visual frame matching) and displays an overlay **Skip Intro** button over the video—including in fullscreen—plus a highlighted intro segment on the timeline and manual fine-tuning.
- **Non-Destructive Rescan & Manual Overrides**: Re-check folders anytime to pick up new or removed files without losing saved watch progress, or customize episode playback order.
- **Automatic Git Updates (Offline-Safe)**: Checks the configured Git remote on startup (and via the in-app **↻ Updates** button), automatically backs up your SQLite database before updating, fast-forwards cleanly if behind, and skips gracefully without crashing when offline or if local edits exist.

## Quickstart

Requires **Python 3.10+** (zero third-party dependencies).

```bash
python3 run.py
```

Then open **http://127.0.0.1:8080** in your browser.

### Options

```bash
python3 run.py --host 127.0.0.1 --port 8080 --db ./my_notflix.db
python3 run.py --no-auto-update       # Skip startup remote update check
python3 run.py --check-update         # Check remote status and exit
python3 run.py --update-timeout 5     # Network timeout in seconds (default: 5)
```

## Keyboard Shortcuts (Watch View)

- `Space` / `K` — Play / Pause
- `←` / `→` — Seek backward / forward 10 seconds
- `S` — Skip Intro (when active)
- `M` — Mute / Unmute
- `N` — Next episode (`current.next`)
- `P` — Previous episode (`current.prev`)
- `R` — Restart current episode from `0:00`
- `F` — Toggle fullscreen (keeps Skip Intro and Up Next overlays visible)

## Running Tests

```bash
python3 -m unittest discover -s tests -v
```

## Security Note

Notflix is designed as a **local personal media player** bound to `127.0.0.1` by default and includes a local directory browser without authentication. Avoid exposing it directly to untrusted networks (e.g. `--host 0.0.0.0` on a public network).

## License

MIT — see [LICENSE](LICENSE).

