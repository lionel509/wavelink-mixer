//! Model files: fetched on first use into ~/.local/share/wavelink/models, checked against a pinned SHA-256.
#![allow(dead_code, unused_imports, unused_variables)]
use std::path::{Path, PathBuf};

pub const RELEASE: &str = "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/";

/// (file name, url, sha256)
pub const MODELS: [(&str, &str, &str); 6] = [
    ("stemgenrt-v0.7-independent.onnx", "https://github.com/sweetspotsoundsystem/StemgenRT-5.8/releases/download/v0.7.0/independent.onnx", "0a49388dd2a47986ab9a161c8764c22853b4d601c188ac052eb530f076d513c7"),
    ("basic-pitch-nmp.onnx", "https://raw.githubusercontent.com/spotify/basic-pitch/v0.4.0/basic_pitch/saved_models/icassp_2022/nmp.onnx", "2c3c1d144bfa61ad236e92e169c13535c880469a12a047d4e73451f2c059a0ec"),
    ("umxhq-vocals.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-vocals.onnx", "790c961f72f807a33e29d9b2bc689634d1c0a0049ba5b10c03a248cb83f169df"),
    ("umxhq-drums.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-drums.onnx", "9fb5483ed85ff34cfd494e28309862c0003b9cfdf98b4b3d3097a95b425411db"),
    ("umxhq-bass.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-bass.onnx", "4ed20bddaafefb11326f9e42e806a3673561ed60af078ded7c8ea93e60881763"),
    ("umxhq-other.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-other.onnx", "ffa43a4eafebb9ad8d02c521a59edfe521351772a083da41183e118ac634e3a6"),
];
pub const S_FILES: [&str; 2] = ["stemgenrt-v0.7-independent.onnx", "basic-pitch-nmp.onnx"];
pub const U_FILES: [&str; 5] = ["umxhq-vocals.onnx", "umxhq-drums.onnx", "umxhq-bass.onnx", "umxhq-other.onnx", "basic-pitch-nmp.onnx"];

pub struct ModelSet {
    pub dir: PathBuf,
}

impl ModelSet {
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// config::glib_home()/.local/share/wavelink/models
pub fn dir() -> PathBuf {
    crate::config::glib_home().join(".local/share/wavelink/models")
}

pub fn verify(path: &Path, sha_hex: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| format!("read {}: {e}", path.display()))?;
    let got = format!("{:x}", h.finalize());
    if got.eq_ignore_ascii_case(sha_hex) {
        Ok(())
    } else {
        Err(format!("sha256 mismatch for {}: got {got}", path.display()))
    }
}

/// Present and verified files are skipped; otherwise fetch (120 s timeout), write <name>.part, verify, rename.
/// A "TODO" sha is an error: "model <name> not published yet".
pub fn ensure(names: &[&str], status: &dyn Fn(String)) -> Result<ModelSet, String> {
    let dir = dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let set = ModelSet { dir };
    for name in names {
        let (_, url, sha) = *MODELS
            .iter()
            .find(|m| m.0 == *name)
            .ok_or_else(|| format!("unknown model {name}"))?;
        if sha == "TODO" {
            return Err(format!("model {name} not published yet"));
        }
        let path = set.path(name);
        if path.exists() {
            if verify(&path, sha).is_ok() {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
        status(format!("fetching {name}…"));
        fetch(url, sha, &path, name)?;
    }
    Ok(set)
}

/// Download `url` to `<name>.part`, check it, and rename it into place. A wrong hash is deleted
/// and fetched once more before giving up.
fn fetch(url: &str, sha: &str, path: &Path, name: &str) -> Result<(), String> {
    let part = path.with_file_name(format!("{name}.part"));
    let mut last = String::new();
    for _ in 0..2 {
        let resp = ureq::get(url)
            .timeout(std::time::Duration::from_secs(120))
            .call()
            .map_err(|e| format!("fetch {name}: {e}"))?;
        {
            let mut f = std::fs::File::create(&part).map_err(|e| format!("create {}: {e}", part.display()))?;
            std::io::copy(&mut resp.into_reader(), &mut f).map_err(|e| format!("download {name}: {e}"))?;
        }
        match verify(&part, sha) {
            Ok(()) => {
                std::fs::rename(&part, path).map_err(|e| format!("rename {name}: {e}"))?;
                return Ok(());
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                last = e;
            }
        }
    }
    Err(format!("{last} (fetched twice)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn sha_mismatch_is_error() {
        let p = std::env::temp_dir().join(format!("wl-models-test-{}.bin", std::process::id()));
        std::fs::write(&p, b"other bytes").unwrap();
        let wrong = format!("{:x}", Sha256::digest(b"not these bytes"));
        let right = format!("{:x}", Sha256::digest(b"other bytes"));
        assert!(verify(&p, &wrong).is_err());
        assert!(verify(&p, &right).is_ok());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn model_list_is_pinned() {
        assert_eq!(MODELS.len(), 6);
        let mut names: Vec<&str> = MODELS.iter().map(|m| m.0).collect();
        assert!(names.iter().all(|n| n.ends_with(".onnx")));
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 6, "names must be unique");
        for (name, url, sha) in MODELS.iter() {
            assert!(url.starts_with("https://"), "{name}: {url}");
            assert!(
                sha.len() == 64 && sha.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{name}: sha {sha}"
            );
        }
    }

    #[test]
    fn file_lists_resolve() {
        for n in S_FILES.iter().chain(U_FILES.iter()) {
            assert!(MODELS.iter().any(|m| m.0 == *n), "{n} is not in MODELS");
        }
    }
}
