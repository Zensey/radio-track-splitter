# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Windows-only Rust app that splits a long radio recording into per-track files: VGGish embeddings find where tracks change, cuts are snapped to the quietest nearby moment, and tracks are written losslessly with FFmpeg (`-c copy`). `README.md` covers end-user usage and installer building.

## Commands

```powershell
cargo build --release                     # target\release\radio-track-splitter.exe (GUI) and radio-track-splitter-cli.exe
cargo test                                # unit tests only (detect.rs, editor.rs, envelope.rs); no integration tests
cargo test detect::tests::peaks_match_scipy   # single test
cargo run --release --bin radio-track-splitter-cli -- recording.mp3 --dry-run
cargo run --release --bin radio-track-splitter -- [recording.mp3]
powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\build.ps1 [-SkipCargo]   # NSIS installer -> dist\RadioTrackSplitter-Setup-<version>.exe
                                          # (Bypass needed: the user's default policy blocks running .ps1 directly)
```

No linter config; use plain `cargo clippy` / `cargo fmt` if needed. Dependencies are compiled at `opt-level = 3` even for dev/test builds (candle is unusably slow otherwise), so the first dev build is slow.

Running anything end to end needs `ffmpeg`/`ffprobe` (and `ffplay` for GUI playback) plus the VGGish weights; see "External dependencies" below.

## Architecture

The package is a library (`src/lib.rs`) plus two binaries declared explicitly in `Cargo.toml` (`autobins = false`): the GUI is `src/bin/gui.rs` and named `radio-track-splitter`, the CLI is `src/bin/cli.rs` and named `radio-track-splitter-cli`. Both are thin front ends over the same pipeline.

Pipeline (shared code lives in `pipeline.rs`, called by both binaries):

