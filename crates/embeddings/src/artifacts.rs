//! Model artifact management: explicit fetch, SHA-256 cache verification and
//! offline-only load (WP-06 tasks 7/8; design §9.2).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("model '{0}' is not a supported recipe: {1}")]
    UnsupportedModel(String, String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(
        "artifact verification failed for '{file}': digest mismatch (expected {expected}, got {actual})"
    )]
    DigestMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    #[error(
        "offline mode is active and artifact '{file}' is not cached; fetch it explicitly first"
    )]
    OfflineMissing { file: String },
    #[error("artifact download failed for '{0}': {1}")]
    Download(String, String),
    #[error("inference error: {0}")]
    Inference(#[from] candle_core::Error),
}

impl From<tokenizers::Error> for ArtifactError {
    fn from(e: tokenizers::Error) -> Self {
        ArtifactError::Download(String::new(), format!("tokenizer: {e}"))
    }
}

pub type ArtifactResult<T> = Result<T, ArtifactError>;

/// A pinned model artifact set (WP-06 task 1). Digests are the ONLY acceptable
/// values; any mismatch is a hard error.
#[derive(Debug, Clone)]
pub struct ModelArtifact {
    pub id: String,
    pub revision: String,
    pub license: String,
    /// Base URL for downloads (e.g., HF resolve endpoint).
    pub source_base_url: String,
    /// File name -> expected SHA-256 hex.
    pub digests: HashMap<String, String>,
}

impl ModelArtifact {
    fn url_for(&self, file: &str) -> String {
        format!("{}/{}", self.source_base_url.trim_end_matches('/'), file)
    }
}

/// Cache layout: <root>/<model_id>/<revision>/<file>. Atomic writes via temp+rename.
pub struct ArtifactCache {
    root: PathBuf,
    /// When true no network access is attempted; only verified cached files load.
    offline: bool,
}

impl ArtifactCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offline: false,
        }
    }

    /// Offline-only mode (WP-06 task 8): never attempt downloads.
    #[must_use]
    pub const fn offline(mut self) -> Self {
        self.offline = true;
        self
    }

    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// Fetch every artifact of `model` if absent, verify digests for all.
    /// Existing files are always re-verified (digest tampering is a hard error).
    pub async fn ensure_model(
        &self,
        model: &ModelArtifact,
        client: Option<&reqwest::Client>,
    ) -> ArtifactResult<PathBuf> {
        let dir = self.model_dir(model);
        std::fs::create_dir_all(&dir)?;

        for (file, expected) in &model.digests {
            let path = dir.join(file);
            if !path.exists() {
                if self.offline {
                    return Err(ArtifactError::OfflineMissing { file: file.clone() });
                }
                let Some(client) = client else {
                    return Err(ArtifactError::Download(
                        model.id.clone(),
                        "no network client available and artifact is not cached".into(),
                    ));
                };
                self.download_one(model, file, client).await?;
            }
            verify_digest(&path, expected)?;
        }

        Ok(dir)
    }

    /// Load a model directory strictly from cache (no download, offline-safe);
    /// verifies every digest and returns the directory.
    pub fn load_cached(&self, model: &ModelArtifact) -> ArtifactResult<PathBuf> {
        let dir = self.model_dir(model);
        for (file, expected) in &model.digests {
            let path = dir.join(file);
            if !path.exists() {
                return Err(ArtifactError::OfflineMissing { file: file.clone() });
            }
            verify_digest(&path, expected)?;
        }
        Ok(dir)
    }

    /// The cache directory for a model artifact (pub for test helpers).
    pub fn model_dir(&self, model: &ModelArtifact) -> PathBuf {
        self.root
            .join(model.id.as_str())
            .join(model.revision.as_str())
    }

    /// Upper bound for one downloaded artifact file (E5-small totals
    /// ~130MB; 1 GiB leaves generous headroom while a misbehaving endpoint
    /// can no longer force an unbounded response into memory or disk).
    const MAX_ARTIFACT_BYTES: u64 = 1 << 30;

    async fn download_one(
        &self,
        model: &ModelArtifact,
        file: &str,
        client: &reqwest::Client,
    ) -> ArtifactResult<()> {
        let url = model.url_for(file);
        tracing::info!(file, "downloading model artifact");

        let mut resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ArtifactError::Download(model.id.clone(), e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ArtifactError::Download(
                model.id.clone(),
                format!("HTTP {} for {}", resp.status(), url),
            ));
        }

        // Bound first: a declared length beyond the cap fails before
        // reading a single body byte.
        if let Some(len) = resp.content_length()
            && len > Self::MAX_ARTIFACT_BYTES
        {
            return Err(ArtifactError::Download(
                model.id.clone(),
                format!("artifact {file} declares {len} bytes, over the bound"),
            ));
        }

        // Stream chunk by chunk (never buffer the whole response): RAM
        // stays flat no matter what the endpoint sends. The digest is
        // verified by the caller over the published file, as before.
        let final_path = self.model_dir(model).join(file);
        let tmp_path = final_path.with_extension("part");
        let streamed = async {
            let mut staged = std::fs::File::create(&tmp_path)?;
            let mut total: u64 = 0;
            loop {
                match resp
                    .chunk()
                    .await
                    .map_err(|e| ArtifactError::Download(model.id.clone(), e.to_string()))?
                {
                    None => break,
                    Some(chunk) => {
                        total += chunk.len() as u64;
                        if total > Self::MAX_ARTIFACT_BYTES {
                            return Err(ArtifactError::Download(
                                model.id.clone(),
                                format!("artifact {file} exceeds the bound mid-download"),
                            ));
                        }
                        std::io::Write::write_all(&mut staged, &chunk)?;
                    }
                }
            }
            staged.sync_all()?;
            Ok::<(), ArtifactError>(())
        }
        .await;
        // A failed download never leaves a staged partial behind (a later
        // run recreates it from scratch).
        if streamed.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        streamed?;

        // Write atomically: temp file, then rename into place.
        std::fs::rename(&tmp_path, &final_path)?;

        Ok(())
    }
}

