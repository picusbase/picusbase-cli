//! Phase 4 slice A — artifact manifest + Ed25519 signing helpers.
//!
//! The control plane (Phase 2 slice B/C) accepts an optional manifest
//! checksum, signature, signer_id and signature_algorithm on publish. This
//! module gives the CLI a single place to:
//!   * Build a `manifest.json` document describing the function being
//!     published (function_key, name, language, runtime_class, entrypoint,
//!     artifact checksum + size, build timestamp).
//!   * Compute the manifest's own SHA-256 checksum so the control plane can
//!     dedupe / pin it.
//!   * Optionally load an Ed25519 private key from disk (PEM or raw 32-byte
//!     seed) and sign the artifact's lowercase-hex SHA-256 — matching the
//!     verification format implemented in
//!     `services/compute-control-plane/src/artifact_validation.rs`.
//!
//! The signing key path is a development convenience (per the active Compute
//! architecture contract in `docs-platform/01-architecture/04-compute-architecture.md`);
//! production-grade KMS-backed signing is left for a later phase.

use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub schema_version: String,
    pub function_key: String,
    pub name: String,
    pub language: String,
    pub runtime_class: String,
    pub entrypoint: String,
    pub artifact_checksum: String,
    pub artifact_size: u64,
    pub built_at: String,
}

impl ArtifactManifest {
    pub fn new(
        function_key: &str,
        name: &str,
        language: &str,
        runtime_class: &str,
        entrypoint: &str,
        artifact_bytes: &[u8],
    ) -> Self {
        Self {
            schema_version: "v1".to_string(),
            function_key: function_key.to_string(),
            name: name.to_string(),
            language: language.to_string(),
            runtime_class: runtime_class.to_string(),
            entrypoint: entrypoint.to_string(),
            artifact_checksum: sha256_hex(artifact_bytes),
            artifact_size: artifact_bytes.len() as u64,
            built_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Serialize the manifest to canonical JSON bytes (sorted keys via
    /// serde_json's default field ordering matching struct order).
    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("Failed to serialize manifest to JSON")
    }

    /// SHA-256 of `to_json_bytes()` as lowercase hex.
    pub fn checksum_hex(&self) -> Result<String> {
        Ok(sha256_hex(&self.to_json_bytes()?))
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let digest = h.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Load an Ed25519 signing key from disk. Accepts:
///   * PKCS8 PEM (`-----BEGIN PRIVATE KEY-----`)
///   * 32-byte raw seed file
fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read signing key at {}", path.display()))?;

    if bytes.starts_with(b"-----BEGIN") {
        let pem = std::str::from_utf8(&bytes).context("Signing key PEM is not valid UTF-8")?;
        // ed25519-dalek 2 exposes pkcs8 helpers behind the `pkcs8`+`pem`
        // features.
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        return SigningKey::from_pkcs8_pem(pem).context("Failed to parse signing key as PKCS8 PEM");
    }

    if bytes.len() == 32 {
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("32-byte signing key seed truncation")?;
        return Ok(SigningKey::from_bytes(&seed));
    }

    anyhow::bail!(
        "Signing key at {} is not recognized (expected PKCS8 PEM or 32-byte raw seed; got {} bytes)",
        path.display(),
        bytes.len()
    );
}

/// Sign the artifact checksum with an on-disk Ed25519 key. Signs the raw
/// bytes of the lowercase-hex string — matching
/// `artifact_validation::verify_signature` on the control plane side.
pub fn sign_artifact_checksum(key_path: &Path, artifact_checksum_hex: &str) -> Result<String> {
    let key = load_signing_key(key_path)?;
    let signature = key.sign(artifact_checksum_hex.as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;

    #[test]
    fn manifest_checksum_is_stable() {
        let m = ArtifactManifest::new(
            "fn-key",
            "Friendly Name",
            "rust",
            "native-v1",
            "main",
            b"hello-world",
        );
        let c1 = m.checksum_hex().unwrap();
        let c2 = m.checksum_hex().unwrap();
        assert_eq!(c1, c2);
        assert_eq!(c1.len(), 64); // 32-byte SHA-256 in hex
    }

    #[test]
    fn manifest_records_artifact_size_and_checksum() {
        let payload = b"abc";
        let m = ArtifactManifest::new("fn", "n", "rust", "native-v1", "main", payload);
        assert_eq!(m.artifact_size, 3);
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            m.artifact_checksum,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn signing_roundtrip_with_raw_seed() {
        // 32-byte deterministic seed for test reproducibility.
        let seed_path = std::env::temp_dir().join("picus-cli-test-seed.bin");
        let seed: [u8; 32] = [7u8; 32];
        std::fs::write(&seed_path, seed).unwrap();
        let key = SigningKey::from_bytes(&seed);
        let verifying_key = key.verifying_key();

        let checksum_hex = sha256_hex(b"some-artifact-bytes");
        let sig_b64 = sign_artifact_checksum(&seed_path, &checksum_hex).unwrap();

        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        verifying_key
            .verify(checksum_hex.as_bytes(), &sig)
            .expect("signature must verify against the matching public key");
        let _ = std::fs::remove_file(seed_path);
    }
}
