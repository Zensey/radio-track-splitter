#!/usr/bin/env python3
"""Split a long radio-recording mp3 into per-track files using a neural
audio embedding (VGGish) to detect track-change points, instead of relying
only on a silence-level threshold.

Why: DJ crossfades, beatmatched transitions, and jingles-over-music don't
produce a clean silence gap, so pure silencedetect (see split_radio.py)
misses them. VGGish embeddings capture the "character" of the audio
(timbre / instrumentation / voice-vs-music), so a track change shows up as
a jump in embedding-space even when the volume never drops.

Pipeline:
  1. ffmpeg decodes the mp3 to 16kHz mono WAV (VGGish's expected input).
  2. VGGish (pretrained on AudioSet) produces one 128-dim embedding per
     ~0.96s frame of audio.
  3. A novelty curve is computed at several time scales (default 8s, 15s,
     25s): at each frame, compare the mean embedding of a window just
     before it vs. just after it.
  4. The curves are z-scored and combined with min(), so a boundary must
     stand out at EVERY scale. Real track changes do; section changes
     inside a song (verse -> chorus, breakdown) usually only show up at
     the short scale, which is what made tracks split into 2-3 parts.
     Peaks above --sensitivity (with --min-track-length spacing) become
     boundaries.
  5. Each boundary is snapped to the nearest local RMS-energy minimum
     within a small search window, so the cut lands in a quiet moment
     instead of mid-note.
  6. ffmpeg extracts each segment losslessly with -c copy.

Requires: ffmpeg on PATH, and (see requirements.txt):
    PYTHONUTF8=1 pip install -r requirements.txt

First run downloads the VGGish pretrained weights from GitHub. Embeddings are
cached in <output-dir>/.vggish_cache.npz, so re-running with different
--sensitivity / --min-track-length / --scales is fast.

Usage:
    python split_radio_nn.py input.mp3 --dry-run
    python split_radio_nn.py input.mp3 -o tracks --sensitivity 1.5 --min-track-length 120

Tuning: still too many splits -> raise --sensitivity (e.g. 2.0) or
--min-track-length. Missing real splits -> lower them (e.g. 1.0 / 90).
"""
import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np


def decode_to_wav(input_path: Path, wav_path: Path, sr: int = 16000):
    cmd = [
        "ffmpeg", "-y", "-i", str(input_path),
        "-ac", "1", "-ar", str(sr), "-f", "wav", str(wav_path),
    ]
    subprocess.run(cmd, capture_output=True, text=True, check=True)


def probe_duration(input_path: Path) -> float:
    result = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration",
         "-of", "default=noprint_wrappers=1:nokey=1", str(input_path)],
        capture_output=True, text=True, check=True,
    )
    return float(result.stdout.strip())


def compute_embeddings(wav_path: Path, batch_size: int = 256):
    """Returns (embeddings: np.ndarray [N, 128], hop_seconds: float)."""
    import torch
    from torchvggish import vggish, vggish_input, vggish_params

    examples = vggish_input.wavfile_to_examples(str(wav_path))
    if examples.shape[0] == 0:
        raise RuntimeError("No audio examples extracted — file too short or empty?")

    # postprocess=False: raw embeddings. The default PCA+8-bit quantization gives
    # values in 0..255 (cosine ~1 for everything) and squeezes single-example batches.
    model = vggish(postprocess=False)
    model.eval()

    embeddings = []
    with torch.no_grad():
        for i in range(0, examples.shape[0], batch_size):
            chunk = examples[i:i + batch_size]
            emb = model.forward(chunk)
            embeddings.append(emb.cpu().numpy())
    embeddings = np.concatenate(embeddings, axis=0)

    return embeddings, vggish_params.EXAMPLE_HOP_SECONDS


def novelty_curve(embeddings: np.ndarray, window_frames: int) -> np.ndarray:
    """For each frame i, cosine distance between mean(embeddings[i-w:i])
    and mean(embeddings[i:i+w]). High value = abrupt change in audio character."""
    n = embeddings.shape[0]
    centered = embeddings - embeddings.mean(axis=0)
    norm = centered / (np.linalg.norm(centered, axis=1, keepdims=True) + 1e-9)
    novelty = np.zeros(n)
    for i in range(window_frames, n - window_frames):
        before = norm[i - window_frames:i].mean(axis=0)
        after = norm[i:i + window_frames].mean(axis=0)
        before /= (np.linalg.norm(before) + 1e-9)
        after /= (np.linalg.norm(after) + 1e-9)
        novelty[i] = 1.0 - float(np.dot(before, after))
    return novelty


def consensus_novelty(embeddings: np.ndarray, hop_seconds: float, scales) -> np.ndarray:
    """z-scored novelty at each scale (seconds), combined with min() so a
    frame scores high only if it stands out at every scale."""
    curves = []
    for scale in scales:
        novelty = novelty_curve(embeddings, max(1, int(round(scale / hop_seconds))))
        curves.append((novelty - novelty.mean()) / (novelty.std() + 1e-9))
    return np.min(curves, axis=0)


def pick_peaks(score: np.ndarray, hop_seconds: float, min_track_length: float, sensitivity: float):
    from scipy.signal import find_peaks

    min_distance_frames = max(1, int(min_track_length / hop_seconds))
    peaks, _ = find_peaks(score, height=sensitivity, distance=min_distance_frames)
    return peaks


