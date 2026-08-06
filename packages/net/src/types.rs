//! Shared types for PeerTransport (SDP, ICE, media units, connection state).

use remotelink_protocol::IceCandidate;
use std::time::Duration;

use crate::error::{NetError, Result};

/// SDP offer/answer kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpType {
    /// Local/remote offer.
    Offer,
    /// Local/remote answer.
    Answer,
    /// Provisional answer (rarely used in v1).
    Pranswer,
    /// Rollback to previous stable description.
    Rollback,
}

impl SdpType {
    /// Wire / debug name.
    pub fn as_str(self) -> &'static str {
        match self {
            SdpType::Offer => "offer",
            SdpType::Answer => "answer",
            SdpType::Pranswer => "pranswer",
            SdpType::Rollback => "rollback",
        }
    }
}

/// Session description (SDP) exchanged via signaling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescription {
    /// Offer or answer.
    pub sdp_type: SdpType,
    /// Full SDP body.
    pub sdp: String,
}

impl SessionDescription {
    /// Build an offer description.
    pub fn offer(sdp: impl Into<String>) -> Self {
        Self {
            sdp_type: SdpType::Offer,
            sdp: sdp.into(),
        }
    }

    /// Build an answer description.
    pub fn answer(sdp: impl Into<String>) -> Self {
        Self {
            sdp_type: SdpType::Answer,
            sdp: sdp.into(),
        }
    }
}

/// ICE candidate applied to or emitted by a peer connection.
///
/// Mirrors the protocol signaling shape so host/viewer can forward without remapping.
pub type TransportIceCandidate = IceCandidate;

/// DTLS certificate fingerprint for identity binding (SHA-256 of cert DER).
///
/// # Canonical form (PR 13 `fingerprint_sig`)
///
/// - `algorithm`: lowercase (`"sha-256"`).
/// - `value`: **uppercase** colon-separated hex, exactly 32 bytes (64 hex digits →
///   95 characters with colons).
/// - [`Self::as_sign_material`] returns `sha-256 AA:BB:…` (algorithm SP value) —
///   the fingerprint half of the signed payload. Host signs
///   `session_id || "\0" || as_sign_material()` (or equivalent documented in auth);
///   do **not** sign mixed-case or colon-stripped variants.
/// - [`Self::digest_bytes`] returns the raw 32-byte digest for binary APIs.
///
/// # Timing
///
/// - **Mock:** [`crate::PeerTransport::remote_fingerprint`] is parsed from the
///   remote SDP `a=fingerprint:` line when both descriptions are set (no real DTLS).
/// - **Real backends:** export local fingerprint from the DTLS certificate used in
///   the handshake; remote fingerprint must be taken from the **completed DTLS**
///   cert (not solely from SDP) before identity bind enables input.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DtlsFingerprint {
    /// Hash algorithm label (v1: always `"sha-256"` lowercase).
    pub algorithm: String,
    /// Colon-separated uppercase hex fingerprint value (32-byte SHA-256).
    pub value: String,
}

impl DtlsFingerprint {
    /// Create a sha-256 fingerprint from hex (with or without colons).
    ///
    /// Requires exactly 32 bytes of hex payload (64 hex digits).
    pub fn sha256(value: impl Into<String>) -> Result<Self> {
        let raw = value.into();
        let cleaned: String = raw.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if cleaned.len() != 64 {
            return Err(NetError::InvalidFingerprint(format!(
                "expected 64 hex digits (32 bytes), got {}",
                cleaned.len()
            )));
        }
        if !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(NetError::InvalidFingerprint("non-hex digit".into()));
        }
        let value = cleaned
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or("00").to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        Ok(Self {
            algorithm: "sha-256".into(),
            value,
        })
    }

    /// SDP `a=fingerprint:` attribute value (`sha-256 AA:BB:…`).
    pub fn sdp_attribute(&self) -> String {
        format!("{} {}", self.algorithm, self.value)
    }

    /// Canonical string for device-key signatures (`fingerprint_sig`).
    ///
    /// Always `sha-256` (lowercase) + space + uppercase colon-hex. Equal for two
    /// fingerprints that represent the same digest regardless of input casing.
    pub fn as_sign_material(&self) -> String {
        format!(
            "{} {}",
            self.algorithm.to_ascii_lowercase(),
            self.value.to_ascii_uppercase()
        )
    }

    /// Raw 32-byte SHA-256 digest parsed from [`Self::value`].
    pub fn digest_bytes(&self) -> Result<[u8; 32]> {
        let cleaned: String = self
            .value
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        if cleaned.len() != 64 {
            return Err(NetError::InvalidFingerprint(format!(
                "value is not 32 bytes: {} hex digits",
                cleaned.len()
            )));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in cleaned.as_bytes().chunks(2).enumerate() {
            let s = std::str::from_utf8(chunk)
                .map_err(|e| NetError::InvalidFingerprint(format!("utf8 in hex pair: {e}")))?;
            out[i] = u8::from_str_radix(s, 16)
                .map_err(|e| NetError::InvalidFingerprint(format!("hex parse: {e}")))?;
        }
        Ok(out)
    }
}

