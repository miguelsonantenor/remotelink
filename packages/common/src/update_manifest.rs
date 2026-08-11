//! Signed client update manifest (schema + ed25519 sign/verify).
//!
//! Host and viewer poll a channel-pinned manifest on a timer (not via the remote
//! session). Release keys sign the canonical JSON body; clients verify with the
//! embedded public key. Device enrollment keys are **not** used here.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Domain separator for update-manifest signatures (v1).
pub const MANIFEST_DOMAIN: &[u8] = b"remotelink-update-manifest-v1";

/// Current schema version for [`UpdateManifest`].
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Errors from signing, verifying, or parsing update manifests.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ManifestError {
    /// JSON (de)serialization failure.
    #[error("manifest json: {0}")]
    Json(String),
    /// Signature length or cryptographic verification failed.
    #[error("invalid update manifest signature")]
    InvalidSignature,
    /// Key material was not the expected length / format.
    #[error("crypto: {0}")]
    Crypto(String),
    /// Manifest schema or channel failed validation.
    #[error("invalid manifest: {0}")]
    Invalid(String),
    /// Base64 decode failure.
    #[error("base64: {0}")]
    Base64(String),
}

/// Result alias for manifest operations.
pub type ManifestResult<T> = Result<T, ManifestError>;

/// Update distribution channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    /// Open / closed beta track (may force-update more aggressively).
    Beta,
    /// Stable / GA candidate track.
    Stable,
}

impl UpdateChannel {
    /// Wire string (`beta` / `stable`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Beta => "beta",
            Self::Stable => "stable",
        }
    }

    /// Parse a channel name (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "beta" => Some(Self::Beta),
            "stable" => Some(Self::Stable),
            _ => None,
        }
    }
}

/// One downloadable artifact referenced by a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestArtifact {
    /// Logical name (e.g. `remotelink-host`, `remotelink-viewer`).
    pub name: String,
    /// Target triple-ish label (`windows-x86_64`).
    pub platform: String,
    /// HTTPS download URL (or path for tests).
    pub url: String,
    /// Lowercase hex SHA-256 of the artifact bytes.
    pub sha256: String,
    /// Size in bytes (advisory; clients should still hash).
    pub size_bytes: u64,
    /// Optional package format hint (`msi`, `msix`, `exe`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

/// Unsigned update manifest body (the signed payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// Schema version (currently [`MANIFEST_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Distribution channel.
    pub channel: UpdateChannel,
    /// Semver-ish product version this manifest advertises.
    pub version: String,
    /// RFC 3339 release timestamp.
    pub released_at: String,
    /// Minimum protocol version clients must speak after updating.
    pub min_protocol_version: u32,
    /// Clients below this version should force-update before connecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_update_below: Option<String>,
    /// Artifacts for this release.
    pub artifacts: Vec<ManifestArtifact>,
    /// Optional human-readable notes / changelog URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Signed envelope: manifest body + base64 ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedUpdateManifest {
    /// Unsigned body.
    pub manifest: UpdateManifest,
    /// Standard base64 encoding of the 64-byte ed25519 signature.
    pub signature_b64: String,
    /// Optional key identifier for key rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

impl UpdateManifest {
    /// Lightweight structural validation (not signature).
    pub fn validate(&self) -> ManifestResult<()> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::Invalid(format!(
                "unsupported schema_version {} (expected {})",
                self.schema_version, MANIFEST_SCHEMA_VERSION
            )));
        }
        if self.version.trim().is_empty() {
            return Err(ManifestError::Invalid("version is required".into()));
        }
        if self.released_at.trim().is_empty() {
            return Err(ManifestError::Invalid("released_at is required".into()));
        }
        if self.artifacts.is_empty() {
            return Err(ManifestError::Invalid(
                "at least one artifact is required".into(),
            ));
        }
        for (i, a) in self.artifacts.iter().enumerate() {
            if a.name.trim().is_empty() {
                return Err(ManifestError::Invalid(format!(
                    "artifact[{i}].name is required"
                )));
            }
            if a.sha256.len() != 64 || !a.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ManifestError::Invalid(format!(
                    "artifact[{i}].sha256 must be 64 hex chars"
                )));
            }
        }
        Ok(())
    }

    /// Canonical JSON bytes used for signing (serde_json field order as defined).
    pub fn canonical_bytes(&self) -> ManifestResult<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| ManifestError::Json(e.to_string()))
    }
}

