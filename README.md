# Radio Track Splitter

Splits a long radio-stream recording (e.g. an mp3) into separate track files. A
VGGish neural network finds where one track changes to the next, each cut is
snapped to the nearest quiet moment, and the tracks are written losslessly with
FFmpeg (`-c copy`, no re-encoding).

Two programs are built from this repository:

| Program | Executable | What it does |
|---|---|---|
| Editor (GUI) | `radio-track-splitter.exe` | Waveform editor: auto-detects split points, lets you fix them by hand, listen to each cut, then export |
| Command line | `radio-track-splitter-cli.exe` | Detects the split points and exports the tracks in one go |

## Installing

Run `RadioTrackSplitter-Setup-<version>.exe` (see [Building the installer](#building-the-installer)).
It installs per user, without admin rights, into
`%LOCALAPPDATA%\Programs\Radio Track Splitter` and downloads two things next to
the executables:

- **FFmpeg** (`ffmpeg`, `ffprobe`, `ffplay`; about 110 MB)
- **VGGish weights** `vggish-10086976.pth` (about 275 MB)

Both downloads are checked against a pinned SHA-256. A download is skipped if the
file is already on the PC (FFmpeg on `PATH`, weights in the torch cache).
Silent install: `RadioTrackSplitter-Setup-<version>.exe /S`.

## Usage

```powershell
radio-track-splitter.exe [recording.mp3]                            # editor
radio-track-splitter-cli.exe recording.mp3 --dry-run                # just print split points
radio-track-splitter-cli.exe recording.mp3 -o D:\tracks             # choose another output folder
```

Tracks are written to `%USERPROFILE%\Music\Splitter` by default (the editor's
output folder box, and the CLI's `-o`, change that). They are named after the
recording: `<recording name>_track_001.mp3`. The CLI's `--prefix` replaces the
`<recording name>_track_` part. Run
`radio-track-splitter-cli.exe --help` for all options (`--sensitivity`,
`--min-track-length`, `--snap-window`, `--weights`, ...).

The slow neural-network step is cached per recording, in `vggish_cache_<hash>.bin`
next to the executable (or in `%LOCALAPPDATA%\radio-track-splitter` if that folder
isn't writable). Reopening the same, unmodified file skips it. Deleting these files
is always safe.

The app looks for FFmpeg on `PATH` and in its own folder, and for the weights
next to the executable, then in the torch hub cache, then in
`%LOCALAPPDATA%\radio-track-splitter`. If none is found it downloads the weights
there on first use (needs `curl`, included with Windows 10 and later).

## Building from source

Requires the [Rust toolchain](https://rustup.rs) (MSVC target on Windows).

```powershell
cargo build --release
```

Executables are written to `target\release\`. The C runtime is linked
statically (see `.cargo/config.toml`), so no Visual C++ Redistributable is needed.

## Building the installer

The installer is an [NSIS](https://nsis.sourceforge.io) script in `installer/`.

**Prerequisites**

- Windows 10 or later (the installer uses the built-in `curl`, `tar` and `certutil`)
- Rust toolchain (as above)
- NSIS 3:

  ```powershell
  winget install NSIS.NSIS
  ```

**Build**

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\build.ps1
```

The `-ExecutionPolicy Bypass` part is needed on a default Windows setup, where
running `.\installer\build.ps1` directly fails with "running scripts is disabled on
this system". It applies to this one run only and changes no system setting.

The script:

1. finds `makensis` (on `PATH`, or in `Program Files (x86)\NSIS`);
2. downloads the [NScurl](https://github.com/negrutiu/nsis-nscurl) NSIS plugin on
   first run (pinned by SHA-256, cached in `installer\plugins\`), which gives the
   installer HTTPS downloads with a progress bar;
3. runs `cargo build --release --bins`;
4. compiles `installer\radio_track_splitter.nsi` with the version from `Cargo.toml`.

Result: `dist\RadioTrackSplitter-Setup-<version>.exe` (about 7 MB; FFmpeg and the
weights are downloaded when the installer runs, not embedded).

Use `-SkipCargo` to package the binaries already in `target\release\` without
rebuilding them:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\build.ps1 -SkipCargo
```

To release a new version, bump `version` in `Cargo.toml` and run the script.

**Changing the downloaded files**

FFmpeg and weights URLs and SHA-256 hashes are defined together at the top of
`installer\radio_track_splitter.nsi` (`FFMPEG_URL`/`FFMPEG_SHA256`,
`WEIGHTS_URL`/`WEIGHTS_SHA256`). Update the URL and hash together, or the
installer will reject the download.

**Testing the installer on a PC that already has FFmpeg and the weights**

The installer unticks a download when the file is already present, so to exercise
the download path build a test variant that skips that check, and install it into
a throwaway folder:

```powershell
& "${env:ProgramFiles(x86)}\NSIS\makensis.exe" /DVERSION=0.1.0 /DFORCE_DOWNLOADS /DOUTFILE=C:\temp\test-setup.exe installer\radio_track_splitter.nsi
C:\temp\test-setup.exe /S /D=C:\temp\inst
C:\temp\inst\Uninstall.exe /S _?=C:\temp\inst   # uninstall when done
```

The installer is not code-signed, so Windows SmartScreen will warn when it is run.
