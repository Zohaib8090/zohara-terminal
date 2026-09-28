//! `ut-plugin-signing`: signed plugin manifests (Section 19 "Signed
//! plugins" / "Plugin compatibility checks" / "Plugin version pinning").
//!
//! A plugin package ships a [`Manifest`] (identity, version, requested
//! capabilities, and a content hash of the actual plugin artifact) plus an
//! ed25519 signature over that manifest's canonical bytes. Verification
//! happens against a [`Keyring`] of *trusted* publisher keys the plugin
//! manager already holds — never against a key embedded in the package
//! itself, since an attacker can always generate their own keypair and
//! self-sign. `Keyring::verify` is what a plugin manager calls before
//! installing or updating a plugin; a manifest whose `artifact_sha256`
//! doesn't match the actual downloaded bytes, or whose signature doesn't
//! verify against any trusted key, is rejected before anything from the
//! package runs.
//!
//! What this crate does *not* do: sandboxing the plugin's actual code at
//! runtime (process isolation, syscall filtering) — that's a separate,
//! OS-level concern for whatever loads the plugin (`libloading`, a
//! subprocess, a WASM runtime); this crate only answers "should the plugin
//! manager trust and install this package at all".

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A plugin package's identity and metadata, prior to signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    /// Semver-ish version string, e.g. "1.2.0".
    pub version: String,
    pub author: String,
    /// Capability names requested, matching `ut_plugin_api::Permissions`
    /// field names (e.g. "read_output", "network") — kept as plain strings
    /// here so this crate doesn't need to depend on `ut-plugin-api`.
    pub requested_capabilities: Vec<String>,
    /// SHA-256 of the actual plugin artifact (the code/package bytes),
    /// hex-encoded. This is what pins a signature to one specific build —
    /// "Plugin version pinning" / tamper detection.
    pub artifact_sha256: String,
}

impl Manifest {
    /// A deterministic byte encoding of the manifest to sign/verify over.
    /// Deliberately not JSON (whose key ordering isn't guaranteed stable
    /// across serializers) — a fixed field order with explicit separators.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for field in [self.id.as_str(), self.name.as_str(), self.version.as_str(), self.author.as_str()] {
            out.extend_from_slice(field.as_bytes());
            out.push(0);
        }
        for cap in &self.requested_capabilities {
            out.extend_from_slice(cap.as_bytes());
            out.push(0);
        }
        out.push(0xFF); // separator before the artifact hash
        out.extend_from_slice(self.artifact_sha256.as_bytes());
        out
    }
}

/// Hash a plugin artifact's raw bytes for embedding in a [`Manifest`].
pub fn hash_artifact(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A manifest plus its ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    pub manifest: Manifest,
    /// Hex-encoded ed25519 signature over `manifest.canonical_bytes()`.
    pub signature: String,
    /// Hex-encoded public key that produced the signature, so a verifier
    /// can look it up in its keyring without guessing.
    pub public_key: String,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SigningError {
    #[error("malformed hex in signature or key")]
    MalformedHex,
    #[error("signature does not verify against the embedded public key")]
    InvalidSignature,
    #[error("public key is not in the trusted keyring")]
    UntrustedKey,
    #[error("artifact hash does not match the manifest's recorded hash")]
    ArtifactMismatch,
}

/// A publisher's signing keypair. A real plugin publisher generates one of
/// these once and keeps the private half secret; only `public_key_hex` is
/// ever shared (and added to a `Keyring`).
pub struct PluginKeypair {
    signing_key: SigningKey,
}

impl PluginKeypair {
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        PluginKeypair { signing_key: SigningKey::generate(&mut rng) }
    }

    pub fn from_bytes(secret: &[u8; 32]) -> Self {
        PluginKeypair { signing_key: SigningKey::from_bytes(secret) }
    }

    pub fn public_key_hex(&self) -> String {
        hex_encode(self.signing_key.verifying_key().as_bytes())
    }

    pub fn sign(&self, manifest: Manifest) -> SignedManifest {
        let sig: Signature = self.signing_key.sign(&manifest.canonical_bytes());
        SignedManifest { manifest, signature: hex_encode(&sig.to_bytes()), public_key: self.public_key_hex() }
    }
}

/// The set of publisher public keys a plugin manager trusts, each with a
/// human-readable label ("official", "community", a specific org name —
/// "Official repository indicator" / "Community repository indicator" for
/// plugins, mirroring the spec's package-manager source indicators).
#[derive(Default)]
pub struct Keyring {
    trusted: HashMap<String, (VerifyingKey, String)>,
}

