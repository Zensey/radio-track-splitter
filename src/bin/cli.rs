//! Split a long radio recording into per-track files using VGGish
//! embeddings to detect track changes (Rust port of split_radio_nn.py).
//!
//! Novelty is computed at several time scales and combined with min(), so a
//! boundary must stand out at every scale; each boundary is then snapped to the
//! nearest quiet moment and the tracks are cut losslessly with ffmpeg, in the
//! recording's own format (no re-encoding, no resampling).
//! For manual editing of the cut points use the `radio-track-splitter` GUI.

use anyhow::{bail, Context, Result};
use clap::Parser;
use radio_track_splitter::{audio, pipeline};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "radio-track-splitter-cli", version, about)]
struct Args {
    /// Input recording (mp3, m4a, flac, ogg, wav, ...); the tracks keep its format
    input: PathBuf,
    /// Output folder (default: <user profile>\Music\Splitter)
    #[arg(short, long)]
    output_dir: Option<PathBuf>,
    /// Output file name prefix (default: "<input name>_track_")
    #[arg(long)]
    prefix: Option<String>,
    /// Comma-separated novelty window sizes in seconds; a boundary must stand out at all of them
    #[arg(long, default_value = "8,15,25")]
    scales: String,
    /// Minimum combined z-score for a boundary; higher = fewer, more confident splits
    #[arg(long, default_value_t = 1.5)]
    sensitivity: f64,
    /// Minimum seconds between splits
    #[arg(long, default_value_t = 120.0)]
    min_track_length: f64,
    /// Seconds to search around each boundary for a quiet moment to cut at
    #[arg(long, default_value_t = 3.0)]
    snap_window: f64,
    /// Only print detected split points, don't extract
    #[arg(long)]
    dry_run: bool,
    /// Path to vggish-10086976.pth (default: next to the exe, else torch hub cache, else downloaded)
    #[arg(long)]
    weights: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    audio::check_tool("ffmpeg")?;
    audio::check_tool("ffprobe")?;
    if !args.input.exists() {
        bail!("Input file not found: {}", args.input.display());
    }
    let scales: Vec<f64> = args
        .scales
        .split(',')
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .context("--scales must be comma-separated numbers")?;

    println!("Probing duration of {}...", args.input.display());
    let duration = audio::probe_duration(&args.input)?;
    println!("Duration: {duration:.1}s ({:.1} min)", duration / 60.0);

    let tmp = tempfile::tempdir()?;
    let wav = tmp.path().join("audio.wav");
    println!("Decoding to 16kHz mono WAV for analysis...");
    audio::decode_to_wav(&args.input, &wav)?;
    let samples = audio::read_wav(&wav)?;

    println!("Loading VGGish embeddings (computed on first run)...");
    let (emb, cached) = pipeline::load_embeddings(
        &args.input,
        &samples,
        args.weights.clone(),
        &|done, total| {
            eprint!("\r  embeddings {done}/{total}");
            if done == total {
                eprintln!();
            }
            let _ = std::io::stderr().flush();
        },
    )?;
    if cached {
        println!("Using cached embeddings.");
    }
    println!("Got {} embeddings at {:.2}s resolution", emb.n, emb.hop);

    println!("Computing consensus novelty over scales {scales:?}s...");
    let candidates = pipeline::find_candidates(
        &emb,
        &samples,
        &scales,
        args.sensitivity,
        args.min_track_length,
        args.snap_window,
    );
    println!(
        "{} boundaries with score >= {} and >= {:.0}s apart",
        candidates.len(),
        args.sensitivity,
        args.min_track_length
    );
    for c in &candidates {
        println!("  candidate at {:7.1}s  score {:.2}", c.raw, c.score);
    }

    let mut split_points: Vec<f64> = candidates.iter().map(|c| c.snapped).collect();
    split_points.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!("Using {} split points -> {} tracks", split_points.len(), split_points.len() + 1);
    let mut bounds = vec![0.0];
    bounds.extend_from_slice(&split_points);
    bounds.push(duration);
    for (i, w) in bounds.windows(2).enumerate() {
        println!("  track {:2}: {:9.3}s - {:9.3}s  ({:6.1}s)", i + 1, w[0], w[1], w[1] - w[0]);
    }

    if args.dry_run {
        return Ok(());
    }
    let prefix = args.prefix.clone().unwrap_or_else(|| audio::default_prefix(&args.input));
    let output_dir = args.output_dir.clone().unwrap_or_else(audio::default_output_dir);
    audio::extract_segments(
        &args.input,
        &split_points,
        duration,
        &output_dir,
        &prefix,
        &mut |_, _, path, start, end| {
            println!(
                "Writing {}  [{start:.1}s - {end:.1}s]  ({:.1}s)",
                path.file_name().unwrap().to_string_lossy(),
                end - start
            );
        },
    )?;
    println!("Done. {} tracks written to {}", split_points.len() + 1, output_dir.display());
    Ok(())
}