/// Build the domain-separated message that is actually signed.
pub fn encode_manifest_message(canonical_json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MANIFEST_DOMAIN.len() + 8 + canonical_json.len());
    out.extend_from_slice(MANIFEST_DOMAIN);
    let len = u64::try_from(canonical_json.len()).expect("manifest fits u64");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(canonical_json);
    out
}

/// Generate a dedicated release signing keypair (not a device key).
pub fn generate_manifest_keypair() -> (SigningKey, VerifyingKey) {
    let signing = SigningKey::generate(&mut OsRng);
    let verifying = signing.verifying_key();
    (signing, verifying)
}

/// Reconstruct a verifying key from 32 raw bytes.
pub fn verifying_key_from_bytes(bytes: &[u8]) -> ManifestResult<VerifyingKey> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ManifestError::Crypto("verifying key must be 32 bytes".into()))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| ManifestError::Crypto(e.to_string()))
}

/// Reconstruct a signing key from 32 raw seed bytes.
pub fn signing_key_from_bytes(bytes: &[u8]) -> ManifestResult<SigningKey> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ManifestError::Crypto("signing key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&arr))
}

/// Sign a validated manifest; returns a [`SignedUpdateManifest`].
pub fn sign_manifest(
    signing_key: &SigningKey,
    manifest: UpdateManifest,
    key_id: Option<String>,
) -> ManifestResult<SignedUpdateManifest> {
    manifest.validate()?;
    let canonical = manifest.canonical_bytes()?;
    let msg = encode_manifest_message(&canonical);
    let sig = signing_key.sign(&msg);
    let signature_b64 = base64_encode(&sig.to_bytes());
    Ok(SignedUpdateManifest {
        manifest,
        signature_b64,
        key_id,
    })
}

/// Verify signature and structural validity (**crypto only**).
///
/// Does **not** enforce a channel pin. Callers that pin `beta`/`stable` at
/// install time **must** use [`verify_manifest_for_channel`] so a validly signed
/// payload for the other track is rejected.
pub fn verify_manifest(
    verifying_key: &VerifyingKey,
    signed: &SignedUpdateManifest,
) -> ManifestResult<()> {
    signed.manifest.validate()?;
    let canonical = signed.manifest.canonical_bytes()?;
    let msg = encode_manifest_message(&canonical);
    let sig_bytes = base64_decode(&signed.signature_b64)?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| ManifestError::InvalidSignature)?;
    let sig = Signature::from_bytes(&sig_arr);
    verifying_key
        .verify(&msg, &sig)
        .map_err(|_| ManifestError::InvalidSignature)?;
    Ok(())
}

/// Verify signature, structure, **and** channel pin.
///
/// Preferred client entry point: refuses a validly signed manifest whose
/// `channel` does not match the install-time pin (e.g. beta force-update while
/// the client is pinned to stable). Signature is checked first; channel
/// mismatch returns [`ManifestError::Invalid`].
pub fn verify_manifest_for_channel(
    verifying_key: &VerifyingKey,
    signed: &SignedUpdateManifest,
    expected: UpdateChannel,
) -> ManifestResult<()> {
    verify_manifest(verifying_key, signed)?;
    if signed.manifest.channel != expected {
        return Err(ManifestError::Invalid(format!(
            "channel pin mismatch: manifest is {}, expected {}",
            signed.manifest.channel.as_str(),
            expected.as_str()
        )));
    }
    Ok(())
}

/// Parse JSON into a signed manifest (does not verify).
pub fn parse_signed_manifest(json: &[u8]) -> ManifestResult<SignedUpdateManifest> {
    serde_json::from_slice(json).map_err(|e| ManifestError::Json(e.to_string()))
}

