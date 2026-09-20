//! VGGish (AudioSet-pretrained CNN) embeddings: log-mel front end and network,
//! numerically matching torchvggish with postprocess=False.

use anyhow::{bail, Context, Result};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{conv2d, linear, Conv2d, Conv2dConfig, Linear, VarBuilder};
use rustfft::{num_complex::Complex, FftPlanner};
use std::path::{Path, PathBuf};

use crate::cache::Embeddings;

pub const HOP_SECONDS: f64 = 0.96;
const SAMPLE_RATE: f64 = 16000.0;
const WINDOW: usize = 400;
const HOP: usize = 160;
const FFT_LEN: usize = 512;
const BINS: usize = FFT_LEN / 2 + 1;
const MELS: usize = 64;
const EXAMPLE_FRAMES: usize = 96;
const EMBEDDING_DIM: usize = 128;
const LOG_OFFSET: f64 = 0.01;

const WEIGHTS_NAME: &str = "vggish-10086976.pth";
const WEIGHTS_URL: &str =
    "https://github.com/harritaylor/torchvggish/releases/download/v0.1/vggish-10086976.pth";

fn hz_to_mel(hz: f64) -> f64 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

/// BINS x MELS row-major triangular filterbank (HTK mel scale, 125-7500 Hz).
fn mel_matrix() -> Vec<f64> {
    let nyquist = SAMPLE_RATE / 2.0;
    let bins_mel: Vec<f64> = (0..BINS)
        .map(|i| hz_to_mel(nyquist * i as f64 / (BINS - 1) as f64))
        .collect();
    let (lo, hi) = (hz_to_mel(125.0), hz_to_mel(7500.0));
    let edges: Vec<f64> = (0..MELS + 2)
        .map(|i| lo + (hi - lo) * i as f64 / (MELS + 1) as f64)
        .collect();
    let mut w = vec![0.0; BINS * MELS];
    for m in 0..MELS {
        let (l, c, u) = (edges[m], edges[m + 1], edges[m + 2]);
        for k in 1..BINS {
            let lower = (bins_mel[k] - l) / (c - l);
            let upper = (u - bins_mel[k]) / (u - c);
            w[k * MELS + m] = lower.min(upper).max(0.0);
        }
    }
    w
}

/// Returns (features, number of examples); each example is EXAMPLE_FRAMES x MELS.
pub fn log_mel_examples(samples: &[i16]) -> (Vec<f32>, usize) {
    if samples.len() < WINDOW {
        return (Vec::new(), 0);
    }
    let num_frames = 1 + (samples.len() - WINDOW) / HOP;
    let hann: Vec<f64> = (0..WINDOW)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI / WINDOW as f64 * i as f64).cos())
        .collect();
    let mel = mel_matrix();
    let ranges: Vec<(usize, usize)> = (0..MELS)
        .map(|m| {
            let nz: Vec<usize> = (0..BINS).filter(|&k| mel[k * MELS + m] > 0.0).collect();
            (*nz.first().unwrap_or(&0), nz.last().map_or(0, |l| l + 1))
        })
        .collect();

    let fft = FftPlanner::<f64>::new().plan_fft_forward(FFT_LEN);
    let mut buf = vec![Complex::new(0.0, 0.0); FFT_LEN];
    let mut mag = vec![0.0f64; BINS];
    let mut log_mel = vec![0f32; num_frames * MELS];
    for f in 0..num_frames {
        let off = f * HOP;
        for i in 0..WINDOW {
            buf[i] = Complex::new(samples[off + i] as f64 / 32768.0 * hann[i], 0.0);
        }
        buf[WINDOW..].fill(Complex::new(0.0, 0.0));
        fft.process(&mut buf);
        for k in 0..BINS {
            mag[k] = buf[k].norm();
        }
        for m in 0..MELS {
            let (a, b) = ranges[m];
            let s: f64 = (a..b).map(|k| mag[k] * mel[k * MELS + m]).sum();
            log_mel[f * MELS + m] = (s + LOG_OFFSET).ln() as f32;
        }
    }
    let examples = if num_frames >= EXAMPLE_FRAMES {
        1 + (num_frames - EXAMPLE_FRAMES) / EXAMPLE_FRAMES
    } else {
        0
    };
    log_mel.truncate(examples * EXAMPLE_FRAMES * MELS);
    (log_mel, examples)
}

struct Vggish {
    convs: Vec<Conv2d>,
    fcs: Vec<Linear>,
}