/// Verify a file's SHA-256 against the expected hex digest.
pub fn verify_digest(path: &Path, expected_hex: &str) -> ArtifactResult<()> {
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(ArtifactError::DigestMismatch {
            file: path.display().to_string(),
            expected: expected_hex.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Shared HTTP client for artifact downloads (kept out of the sync worker).
pub type HttpClient = Arc<reqwest::Client>;

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, files: &[(&str, &str)]) -> ModelArtifact {
        let mut digests = HashMap::new();
        for (f, d) in files {
            digests.insert((*f).to_string(), (*d).to_string());
        }
        ModelArtifact {
            id: id.to_string(),
            revision: "rev1".into(),
            license: "MIT".into(),
            source_base_url: "https://example.invalid/model/resolve/main".into(),
            digests,
        }
    }

    fn sha256_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn verify_digest_accepts_matching_and_rejects_tampered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let good = sha256_hex(b"hello world");
        assert!(verify_digest(&path, &good).is_ok());

        let bad = "0".repeat(64);
        match verify_digest(&path, &bad) {
            Err(ArtifactError::DigestMismatch { .. }) => {}
            other => panic!("expected DigestMismatch, got {other:?}"),
        }
    }

    #[test]
    fn offline_missing_artifact_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(dir.path()).offline();
        let m = model("m", &[("a.bin", &"0".repeat(64))]);

        match cache.load_cached(&m) {
            Err(ArtifactError::OfflineMissing { file }) => assert_eq!(file, "a.bin"),
            other => panic!("expected OfflineMissing, got {other:?}"),
        }
    }

    #[test]
    fn load_cached_verifies_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(dir.path());
        let m = model("m", &[("a.bin", &"1".repeat(64))]);
        let mdir = cache.model_dir(&m);
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("a.bin"), b"data").unwrap();

        // Tamper: expected digest wrong -> hard error.
        assert!(matches!(
            cache.load_cached(&m),
            Err(ArtifactError::DigestMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn ensure_model_offline_without_client_refuses_network() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(dir.path()).offline();
        let m = model("m", &[("a.bin", &"0".repeat(64))]);

        // Offline + missing: must not attempt download, clear error.
        match cache.ensure_model(&m, None).await {
            Err(ArtifactError::OfflineMissing { file }) => assert_eq!(file, "a.bin"),
            other => panic!("expected OfflineMissing, got {other:?}"),
        }

        // Online + missing + no client: still a clear error (never guesses).
        let cache2 = ArtifactCache::new(dir.path());
        match cache2.ensure_model(&m, None).await {
            Err(ArtifactError::Download(_, msg)) => assert!(msg.contains("not cached")),
            other => panic!("expected Download, got {other:?}"),
        }
    }
}