/// Serialize a signed manifest to JSON.
pub fn signed_manifest_to_json(signed: &SignedUpdateManifest) -> ManifestResult<Vec<u8>> {
    serde_json::to_vec(signed).map_err(|e| ManifestError::Json(e.to_string()))
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> ManifestResult<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| ManifestError::Base64(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> UpdateManifest {
        UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            channel: UpdateChannel::Beta,
            version: "0.1.0".into(),
            released_at: "2026-04-01T00:00:00Z".into(),
            min_protocol_version: 1,
            force_update_below: Some("0.0.9".into()),
            artifacts: vec![ManifestArtifact {
                name: "remotelink-host".into(),
                platform: "windows-x86_64".into(),
                url: "https://example.test/host-0.1.0.msi".into(),
                sha256: "a".repeat(64),
                size_bytes: 1_048_576,
                package: Some("msi".into()),
            }],
            notes: Some("beta pin test".into()),
        }
    }

    #[test]
    fn channel_parse() {
        assert_eq!(UpdateChannel::parse("BETA"), Some(UpdateChannel::Beta));
        assert_eq!(UpdateChannel::parse("stable"), Some(UpdateChannel::Stable));
        assert_eq!(UpdateChannel::parse("nightly"), None);
        assert_eq!(UpdateChannel::Beta.as_str(), "beta");
    }

    #[test]
    fn sign_verify_roundtrip() {
        let (sk, vk) = generate_manifest_keypair();
        let signed = sign_manifest(&sk, sample_manifest(), Some("release-1".into())).unwrap();
        assert_eq!(signed.key_id.as_deref(), Some("release-1"));
        verify_manifest(&vk, &signed).unwrap();

        let json = signed_manifest_to_json(&signed).unwrap();
        let parsed = parse_signed_manifest(&json).unwrap();
        verify_manifest(&vk, &parsed).unwrap();
    }

    #[test]
    fn tampered_version_fails() {
        let (sk, vk) = generate_manifest_keypair();
        let mut signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        signed.manifest.version = "9.9.9".into();
        assert_eq!(
            verify_manifest(&vk, &signed),
            Err(ManifestError::InvalidSignature)
        );
    }

    #[test]
    fn wrong_key_fails() {
        let (sk, _) = generate_manifest_keypair();
        let (_, vk2) = generate_manifest_keypair();
        let signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        assert_eq!(
            verify_manifest(&vk2, &signed),
            Err(ManifestError::InvalidSignature)
        );
    }

    #[test]
    fn bad_signature_b64_fails() {
        let (sk, vk) = generate_manifest_keypair();
        let mut signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        signed.signature_b64 = "not-valid-base64!!!".into();
        assert!(matches!(
            verify_manifest(&vk, &signed),
            Err(ManifestError::Base64(_))
        ));
    }

    #[test]
    fn validate_rejects_bad_sha() {
        let mut m = sample_manifest();
        m.artifacts[0].sha256 = "deadbeef".into();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_empty_artifacts() {
        let mut m = sample_manifest();
        m.artifacts.clear();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn key_bytes_roundtrip() {
        let (sk, vk) = generate_manifest_keypair();
        let sk2 = signing_key_from_bytes(sk.as_bytes()).unwrap();
        let vk2 = verifying_key_from_bytes(vk.as_bytes()).unwrap();
        let signed = sign_manifest(&sk2, sample_manifest(), None).unwrap();
        verify_manifest(&vk2, &signed).unwrap();
    }

    #[test]
    fn encode_includes_domain() {
        let msg = encode_manifest_message(b"{}");
        assert!(msg
            .windows(MANIFEST_DOMAIN.len())
            .any(|w| w == MANIFEST_DOMAIN));
    }

    #[test]
    fn sample_json_shape() {
        let (sk, _) = generate_manifest_keypair();
        let signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&signed_manifest_to_json(&signed).unwrap()).unwrap();
        assert!(v.get("manifest").is_some());
        assert!(v.get("signature_b64").is_some());
        assert_eq!(v["manifest"]["channel"], "beta");
        assert_eq!(v["manifest"]["schema_version"], 1);
    }

    #[test]
    fn channel_pin_accepts_matching() {
        let (sk, vk) = generate_manifest_keypair();
        let signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        assert_eq!(signed.manifest.channel, UpdateChannel::Beta);
        verify_manifest_for_channel(&vk, &signed, UpdateChannel::Beta).unwrap();
    }

    #[test]
    fn channel_pin_rejects_wrong_track() {
        let (sk, vk) = generate_manifest_keypair();
        // Valid crypto for beta; stable pin must refuse.
        let signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        assert!(verify_manifest(&vk, &signed).is_ok());
        let err = verify_manifest_for_channel(&vk, &signed, UpdateChannel::Stable).unwrap_err();
        assert!(
            matches!(err, ManifestError::Invalid(ref m) if m.contains("channel pin mismatch")),
            "got {err:?}"
        );
    }

    #[test]
    fn channel_pin_still_requires_valid_signature() {
        let (sk, _) = generate_manifest_keypair();
        let (_, vk2) = generate_manifest_keypair();
        let signed = sign_manifest(&sk, sample_manifest(), None).unwrap();
        assert_eq!(
            verify_manifest_for_channel(&vk2, &signed, UpdateChannel::Beta),
            Err(ManifestError::InvalidSignature)
        );
    }
}