def load_or_compute_embeddings(input_path: Path, wav_path: Path, cache_path: Path):
    stat = input_path.stat()
    key = np.array([stat.st_size, stat.st_mtime_ns], dtype=np.int64)
    if cache_path.exists():
        cached = np.load(cache_path)
        if np.array_equal(cached["key"], key):
            print(f"Using cached embeddings from {cache_path}")
            return cached["embeddings"], float(cached["hop"])

    print("Computing VGGish embeddings (first run downloads pretrained weights)...")
    embeddings, hop_seconds = compute_embeddings(wav_path)
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    np.savez(cache_path, key=key, embeddings=embeddings, hop=hop_seconds)
    return embeddings, hop_seconds


def snap_to_energy_minimum(samples: np.ndarray, sr: int, t: float, search_window: float) -> float:
    import librosa

    center = int(t * sr)
    half = int(search_window * sr)
    start = max(0, center - half)
    end = min(len(samples), center + half)
    if end - start < sr // 10:
        return t

    segment = samples[start:end]
    frame_length, hop_length = 1024, 256
    rms = librosa.feature.rms(y=segment, frame_length=frame_length, hop_length=hop_length)[0]
    if len(rms) == 0:
        return t
    min_idx = int(np.argmin(rms))
    min_sample = start + min_idx * hop_length
    return min_sample / sr


def extract_segments(input_path, split_points, duration, out_dir, prefix, on_progress=None):
    out_dir.mkdir(parents=True, exist_ok=True)
    bounds = [0.0] + split_points + [duration]
    digits = max(3, len(str(len(bounds) - 1)))
    for i in range(len(bounds) - 1):
        seg_start, seg_end = bounds[i], bounds[i + 1]
        out_path = out_dir / f"{prefix}{i + 1:0{digits}d}.mp3"
        cmd = [
            "ffmpeg", "-y",
            "-ss", f"{seg_start}", "-to", f"{seg_end}",
            "-i", str(input_path),
            "-c", "copy", "-avoid_negative_ts", "make_zero",
            str(out_path),
        ]
        print(f"Writing {out_path.name}  [{seg_start:.1f}s - {seg_end:.1f}s]  ({(seg_end - seg_start):.1f}s)")
        subprocess.run(cmd, capture_output=True, text=True, check=True)
        if on_progress:
            on_progress(i + 1, len(bounds) - 1, out_path)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("input", type=Path, help="Input mp3 file (radio stream recording)")
    parser.add_argument("-o", "--output-dir", type=Path, default=Path("tracks"))
    parser.add_argument("--prefix", default="track_")
    parser.add_argument("--scales", default="8,15,25",
                         help="Comma-separated novelty window sizes in seconds; a boundary must "
                              "stand out at all of them (default: 8,15,25)")
    parser.add_argument("--sensitivity", type=float, default=1.5,
                         help="Minimum combined z-score for a boundary. "
                              "Higher = fewer, more confident splits (default: 1.5)")
    parser.add_argument("--min-track-length", type=float, default=120.0,
                         help="Minimum seconds between splits (default: 120)")
    parser.add_argument("--snap-window", type=float, default=3.0,
                         help="Seconds to search around each detected boundary for a local "
                              "energy minimum to snap the cut to (default: 3.0)")
    parser.add_argument("--dry-run", action="store_true", help="Only print detected split points, don't extract")
    args = parser.parse_args()

    for tool in ("ffmpeg", "ffprobe"):
        if shutil.which(tool) is None:
            sys.exit(f"{tool} not found on PATH. Install ffmpeg (e.g. 'winget install ffmpeg') "
                     "and restart your terminal so PATH refreshes.")

    if not args.input.exists():
        sys.exit(f"Input file not found: {args.input}")

    print(f"Probing duration of {args.input}...")
    duration = probe_duration(args.input)
    print(f"Duration: {duration:.1f}s ({duration / 60:.1f} min)")

    with tempfile.TemporaryDirectory() as tmp:
        wav_path = Path(tmp) / "audio.wav"
        print("Decoding to 16kHz mono WAV for analysis...")
        decode_to_wav(args.input, wav_path)

        embeddings, hop_seconds = load_or_compute_embeddings(
            args.input, wav_path, args.output_dir / ".vggish_cache.npz")
        print(f"Got {embeddings.shape[0]} embeddings at {hop_seconds:.2f}s resolution")

        scales = [float(x) for x in args.scales.split(",")]
        print(f"Computing consensus novelty over scales {scales}s...")
        score = consensus_novelty(embeddings, hop_seconds, scales)

        peaks = pick_peaks(score, hop_seconds, args.min_track_length, args.sensitivity)
        print(f"{len(peaks)} boundaries with score >= {args.sensitivity} "
              f"and >= {args.min_track_length:.0f}s apart")
        for p in peaks:
            print(f"  candidate at {p * hop_seconds:7.1f}s  score {score[p]:.2f}")

        boundary_times = [p * hop_seconds for p in peaks]

        print("Loading full-resolution audio for energy snapping...")
        import soundfile as sf
        samples, sr = sf.read(str(wav_path), dtype="float32")
        if samples.ndim > 1:
            samples = samples.mean(axis=1)

        split_points = []
        for t in boundary_times:
            snapped = snap_to_energy_minimum(samples, sr, t, args.snap_window)
            split_points.append(snapped)
        split_points.sort()

    print(f"Using {len(split_points)} split points -> {len(split_points) + 1} tracks")
    bounds = [0.0] + split_points + [duration]
    for i in range(len(bounds) - 1):
        print(f"  track {i + 1:2d}: {bounds[i]:7.1f}s - {bounds[i + 1]:7.1f}s  ({bounds[i + 1] - bounds[i]:5.1f}s)")

    if args.dry_run:
        return

    extract_segments(args.input, split_points, duration, args.output_dir, args.prefix)
    print(f"Done. {len(split_points) + 1} tracks written to {args.output_dir}/")


if __name__ == "__main__":
    main()
