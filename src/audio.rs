use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A command for a console program (ffmpeg, ffprobe, ffplay, curl) that runs without a
/// console window. The GUI has no console of its own, so Windows would otherwise open
/// (and immediately close) a terminal window for every tool it starts. Use this for
/// every such child process.
pub fn quiet_command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn check_tool(name: &str) -> Result<()> {
    let ok = quiet_command(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!(
            "{name} not found on PATH. Install ffmpeg (e.g. 'winget install ffmpeg') \
             and restart your terminal so PATH refreshes."
        );
    }
    Ok(())
}

pub fn probe_duration(input: &Path) -> Result<f64> {
    let out = quiet_command("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration"])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(input)
        .output()
        .context("failed to run ffprobe")?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .context("could not parse ffprobe duration")
}

pub fn decode_to_wav(input: &Path, wav: &Path) -> Result<()> {
    let out = quiet_command("ffmpeg")
        .args(["-y", "-i"])
        .arg(input)
        .args(["-ac", "1", "-ar", "16000", "-f", "wav"])
        .arg(wav)
        .output()
        .context("failed to run ffmpeg")?;
    if !out.status.success() {
        bail!("ffmpeg decode failed: {}", tail(&out.stderr));
    }
    Ok(())
}

pub fn read_wav(path: &Path) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::open(path).context("could not open decoded wav")?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!("expected 16-bit PCM wav, got {spec:?}");
    }
    let channels = spec.channels as usize;
    let mut mono = Vec::with_capacity(reader.len() as usize / channels);
    let mut frame_sum = 0i32;
    let mut in_frame = 0usize;
    for s in reader.samples::<i16>() {
        frame_sum += s? as i32;
        in_frame += 1;
        if in_frame == channels {
            mono.push((frame_sum / channels as i32) as i16);
            frame_sum = 0;
            in_frame = 0;
        }
    }
    Ok(mono)
}

/// Default output folder: `<user profile>\Music\Splitter` (`tracks` in the current
/// folder if the profile folder is unknown).
pub fn default_output_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join("Music").join("Splitter"))
        .unwrap_or_else(|| PathBuf::from("tracks"))
}

/// Output name prefix: the recording's own name, e.g. `show_track_` -> `show_track_001.mp3`.
pub fn default_prefix(input: &Path) -> String {
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{stem}_track_")
}

/// File extension of the tracks: the recording's own (`.m4a` stays `.m4a`, `.flac` stays
/// `.flac`), so the tracks keep its container, codec and sample rate; ffmpeg picks the
/// container from the extension. `mp3` if the recording has none.
pub fn output_extension(input: &Path) -> String {
    input
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "mp3".to_string())
}

/// How a track is cut out of a recording without touching the audio quality.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Cut {
    /// Copy the compressed packets, seeking on the input. Exact to one frame; used for
    /// mp3, aac/m4a/mp4 and wav.
    Copy,
    /// Copy the packets, but seek on the output side. Ogg can only be entered at a page
    /// start, which (up to a second early) would replay the end of the previous track.
    CopyPacketExact,
    /// Re-encode losslessly. A FLAC copied as it is keeps the whole recording's length
    /// in its header, so players show every track as that long; decoding and encoding
    /// again gives identical audio and a correct header.
    LosslessFlac,
}

impl Cut {
    fn for_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "flac" => Cut::LosslessFlac,
            "ogg" | "oga" | "opus" => Cut::CopyPacketExact,
            _ => Cut::Copy,
        }
    }
}

/// The ffmpeg command line that writes `input` from `start` to `end` seconds to `out`.
fn cut_args(cut: Cut, input: &Path, out: &Path, start: f64, end: f64) -> Vec<OsString> {
    let times = |args: &mut Vec<OsString>| {
        args.extend(["-ss", &start.to_string(), "-to", &end.to_string()].map(OsString::from));
    };
    let mut args = vec![OsString::from("-y")];
    if cut != Cut::CopyPacketExact {
        times(&mut args);
    }
    args.push("-i".into());
    args.push(input.into());
    if cut == Cut::CopyPacketExact {
        times(&mut args);
    }
    // Everything is copied as it is; only the FLAC audio is re-encoded (cover art stays copied).
    args.extend(["-c", "copy"].map(OsString::from));
    if cut == Cut::LosslessFlac {
        args.extend(["-c:a", "flac"].map(OsString::from));
    }
    args.extend(["-avoid_negative_ts", "make_zero"].map(OsString::from));
    args.push(out.into());
    args
}

