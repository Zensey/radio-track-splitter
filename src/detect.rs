/// Cosine distance between the mean embedding just before and just after each
/// frame. High value = abrupt change in audio character.
pub fn novelty_curve(emb: &[f32], n: usize, dim: usize, window: usize) -> Vec<f64> {
    let mut mean = vec![0f64; dim];
    for row in emb.chunks_exact(dim) {
        for (m, &v) in mean.iter_mut().zip(row) {
            *m += v as f64;
        }
    }
    mean.iter_mut().for_each(|m| *m /= n as f64);

    let mut norm = vec![0f64; n * dim];
    for (i, row) in emb.chunks_exact(dim).enumerate() {
        let out = &mut norm[i * dim..(i + 1) * dim];
        for ((o, &v), &m) in out.iter_mut().zip(row).zip(&mean) {
            *o = v as f64 - m;
        }
        let len = out.iter().map(|x| x * x).sum::<f64>().sqrt() + 1e-9;
        out.iter_mut().for_each(|x| *x /= len);
    }

    let unit_mean = |from: usize, to: usize| -> Vec<f64> {
        let mut acc = vec![0f64; dim];
        for row in norm[from * dim..to * dim].chunks_exact(dim) {
            for (a, &v) in acc.iter_mut().zip(row) {
                *a += v;
            }
        }
        let count = (to - from) as f64;
        acc.iter_mut().for_each(|a| *a /= count);
        let len = acc.iter().map(|x| x * x).sum::<f64>().sqrt() + 1e-9;
        acc.iter_mut().for_each(|a| *a /= len);
        acc
    };

    let mut novelty = vec![0f64; n];
    if n > 2 * window {
        for i in window..n - window {
            let before = unit_mean(i - window, i);
            let after = unit_mean(i, i + window);
            let dot: f64 = before.iter().zip(&after).map(|(a, b)| a * b).sum();
            novelty[i] = 1.0 - dot;
        }
    }
    novelty
}

/// z-scored novelty at each scale (seconds), combined with min() so a frame
/// scores high only if it stands out at every scale.
pub fn consensus_novelty(emb: &[f32], n: usize, dim: usize, hop: f64, scales: &[f64]) -> Vec<f64> {
    let mut combined = vec![f64::INFINITY; n];
    for &scale in scales {
        let window = ((scale / hop).round_ties_even() as usize).max(1);
        let novelty = novelty_curve(emb, n, dim, window);
        let mean = novelty.iter().sum::<f64>() / n as f64;
        let var = novelty.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        let std = var.sqrt() + 1e-9;
        for (c, x) in combined.iter_mut().zip(&novelty) {
            *c = c.min((x - mean) / std);
        }
    }
    combined
}

/// Same semantics as scipy.signal.find_peaks(x, height=min_height, distance=distance).
pub fn find_peaks(x: &[f64], min_height: f64, distance: usize) -> Vec<usize> {
    let n = x.len();
    let mut peaks = Vec::new();
    if n >= 3 {
        let (mut i, i_max) = (1, n - 1);
        while i < i_max {
            if x[i - 1] < x[i] {
                let mut ahead = i + 1;
                while ahead < i_max && x[ahead] == x[i] {
                    ahead += 1;
                }
                if x[ahead] < x[i] {
                    peaks.push((i + ahead - 1) / 2);
                    i = ahead;
                }
            }
            i += 1;
        }
    }
    peaks.retain(|&p| x[p] >= min_height);

    let mut keep = vec![true; peaks.len()];
    let mut order: Vec<usize> = (0..peaks.len()).collect();
    order.sort_by(|&a, &b| x[peaks[a]].partial_cmp(&x[peaks[b]]).unwrap());
    for &j in order.iter().rev() {
        if !keep[j] {
            continue;
        }
        let mut k = j;
        while k > 0 && peaks[j] - peaks[k - 1] < distance {
            keep[k - 1] = false;
            k -= 1;
        }
        let mut k = j + 1;
        while k < peaks.len() && peaks[k] - peaks[j] < distance {
            keep[k] = false;
            k += 1;
        }
    }
    peaks.into_iter().zip(keep).filter(|&(_, k)| k).map(|(p, _)| p).collect()
}

/// Search +-window seconds around t for the quietest 1024-sample frame
/// (hop 256, zero-padded at the edges like librosa's rms) and return its centre.
pub fn snap_to_energy_minimum(samples: &[i16], sr: usize, t: f64, window: f64) -> f64 {
    let center = (t * sr as f64) as usize;
    let half = (window * sr as f64) as usize;
    let start = center.saturating_sub(half);
    let end = samples.len().min(center + half);
    if end <= start || end - start < sr / 10 {
        return t;
    }
    let seg = &samples[start..end];
    const FRAME: usize = 1024;
    const HOP: usize = 256;

    let mut cum = Vec::with_capacity(seg.len() + 1);
    cum.push(0f64);
    for &s in seg {
        let v = s as f64 / 32768.0;
        cum.push(cum.last().unwrap() + v * v);
    }
    let n_frames = 1 + seg.len() / HOP;
    let mut best = (f64::INFINITY, 0usize);
    for k in 0..n_frames {
        let lo = (k * HOP).saturating_sub(FRAME / 2);
        let hi = (k * HOP + FRAME / 2).min(seg.len());
        let energy = cum[hi] - cum[lo];
        if energy < best.0 {
            best = (energy, k);
        }
    }
    (start + best.1 * HOP) as f64 / sr as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_match_scipy() {
        // Expected values generated with scipy.signal.find_peaks.
        let x = [
            0.0, 0.3, 0.1, 0.9, 0.9, 0.2, 0.5, 0.4, 1.2, 0.3, 0.3, 0.8, 0.1, 0.6, 2.0, 0.6, 0.7,
            0.1, 0.0,
        ];
        assert_eq!(find_peaks(&x, 0.0, 1), vec![1, 3, 6, 8, 11, 14, 16]);
    }

    #[test]
    fn distance_keeps_highest_first() {
        let mut x = vec![0.0; 40];
        x[5] = 1.0;
        x[10] = 3.0;
        x[14] = 2.0;
        x[30] = 1.5;
        assert_eq!(find_peaks(&x, 0.5, 8), vec![10, 30]);
        assert_eq!(find_peaks(&x, 1.6, 1), vec![10, 14]);
    }

    #[test]
    fn snap_finds_quiet_gap() {
        let sr = 16000;
        let mut s = vec![16384i16; sr * 10];
        for v in &mut s[sr * 5..sr * 5 + 4000] {
            *v = 0;
        }
        let t = snap_to_energy_minimum(&s, sr, 5.5, 3.0);
        assert!((t - 5.12).abs() < 0.2, "got {t}");
    }
}