impl Keyring {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trust(&mut self, public_key_hex: &str, label: impl Into<String>) -> Result<(), SigningError> {
        let key_bytes = decode_hex_32(public_key_hex)?;
        let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| SigningError::MalformedHex)?;
        self.trusted.insert(public_key_hex.to_string(), (key, label.into()));
        Ok(())
    }

    pub fn untrust(&mut self, public_key_hex: &str) {
        self.trusted.remove(public_key_hex);
    }

    pub fn is_trusted(&self, public_key_hex: &str) -> bool {
        self.trusted.contains_key(public_key_hex)
    }

    /// Verify a signed manifest against this keyring and (if provided) the
    /// actual downloaded artifact bytes. Returns the trust label on
    /// success (e.g. `"official"`), so the caller can show "Official
    /// repository indicator"-style UI.
    pub fn verify<'a>(&'a self, signed: &SignedManifest, artifact_bytes: Option<&[u8]>) -> Result<&'a str, SigningError> {
        if let Some(bytes) = artifact_bytes {
            if hash_artifact(bytes) != signed.manifest.artifact_sha256 {
                return Err(SigningError::ArtifactMismatch);
            }
        }

        let (key, label) = self.trusted.get(&signed.public_key).ok_or(SigningError::UntrustedKey)?;

        let sig_bytes = decode_hex_64(&signed.signature)?;
        let signature = Signature::from_bytes(&sig_bytes);
        key.verify(&signed.manifest.canonical_bytes(), &signature).map_err(|_| SigningError::InvalidSignature)?;

        Ok(label.as_str())
    }
}

fn decode_hex_32(s: &str) -> Result<[u8; 32], SigningError> {
    let v = decode_hex(s)?;
    v.try_into().map_err(|_| SigningError::MalformedHex)
}

fn decode_hex_64(s: &str) -> Result<[u8; 64], SigningError> {
    let v = decode_hex(s)?;
    v.try_into().map_err(|_| SigningError::MalformedHex)
}

fn decode_hex(s: &str) -> Result<Vec<u8>, SigningError> {
    if !s.len().is_multiple_of(2) {
        return Err(SigningError::MalformedHex);
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| SigningError::MalformedHex))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(hash: &str) -> Manifest {
        Manifest {
            id: "ssh".into(),
            name: "SSH Plugin".into(),
            version: "1.0.0".into(),
            author: "Zohara".into(),
            requested_capabilities: vec!["read_output".into(), "network".into()],
            artifact_sha256: hash.into(),
        }
    }

    #[test]
    fn sign_then_verify_succeeds_with_trusted_key() {
        let artifact = b"pretend plugin binary bytes";
        let hash = hash_artifact(artifact);
        let keypair = PluginKeypair::generate();
        let signed = keypair.sign(sample_manifest(&hash));

        let mut keyring = Keyring::new();
        keyring.trust(&keypair.public_key_hex(), "official").unwrap();

        let label = keyring.verify(&signed, Some(artifact)).unwrap();
        assert_eq!(label, "official");
    }

    #[test]
    fn verify_fails_for_untrusted_key() {
        let hash = hash_artifact(b"artifact");
        let keypair = PluginKeypair::generate();
        let signed = keypair.sign(sample_manifest(&hash));

        let keyring = Keyring::new(); // nothing trusted
        assert_eq!(keyring.verify(&signed, None), Err(SigningError::UntrustedKey));
    }

    #[test]
    fn verify_fails_if_manifest_tampered_after_signing() {
        let hash = hash_artifact(b"artifact");
        let keypair = PluginKeypair::generate();
        let mut signed = keypair.sign(sample_manifest(&hash));
        signed.manifest.version = "9.9.9".into(); // tamper

        let mut keyring = Keyring::new();
        keyring.trust(&keypair.public_key_hex(), "official").unwrap();
        assert_eq!(keyring.verify(&signed, None), Err(SigningError::InvalidSignature));
    }

    #[test]
    fn verify_fails_if_artifact_bytes_dont_match_hash() {
        let hash = hash_artifact(b"original bytes");
        let keypair = PluginKeypair::generate();
        let signed = keypair.sign(sample_manifest(&hash));

        let mut keyring = Keyring::new();
        keyring.trust(&keypair.public_key_hex(), "official").unwrap();

        let result = keyring.verify(&signed, Some(b"different, possibly malicious bytes"));
        assert_eq!(result, Err(SigningError::ArtifactMismatch));
    }

    #[test]
    fn different_signer_cannot_forge_a_trusted_signature() {
        let hash = hash_artifact(b"artifact");
        let real_publisher = PluginKeypair::generate();
        let attacker = PluginKeypair::generate();

        let mut keyring = Keyring::new();
        keyring.trust(&real_publisher.public_key_hex(), "official").unwrap();

        // Attacker signs with their own key but claims to be the real publisher's package.
        let forged = attacker.sign(sample_manifest(&hash));
        assert_eq!(keyring.verify(&forged, None), Err(SigningError::UntrustedKey));
    }

    #[test]
    fn untrust_revokes_a_previously_trusted_key() {
        let hash = hash_artifact(b"artifact");
        let keypair = PluginKeypair::generate();
        let signed = keypair.sign(sample_manifest(&hash));

        let mut keyring = Keyring::new();
        keyring.trust(&keypair.public_key_hex(), "official").unwrap();
        assert!(keyring.verify(&signed, None).is_ok());

        keyring.untrust(&keypair.public_key_hex());
        assert_eq!(keyring.verify(&signed, None), Err(SigningError::UntrustedKey));
    }

    #[test]
    fn hash_artifact_is_deterministic_and_content_sensitive() {
        let h1 = hash_artifact(b"hello");
        let h2 = hash_artifact(b"hello");
        let h3 = hash_artifact(b"hellp");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn keypair_from_fixed_bytes_is_reproducible() {
        let seed = [7u8; 32];
        let a = PluginKeypair::from_bytes(&seed);
        let b = PluginKeypair::from_bytes(&seed);
        assert_eq!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let m = sample_manifest("deadbeef");
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