pub fn extract_segments(
    input: &Path,
    split_points: &[f64],
    duration: f64,
    out_dir: &Path,
    prefix: &str,
    on_progress: &mut dyn FnMut(usize, usize, &Path, f64, f64),
) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let ext = output_extension(input);
    let cut = Cut::for_extension(&ext);
    let mut bounds = vec![0.0];
    bounds.extend_from_slice(split_points);
    bounds.push(duration);
    let digits = std::cmp::max(3, (bounds.len() - 1).to_string().len());
    for i in 0..bounds.len() - 1 {
        let (start, end) = (bounds[i], bounds[i + 1]);
        let out_path = out_dir.join(format!("{prefix}{:0digits$}.{ext}", i + 1));
        on_progress(i + 1, bounds.len() - 1, &out_path, start, end);
        let out = quiet_command("ffmpeg")
            .args(cut_args(cut, input, &out_path, start, end))
            .output()
            .context("failed to run ffmpeg")?;
        if !out.status.success() {
            bail!("ffmpeg failed writing {}: {}", out_path.display(), tail(&out.stderr));
        }
    }
    Ok(())
}

fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let start = s.len().saturating_sub(500);
    let start = (start..=s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_dir_is_music_splitter() {
        let dir = default_output_dir();
        assert!(dir.ends_with(Path::new("Music").join("Splitter")) || dir == Path::new("tracks"), "{dir:?}");
    }

    #[test]
    fn tracks_keep_the_recordings_format() {
        for (input, ext) in [("a.mp3", "mp3"), (r"C:\rec\show.m4a", "m4a"), ("x.FLAC", "FLAC"), ("s.ogg", "ogg"), ("noext", "mp3")] {
            assert_eq!(output_extension(Path::new(input)), ext, "{input}");
        }
    }

    fn args(ext: &str) -> Vec<String> {
        cut_args(Cut::for_extension(ext), Path::new("in"), Path::new("out"), 1.5, 3.0)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn position(args: &[String], what: &str) -> usize {
        args.iter().position(|a| a == what).unwrap_or_else(|| panic!("{what} missing in {args:?}"))
    }

    #[test]
    fn mp3_and_friends_are_copied_with_input_seeking() {
        for ext in ["mp3", "MP3", "m4a", "aac", "mp4", "wav", "anything"] {
            let a = args(ext);
            assert!(position(&a, "-ss") < position(&a, "-i"), "{ext}: {a:?}");
            assert!(!a.contains(&"-c:a".to_string()), "{ext} must not be re-encoded");
        }
        assert_eq!(args("mp3"), ["-y", "-ss", "1.5", "-to", "3", "-i", "in", "-c", "copy", "-avoid_negative_ts", "make_zero", "out"]);
    }

    #[test]
    fn ogg_is_cut_on_exact_packets() {
        for ext in ["ogg", "OGG", "oga", "opus"] {
            let a = args(ext);
            assert!(position(&a, "-i") < position(&a, "-ss"), "{ext}: seek after the input: {a:?}");
            assert!(!a.contains(&"-c:a".to_string()));
        }
    }

    #[test]
    fn flac_is_reencoded_losslessly_and_only_the_audio() {
        for ext in ["flac", "FLAC"] {
            let a = args(ext);
            assert!(position(&a, "-ss") < position(&a, "-i"));
            assert!(position(&a, "-c") < position(&a, "-c:a"), "copy first, then the audio override: {a:?}");
            assert_eq!(a[position(&a, "-c:a") + 1], "flac");
        }
    }

    #[test]
    fn default_prefix_uses_the_input_name() {
        assert_eq!(default_prefix(Path::new(r"C:\rec\Morning Show 2026-09-20.mp3")), "Morning Show 2026-09-20_track_");
        assert_eq!(default_prefix(Path::new("a.b.mp3")), "a.b_track_");
    }
}
