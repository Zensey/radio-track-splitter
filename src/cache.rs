use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAGIC: &[u8; 8] = b"VGGCACH1";

/// Candidate cache files for `input`, best first: next to the executable, then the
/// per-user data folder (in case the install folder is read-only). The name is
/// derived from the input's full path, so each recording has its own file.
pub fn locations(input: &Path) -> Vec<PathBuf> {
    let canonical = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());
    // FNV-1a: stable across Rust versions, unlike std's DefaultHasher.
    let hash = canonical
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let name = format!("vggish_cache_{hash:016x}.bin");

    let beside_exe = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
    let user_dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|base| base.join("radio-track-splitter"));
    beside_exe.into_iter().chain(user_dir).map(|dir| dir.join(&name)).collect()
}

/// Loads the cached embeddings for `input` from any location, if still valid for the file.
pub fn load_for(input: &Path, key: (u64, u64)) -> Option<Embeddings> {
    locations(input).iter().find_map(|p| load(p, key))
}

/// Saves to the first location that is writable.
pub fn save_for(input: &Path, key: (u64, u64), emb: &Embeddings) -> Result<()> {
    let mut last_err = None;
    for path in locations(input) {
        match save(&path, key, emb) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no cache location available")))
}

pub struct Embeddings {
    pub data: Vec<f32>,
    pub n: usize,
    pub dim: usize,
    pub hop: f64,
}

pub fn source_key(input: &Path) -> Result<(u64, u64)> {
    let meta = fs::metadata(input)?;
    let mtime_ns = meta.modified()?.duration_since(UNIX_EPOCH)?.as_nanos() as u64;
    Ok((meta.len(), mtime_ns))
}

pub fn load(path: &Path, key: (u64, u64)) -> Option<Embeddings> {
    let bytes = fs::read(path).ok()?;
    let header = 8 + 8 + 8 + 8 + 8 + 8;
    if bytes.len() < header || &bytes[..8] != MAGIC {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    if (u64_at(8), u64_at(16)) != key {
        return None;
    }
    let (n, dim) = (u64_at(24) as usize, u64_at(32) as usize);
    let hop = f64::from_le_bytes(bytes[40..48].try_into().unwrap());
    if bytes.len() != header + n * dim * 4 {
        return None;
    }
    let data = bytes[header..]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    Some(Embeddings { data, n, dim, hop })
}

pub fn save(path: &Path, key: (u64, u64), emb: &Embeddings) -> Result<()> {
    if emb.data.len() != emb.n * emb.dim {
        bail!("embedding buffer has the wrong size");
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut out = Vec::with_capacity(48 + emb.data.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&key.0.to_le_bytes());
    out.extend_from_slice(&key.1.to_le_bytes());
    out.extend_from_slice(&(emb.n as u64).to_le_bytes());
    out.extend_from_slice(&(emb.dim as u64).to_le_bytes());
    out.extend_from_slice(&emb.hop.to_le_bytes());
    for v in &emb.data {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Embeddings {
        Embeddings { data: vec![0.5; 6], n: 3, dim: 2, hop: 0.96 }
    }

    #[test]
    fn round_trip_rejects_a_different_file_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        save(&path, (10, 20), &sample()).unwrap();
        let loaded = load(&path, (10, 20)).unwrap();
        assert_eq!((loaded.n, loaded.dim, loaded.data.len()), (3, 2, 6));
        assert!(load(&path, (10, 21)).is_none(), "modified file must miss");
        assert!(load(&path, (11, 20)).is_none(), "resized file must miss");
    }

    #[test]
    fn each_input_gets_its_own_cache_file() {
        let a = locations(Path::new("a.mp3"));
        let b = locations(Path::new("b.mp3"));
        assert!(!a.is_empty());
        assert_ne!(a[0].file_name(), b[0].file_name());
        assert_eq!(a[0].file_name(), locations(Path::new("a.mp3"))[0].file_name());
    }
}