/// High-level ICE / DTLS peer connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Fresh peer; no descriptions yet.
    New,
    /// Local description set; gathering / waiting for remote.
    Connecting,
    /// ICE+DTLS established (mock: loopback ready).
    Connected,
    /// Temporarily disconnected; may recover (ICE restart path).
    Disconnected,
    /// Failed permanently until restart or close.
    Failed,
    /// Closed by application.
    Closed,
}

impl ConnectionState {
    /// Human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionState::New => "new",
            ConnectionState::Connecting => "connecting",
            ConnectionState::Connected => "connected",
            ConnectionState::Disconnected => "disconnected",
            ConnectionState::Failed => "failed",
            ConnectionState::Closed => "closed",
        }
    }
}

/// H.264 access unit / NAL unit batch from an external encoder (HW or SW).
///
/// Encoder owns bitstream format; transport packetizes into RTP. v1 expects
/// Annex-B start codes or AVCC length-prefixed NALs as indicated by `format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoNalu {
    /// Host-monotonic capture / encode PTS (shared A/V epoch on host).
    pub pts_host_mono: Duration,
    /// RTP timestamp (90 kHz) when known; mock may leave as `None` and derive later.
    pub rtp_ts: Option<u32>,
    /// True if this AU contains an IDR (keyframe).
    pub keyframe: bool,
    /// Bitstream packaging.
    pub format: NaluFormat,
    /// Encoded bytes (one or more NAL units).
    pub data: Vec<u8>,
}

/// How NAL units are framed in [`VideoNalu::data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NaluFormat {
    /// Annex-B: `00 00 00 01` / `00 00 01` start codes.
    AnnexB,
    /// AVCC: 4-byte big-endian length prefixes.
    Avcc,
}

/// Opus (or raw PCM passthrough in mock) audio packet for the audio track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    /// Host-monotonic PTS of the first sample.
    pub pts_host_mono: Duration,
    /// RTP timestamp (48 kHz) when known.
    pub rtp_ts: Option<u32>,
    /// Sample rate of the payload (Opus always 48 kHz decode clock).
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Encoded Opus frame or mock PCM payload.
    pub data: Vec<u8>,
}

/// Application DataChannel message (input events, identity challenge, control).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataMessage {
    /// Logical channel label (`"input"`, `"control"`, …).
    pub label: String,
    /// Raw payload bytes (JSON input events for `"input"`).
    pub data: Vec<u8>,
    /// When true, message was sent/received on an unordered / partial-reliability channel.
    pub unordered: bool,
}

/// Kind of media track for receive callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// H.264 video.
    Video,
    /// Opus audio.
    Audio,
}

/// Received media unit delivered to `on_track` style callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingTrackData {
    /// De-packetized (or mock-forwarded) video NAL batch.
    Video(VideoNalu),
    /// De-packetized (or mock-forwarded) audio packet.
    Audio(AudioPacket),
}

/// Outbound local ICE candidate that the application must signal to the remote peer.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalIceCandidate {
    /// Candidate object (protocol shape).
    pub candidate: IceCandidate,
}

/// Receiver → sender feedback for external encoder control (GCC / RTCP).
///
/// Host session agent maps this to encoder bitrate and keyframe requests
/// (KD5: PLI/FIR/GCC in-process with capture). Mock never emits unless tests
/// inject via [`crate::mock::MockPeerTransport::inject_receiver_feedback`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceiverFeedback {
    /// Picture Loss Indication — request keyframe.
    pub pli: bool,
    /// Full Intra Request — request keyframe (FIR).
    pub fir: bool,
    /// Count of NACK sequence numbers in this report (0 if none).
    pub nack_count: u32,
    /// Estimated target media bitrate from GCC / transport-cc, if known.
    pub target_bitrate_bps: Option<u32>,
}