impl Vggish {
    fn load(weights: &Path, dev: &Device) -> Result<Self> {
        let vb = if crate::torch_legacy::is_legacy(weights)? {
            let tensors = crate::torch_legacy::load_state_dict(weights, dev)
                .with_context(|| format!("could not read weights {}", weights.display()))?;
            VarBuilder::from_tensors(tensors, DType::F32, dev)
        } else {
            VarBuilder::from_pth(weights, DType::F32, dev)
                .with_context(|| format!("could not read weights {}", weights.display()))?
        };
        let cfg = Conv2dConfig { padding: 1, ..Default::default() };
        let features = vb.pp("features");
        let mut convs = Vec::new();
        for (cin, cout, idx) in [(1, 64, "0"), (64, 128, "3"), (128, 256, "6"),
                                 (256, 256, "8"), (256, 512, "11"), (512, 512, "13")] {
            convs.push(conv2d(cin, cout, 3, cfg, features.pp(idx))?);
        }
        let embeddings = vb.pp("embeddings");
        let mut fcs = Vec::new();
        for (din, dout, idx) in [(512 * 4 * 6, 4096, "0"), (4096, 4096, "2"),
                                 (4096, EMBEDDING_DIM, "4")] {
            fcs.push(linear(din, dout, embeddings.pp(idx))?);
        }
        Ok(Self { convs, fcs })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let mut x = x.clone();
        for (i, conv) in self.convs.iter().enumerate() {
            x = conv.forward(&x)?.relu()?;
            if matches!(i, 0 | 1 | 3 | 5) {
                x = x.max_pool2d(2)?;
            }
        }
        // torchvggish flattens as (H, W, C) to stay compatible with the TF model.
        let mut x = x.permute((0, 2, 3, 1))?.contiguous()?.flatten_from(1)?;
        for fc in &self.fcs {
            x = fc.forward(&x)?.relu()?;
        }
        Ok(x)
    }
}

pub fn ensure_weights(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            bail!("weights file not found: {}", p.display());
        }
        return Ok(p);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    let torch_cache = home.as_ref().map(|h| h.join(".cache/torch/hub/checkpoints").join(WEIGHTS_NAME));
    let cache_base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cache")))
        .context("cannot determine a cache directory")?;
    let own_dir = cache_base.join("radio-track-splitter");
    let own = own_dir.join(WEIGHTS_NAME);
    // Folder used before the project was renamed.
    let legacy = cache_base.join("split_radio").join(WEIGHTS_NAME);
    // The installer puts the weights next to the executable.
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(WEIGHTS_NAME)));
    let candidates = beside_exe.iter().chain(torch_cache.iter()).chain([&own, &legacy]);
    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    println!("Downloading VGGish weights to {} ...", own.display());
    std::fs::create_dir_all(&own_dir)?;
    let partial = own.with_extension("partial");
    let status = crate::audio::quiet_command("curl")
        .args(["-L", "--fail", "-o"])
        .arg(&partial)
        .arg(WEIGHTS_URL)
        .status()
        .context("failed to run curl (download the file manually and pass --weights)")?;
    if !status.success() {
        bail!("download failed; fetch {WEIGHTS_URL} manually and pass --weights <file>");
    }
    std::fs::rename(&partial, &own)?;
    Ok(own)
}

pub fn compute_embeddings(
    samples: &[i16],
    weights: &Path,
    on_progress: &dyn Fn(usize, usize),
) -> Result<Embeddings> {
    let (features, examples) = log_mel_examples(samples);
    if examples == 0 {
        bail!("No audio examples extracted - file too short or empty?");
    }
    let dev = Device::Cpu;
    let model = Vggish::load(weights, &dev)?;

    const BATCH: usize = 32;
    let example_len = EXAMPLE_FRAMES * MELS;
    let mut data = Vec::with_capacity(examples * EMBEDDING_DIM);
    for start in (0..examples).step_by(BATCH) {
        let count = BATCH.min(examples - start);
        let slice = &features[start * example_len..(start + count) * example_len];
        let x = Tensor::from_slice(slice, (count, 1, EXAMPLE_FRAMES, MELS), &dev)?;
        data.extend(model.forward(&x)?.flatten_all()?.to_vec1::<f32>()?);
        on_progress(start + count, examples);
    }
    Ok(Embeddings { data, n: examples, dim: EMBEDDING_DIM, hop: HOP_SECONDS })
}
