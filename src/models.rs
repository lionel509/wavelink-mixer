//! Model files: fetched on first use into ~/.local/share/wavelink/models, checked against a pinned SHA-256.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 2.
#![allow(dead_code, unused_imports, unused_variables)]
use std::path::{Path, PathBuf};

pub const RELEASE: &str = "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/";

/// (file name, url, sha256)
pub const MODELS: [(&str, &str, &str); 6] = [
    ("stemgenrt-v0.7-independent.onnx", "https://github.com/sweetspotsoundsystem/StemgenRT-5.8/releases/download/v0.7.0/independent.onnx", "0a49388dd2a47986ab9a161c8764c22853b4d601c188ac052eb530f076d513c7"),
    ("basic-pitch-nmp.onnx", "https://raw.githubusercontent.com/spotify/basic-pitch/v0.4.0/basic_pitch/saved_models/icassp_2022/nmp.onnx", "2c3c1d144bfa61ad236e92e169c13535c880469a12a047d4e73451f2c059a0ec"),
    ("umxhq-vocals.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-vocals.onnx", "TODO"),
    ("umxhq-drums.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-drums.onnx", "TODO"),
    ("umxhq-bass.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-bass.onnx", "TODO"),
    ("umxhq-other.onnx", "https://github.com/lionel509/wavelink-mixer/releases/download/models-v1/umxhq-other.onnx", "TODO"),
];
pub const S_FILES: [&str; 2] = ["stemgenrt-v0.7-independent.onnx", "basic-pitch-nmp.onnx"];
pub const U_FILES: [&str; 5] = ["umxhq-vocals.onnx", "umxhq-drums.onnx", "umxhq-bass.onnx", "umxhq-other.onnx", "basic-pitch-nmp.onnx"];

pub struct ModelSet {
    pub dir: PathBuf,
}

impl ModelSet {
    pub fn path(&self, name: &str) -> PathBuf {
        todo!()
    }
}

/// config::glib_home()/.local/share/wavelink/models
pub fn dir() -> PathBuf {
    todo!()
}

pub fn verify(path: &Path, sha_hex: &str) -> Result<(), String> {
    todo!()
}

/// Present and verified files are skipped; otherwise fetch (120 s timeout), write <name>.part, verify, rename.
/// A "TODO" sha is an error: "model <name> not published yet".
pub fn ensure(names: &[&str], status: &dyn Fn(String)) -> Result<ModelSet, String> {
    todo!()
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