1. `audio.rs` shells out to `ffprobe`/`ffmpeg`: probe duration, decode to 16 kHz mono 16-bit WAV, read into `Vec<i16>`. Final track export is also an `ffmpeg -c copy` call. **Every console child process (ffmpeg, ffprobe, ffplay, curl) must be started with `audio::quiet_command`**, which sets `CREATE_NO_WINDOW`; the GUI has no console, so a plain `Command::new` flashes a terminal window per call (measured: 5 visible windows just to open one file). `explorer` is a windowed program and doesn't need it.
2. `vggish.rs` computes a log-mel spectrogram and runs the VGGish CNN on candle (CPU), giving one 128-d embedding per 0.96 s. It is written to match torchvggish numerically (`postprocess=False`), including the H,W,C flatten order, so keep it in sync with that reference when touching it.
3. `torch_legacy.rs` is a hand-written pickle reader for PyTorch's pre-1.6 `torch.save` format, because torchvggish's published `.pth` uses it and candle cannot load it. `Vggish::load` falls back to candle's own loader for zip-format files.
4. `detect.rs` turns embeddings into split points: novelty curves (cosine distance between mean embeddings before/after each frame) at several window scales, z-scored and combined with `min()` so a boundary must stand out at every scale; then `find_peaks` (tests check parity with `scipy.signal.find_peaks`) and `snap_to_energy_minimum` on the raw samples.
5. `cache.rs` stores embeddings in one file per input, `vggish_cache_<FNV-1a of the canonical input path>.bin`, in `%TEMP%\radio-track-splitter\` (`cache::cache_path`); it is recomputable data, so Temp is the right home even though Windows may clear it. The uninstaller removes that folder's cache files, and still removes `vggish_cache_*.bin` from the install dir where earlier versions wrote them. The file's header holds the input's (size, mtime); a mismatch, bad magic or wrong length is a cache miss. A failed cache write only warns, it never discards computed embeddings. Embeddings are the slow part; the detection parameters (sensitivity, scales, min length, snap window) are cheap to re-run.

Output files are named `<input stem>_track_NNN.<input extension>` (`audio::default_prefix`, `audio::output_extension`): the tracks keep the recording's format and sample rate, and ffmpeg picks the container from that extension. The GUI's overwrite warning uses the same prefix and extension, so keep them in sync. The CLI's `--prefix` replaces the prefix entirely.

`audio::extract_segments` cuts per format (`Cut`, built by the pure `cut_args`, unit-tested without ffmpeg), each choice was measured against real ffmpeg output:
- default (mp3, m4a/aac/mp4, wav): `-c copy` with `-ss/-to` before `-i`.
- ogg/oga/opus: `-c copy` but `-ss/-to` *after* `-i`. Seeking on the input can only enter an Ogg file at a page start, so tracks began up to ~0.8 s early and replayed the end of the previous track.
- flac: `-c copy -c:a flac`, a lossless re-encode. Plain copy leaves the whole recording's length in each track's FLAC header (players show every track as that long); the re-encoded audio decodes bit-identically (verified by joining the tracks and comparing MD5s).
Only these output formats were verified end to end (mp3, m4a, aac, mp4, flac, ogg, wav); recordings in another format fall into the default copy path.

GUI specifics (`src/bin/gui.rs`, egui/eframe with glow):

- Heavy work (decode, embeddings, export) runs on spawned threads that report back over an `mpsc` channel of `Msg`, drained in `poll()` each frame. Do not block the UI thread.
- `editor.rs` (marker/selection/playhead/view state) and `envelope.rs` (500 bins/s min/max waveform for drawing at any zoom) are deliberately pure logic with no egui dependency so they stay unit-testable.
- Playback shells out to `ffplay` (`-nodisp`).
- The window title is `<program name> - <file name>` (`window_title`: no folder, plain program name when nothing is open). `ui()` calls `title_update` every frame and only sends `ViewportCommand::Title` when the text changed; the file name appears once the recording has finished loading, because that is when `App::input` is set.
- The About dialog (`centered_dialog`) opens centred on the waveform (`App::signal_rect`, stored each frame) and stays draggable, which rules out `.anchor()`/`.fixed_pos()`. It gets a new window id per opening (`about_serial`) because egui remembers a window's position by id and, in 0.36, ignores both `default_pos` and `current_pos` for a window that already has one (the title-drag code re-applies the stored position right after `Area::begin`); reusing the id would reopen it wherever it was last left. Headless egui tests at the bottom of `gui.rs` cover centring, dragging and re-centring (`Context::run_ui` with a `RawInput`; drop the returned `FullOutput` with `drop_without_applying_deltas()`).
- Per-recording marks and output dir persist in `%LOCALAPPDATA%\radio-track-splitter\gui_state.json`. The old `split_radio` folder is still read as a fallback (the project was renamed from `split_radio`).
- Release builds use `windows_subsystem = "windows"`; debug builds keep a console.

## External dependencies and lookup order

The app spawns tools by bare name (`Command::new("ffmpeg")`), relying on Windows searching the executable's own directory first. That is how the installer's FFmpeg copy is found without touching `PATH`; there is no explicit path handling in the code.

Weights (`vggish::ensure_weights`, `vggish-10086976.pth`) are searched in order: `--weights` if given, next to the exe, `~\.cache\torch\hub\checkpoints`, `%LOCALAPPDATA%\radio-track-splitter`, the legacy `%LOCALAPPDATA%\split_radio`. If none exists it downloads to `%LOCALAPPDATA%\radio-track-splitter` with `curl`.

## Installer (`installer/`)

`radio_track_splitter.nsi` builds a small per-user installer that downloads FFmpeg and the weights at install time (NScurl plugin, SHA-256 pinned) into the install folder, next to the exes. The URLs and hashes are `!define`s at the top of the `.nsi`; change URL and hash together. The version is defined only in `Cargo.toml` (`[package] version`): `build.ps1` reads it with `cargo metadata` and passes `/DVERSION` and `/DVERSION_NUM` (its x.y.z part, for the Windows version resource), and the `.nsi` deliberately has no default and aborts if they are missing; the app uses `env!("CARGO_PKG_VERSION")`. Never write the number anywhere else. `build.ps1 -ForceDownloads` builds the `-test` variant instead of hand-running `makensis` with `/DFORCE_DOWNLOADS`. `installer/plugins/` is generated by `build.ps1` and gitignored. CI (`.github/workflows/build-installer.yml`, windows-latest) runs ONLY when a version tag matching `v[0-9]+.[0-9]+.[0-9]+*` is pushed (deliberately no branch, pull-request or manual triggers, so tests run at release time only). It runs `cargo test --release`, `build.ps1`, then `installer/verify.ps1` (installer version == Cargo.toml, tag == `v<version>`, exes must not import VCRUNTIME140, i.e. `.cargo/config.toml` must be committed), uploads the installer, and a second job puts it on a draft release. Every run is a tag run, so the release job has no `if` guard. The workflow was validated with actionlint and by running its exact steps on a cold build in a clean copy of the tree; `verify.ps1`'s failure paths were tested against a fake project. Local shortcut: `cargo test` and the installer share compiled dependencies only in the same profile, hence `--release` in CI. The uninstaller deletes files by name, so any new app-written file in the install folder must be added to its `Delete` list or `RMDir` will leave the folder behind (the weights are the one such file today). Test variant on a machine that already has FFmpeg/weights: build with `/DFORCE_DOWNLOADS` (otherwise those sections are auto-unticked).

## Notes

- `.cargo/config.toml` links the MSVC CRT statically so the exes need no VC++ Redistributable; keep it when changing targets.
- `_prototype_/` holds the original Python implementation (`split_radio_nn.py` etc.) that the Rust code was ported from; it is the numerical reference but not part of the build.
