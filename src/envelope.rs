//! Min/max waveform envelope at 500 bins per second, for fast drawing at any zoom.

const SAMPLE_RATE: usize = 16000;
pub const RATE: usize = 500;
const BIN: usize = SAMPLE_RATE / RATE;

pub struct Envelope {
    lo: Vec<f32>,
    hi: Vec<f32>,
    /// Amplitude that should fill the display height (99.9th percentile of the peaks).
    pub gain_ref: f32,
}

impl Envelope {
    pub fn from_samples(samples: &[i16]) -> Self {
        let mut lo = Vec::with_capacity(samples.len() / BIN + 1);
        let mut hi = Vec::with_capacity(samples.len() / BIN + 1);
        for chunk in samples.chunks(BIN) {
            let (mut mn, mut mx) = (i16::MAX, i16::MIN);
            for &s in chunk {
                mn = mn.min(s);
                mx = mx.max(s);
            }
            lo.push(mn as f32 / 32768.0);
            hi.push(mx as f32 / 32768.0);
        }
        let mut peaks: Vec<f32> = lo.iter().zip(&hi).map(|(l, h)| l.abs().max(h.abs())).collect();
        let gain_ref = if peaks.is_empty() {
            1.0
        } else {
            let idx = ((peaks.len() - 1) as f64 * 0.999) as usize;
            peaks.select_nth_unstable_by(idx, |a, b| a.partial_cmp(b).unwrap());
            peaks[idx].max(1e-3)
        };
        Self { lo, hi, gain_ref }
    }

    /// (min, max) for each of `width` columns spanning t0..t1 seconds.
    pub fn columns(&self, t0: f64, t1: f64, width: usize) -> Vec<(f32, f32)> {
        let n = self.lo.len();
        if n == 0 || width == 0 {
            return Vec::new();
        }
        let edge = |k: usize| {
            let t = t0 + (t1 - t0) * k as f64 / width as f64;
            ((t * RATE as f64).max(0.0) as usize).min(n - 1)
        };
        (0..width)
            .map(|k| {
                let start = edge(k);
                let end = edge(k + 1).max(start + 1).min(n);
                let mut mn = f32::INFINITY;
                let mut mx = f32::NEG_INFINITY;
                for i in start..end {
                    mn = mn.min(self.lo[i]);
                    mx = mx.max(self.hi[i]);
                }
                (mn, mx)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_cover_min_and_max() {
        let mut s = vec![0i16; SAMPLE_RATE * 2];
        s[SAMPLE_RATE] = 16384;
        s[SAMPLE_RATE + 100] = -8192;
        let env = Envelope::from_samples(&s);
        let cols = env.columns(0.0, 2.0, 2);
        assert_eq!(cols[0], (0.0, 0.0));
        assert_eq!(cols[1], (-0.25, 0.5));
    }

    #[test]
    fn zoomed_in_columns_repeat_bins() {
        let s: Vec<i16> = (0..SAMPLE_RATE).map(|i| ((i as f32 / 100.0).sin() * 20000.0) as i16).collect();
        let env = Envelope::from_samples(&s);
        assert_eq!(env.columns(0.0, 0.01, 50).len(), 50);
    }
}
