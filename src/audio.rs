use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn check_tool(name: &str) -> Result<()> {
    let ok = Command::new(name)
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
    let out = Command::new("ffprobe")
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
    let out = Command::new("ffmpeg")
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

pub fn extract_segments(
    input: &Path,
    split_points: &[f64],
    duration: f64,
    out_dir: &Path,
    prefix: &str,
    on_progress: &mut dyn FnMut(usize, usize, &Path, f64, f64),
) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let mut bounds = vec![0.0];
    bounds.extend_from_slice(split_points);
    bounds.push(duration);
    let digits = std::cmp::max(3, (bounds.len() - 1).to_string().len());
    for i in 0..bounds.len() - 1 {
        let (start, end) = (bounds[i], bounds[i + 1]);
        let out_path = out_dir.join(format!("{prefix}{:0digits$}.mp3", i + 1));
        on_progress(i + 1, bounds.len() - 1, &out_path, start, end);
        let out = Command::new("ffmpeg")
            .arg("-y")
            .args(["-ss", &start.to_string(), "-to", &end.to_string()])
            .arg("-i")
            .arg(input)
            .args(["-c", "copy", "-avoid_negative_ts", "make_zero"])
            .arg(&out_path)
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
    fn default_prefix_uses_the_input_name() {
        assert_eq!(default_prefix(Path::new(r"C:\rec\Morning Show 2026-09-20.mp3")), "Morning Show 2026-09-20_track_");
        assert_eq!(default_prefix(Path::new("a.b.mp3")), "a.b_track_");
    }
}
