#!/usr/bin/env python3
"""Split a long radio-recording mp3 into per-track files using silence gaps.

Requires ffmpeg and ffprobe on PATH.

Usage:
    python split_radio.py input.mp3
    python split_radio.py input.mp3 --dry-run
    python split_radio.py input.mp3 -o tracks --noise -35 --min-silence 2.0 --min-track-length 60
"""
import argparse
import re
import shutil
import subprocess
import sys
from pathlib import Path

SILENCE_START_RE = re.compile(r"silence_start:\s*([0-9.]+)")
SILENCE_END_RE = re.compile(r"silence_end:\s*([0-9.]+)\s*\|\s*silence_duration:\s*([0-9.]+)")


def probe_duration(input_path: Path) -> float:
    result = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration",
         "-of", "default=noprint_wrappers=1:nokey=1", str(input_path)],
        capture_output=True, text=True, check=True,
    )
    return float(result.stdout.strip())


def detect_silences(input_path: Path, noise_db: float, min_silence: float):
    cmd = [
        "ffmpeg", "-i", str(input_path),
        "-af", f"silencedetect=noise={noise_db}dB:d={min_silence}",
        "-f", "null", "-",
    ]
    result = subprocess.run(cmd, capture_output=True, text=True)
    silences = []
    start = None
    for line in result.stderr.splitlines():
        m = SILENCE_START_RE.search(line)
        if m:
            start = float(m.group(1))
            continue
        m = SILENCE_END_RE.search(line)
        if m and start is not None:
            end = float(m.group(1))
            silences.append((start, end))
            start = None
    return silences


def compute_split_points(silences, duration, min_track_length):
    candidates = [(s + e) / 2 for s, e in silences]
    filtered = []
    last = 0.0
    for p in candidates:
        if p - last >= min_track_length:
            filtered.append(p)
            last = p
    if filtered and duration - filtered[-1] < min_track_length:
        filtered.pop()
    return filtered


def extract_segments(input_path, split_points, duration, out_dir, prefix):
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


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("input", type=Path, help="Input mp3 file (radio stream recording)")
    parser.add_argument("-o", "--output-dir", type=Path, default=Path("tracks"))
    parser.add_argument("--prefix", default="track_")
    parser.add_argument("--noise", type=float, default=-35.0,
                         help="Silence threshold in dB, more negative = stricter (default: -35)")
    parser.add_argument("--min-silence", type=float, default=2.0,
                         help="Minimum silence duration in seconds to count as a gap (default: 2.0)")
    parser.add_argument("--min-track-length", type=float, default=60.0,
                         help="Minimum seconds between splits, rejects false positives (default: 60)")
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

    print(f"Detecting silence (noise={args.noise}dB, min_duration={args.min_silence}s)...")
    silences = detect_silences(args.input, args.noise, args.min_silence)
    print(f"Found {len(silences)} silence periods")

    split_points = compute_split_points(silences, duration, args.min_track_length)
    print(f"Using {len(split_points)} split points -> {len(split_points) + 1} tracks")
    for p in split_points:
        print(f"  split at {p:.1f}s ({p / 60:.1f} min)")

    if args.dry_run:
        return

    extract_segments(args.input, split_points, duration, args.output_dir, args.prefix)
    print(f"Done. {len(split_points) + 1} tracks written to {args.output_dir}/")


if __name__ == "__main__":
    main()
