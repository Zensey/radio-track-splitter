//! Detection steps shared by the CLI and the GUI.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::cache::{self, Embeddings};
use crate::{detect, vggish};

pub const DEFAULT_SCALES: [f64; 3] = [8.0, 15.0, 25.0];

pub struct Candidate {
    pub raw: f64,
    pub score: f64,
    pub snapped: f64,
}

/// Returns the embeddings and whether they came from the cache.
pub fn load_embeddings(
    input: &Path,
    samples: &[i16],
    weights: Option<PathBuf>,
    on_progress: &dyn Fn(usize, usize),
) -> Result<(Embeddings, bool)> {
    let key = cache::source_key(input)?;
    if let Some(e) = cache::load_for(input, key) {
        return Ok((e, true));
    }
    let weights = vggish::ensure_weights(weights)?;
    let e = vggish::compute_embeddings(samples, &weights, on_progress)?;
    // Failing to cache must not throw away minutes of computed embeddings.
    if let Err(err) = cache::save_for(input, key, &e) {
        eprintln!("warning: could not cache embeddings: {err:#}");
    }
    Ok((e, false))
}

pub fn find_candidates(
    emb: &Embeddings,
    samples: &[i16],
    scales: &[f64],
    sensitivity: f64,
    min_track_length: f64,
    snap_window: f64,
) -> Vec<Candidate> {
    let score = detect::consensus_novelty(&emb.data, emb.n, emb.dim, emb.hop, scales);
    let min_distance = ((min_track_length / emb.hop) as usize).max(1);
    detect::find_peaks(&score, sensitivity, min_distance)
        .into_iter()
        .map(|p| {
            let raw = p as f64 * emb.hop;
            Candidate {
                raw,
                score: score[p],
                snapped: detect::snap_to_energy_minimum(samples, 16000, raw, snap_window),
            }
        })
        .collect()
}
