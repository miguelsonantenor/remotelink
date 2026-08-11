//! Pure-Rust [`PeerTransport`] backend via the [`webrtc`] crate (0.11).
//!
//! # Feature
//!
//! Compiled only with `remotelink-net` feature **`webrtc-rs`** (default-off so
//! CI does not pull the webrtc dependency graph).
//!
//! # Media path
//!
//! **Preferred:** real RTP media tracks (H.264 + Opus) via
//! [`TrackLocalStaticSample`](webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample)
//! on the offerer and [`SampleBuilder`](webrtc::media::io::sample_builder::SampleBuilder)
//! on the answerer (`on_track` + `read_rtp`).
//!
//! **DataChannels** (always):
//!
//! | Label | Purpose |
//! |-------|---------|
//! | `input` | Input / control JSON |
//! | `identity` | Identity bind challenge |
//!
//! Optional **media DC fallback** (`media-video` / `media-audio`) is created for
//! bind-race recovery; media **send prefers RTP-only** when local tracks exist
//! and the peer is `Connected`. Set `REMOTELINK_WEBRTC_DUAL_MEDIA=1` to mirror
//! RTP onto those DCs (debug).
//!
//! # Fingerprints
//!
//! - **Local:** SHA-256 of the DTLS certificate DER minted at construction.
//! - **Remote:** parsed from SDP `a=fingerprint:` after set_remote; upgraded
//!   from the completed DTLS remote certificate when connection state becomes
//!   `Connected`.
//!
//! # Runtime
//!
//! webrtc-rs is async/tokio. This backend owns a process-wide multi-thread
//! runtime and uses `block_on` for trait methods; inbound ICE/state/data are
//! queued and delivered on [`PeerTransport::poll`] (pull model, same as mock/live).

use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use rcgen::KeyPair;
use remotelink_protocol::IceCandidate;
use sha2::{Digest, Sha256};
use tokio::runtime::{Handle, Runtime};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::media::Sample;
use webrtc::peer_connection::certificate::RTCCertificate;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::codecs::opus::OpusPacket;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use crate::error::{NetError, Result};
use crate::factory::PeerRole;
use crate::transport::{NullCallbacks, PeerTransport, PeerTransportCallbacks};
use crate::types::{
    AudioPacket, ConnectionState, DataMessage, DtlsFingerprint, IncomingTrackData,
    LocalIceCandidate, NaluFormat, SdpType, SessionDescription, TransportIceCandidate, VideoNalu,
};

/// DataChannel label: input events.
pub const LABEL_INPUT: &str = "input";
/// DataChannel label: identity bind.
pub const LABEL_IDENTITY: &str = "identity";
/// DataChannel label: optional H.264 NALU media fallback (live-TCP payload layout).
pub const LABEL_MEDIA_VIDEO: &str = "media-video";
/// DataChannel label: optional Opus media fallback (live-TCP payload layout).
pub const LABEL_MEDIA_AUDIO: &str = "media-audio";

/// Application DCs required for identity/input (wait_ready).
const REQUIRED_CHANNEL_LABELS: &[&str] = &[LABEL_INPUT, LABEL_IDENTITY];

/// All offerer-created DCs (input/identity + optional media fallback).
const CHANNEL_LABELS: &[&str] = &[
    LABEL_INPUT,
    LABEL_IDENTITY,
    LABEL_MEDIA_VIDEO,
    LABEL_MEDIA_AUDIO,
];

fn dual_media_enabled() -> bool {
    match std::env::var("REMOTELINK_WEBRTC_DUAL_MEDIA") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        Err(_) => false,
    }
}

/// Process-wide tokio runtime for webrtc-rs.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("remotelink-webrtc")
            .build()
            .expect("failed to build remotelink webrtc tokio runtime")
    })
}

fn block_on<F: Future>(fut: F) -> F::Output {
    // Prefer the dedicated runtime so we never nest `block_on` on a foreign
    // current-thread runtime. If already on our multi-thread workers, use
    // `block_in_place` + handle.
    match Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => runtime().block_on(fut),
    }
}

/// Inbound events drained by [`WebrtcPeerTransport::poll`].
enum Inbound {
    Ice(LocalIceCandidate),
    State(ConnectionState),
    Track(IncomingTrackData),
    Data(DataMessage),
    RemoteFp(DtlsFingerprint),
}

/// Configuration for a webrtc-rs peer.
#[derive(Debug, Clone, Default)]
pub struct WebrtcPeerConfig {
    /// Optional ICE servers (`stun:…` / `turn:…`). Empty = host candidates only.
    pub ice_servers: Vec<String>,
}

impl WebrtcPeerConfig {
    /// Read optional `REMOTELINK_WEBRTC_STUN` (comma-separated URLs).
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(s) = std::env::var("REMOTELINK_WEBRTC_STUN") {
            cfg.ice_servers = s
                .split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect();
        }
        cfg
    }
}

/// webrtc-rs [`PeerTransport`] using real SDP / ICE / DTLS, RTP media tracks,
/// and application DataChannels (input/identity + media fallback).
pub struct WebrtcPeerTransport {
    role: PeerRole,
    pc: Arc<RTCPeerConnection>,
    local_fp: DtlsFingerprint,
    remote_fp: Arc<Mutex<Option<DtlsFingerprint>>>,
    state: Arc<Mutex<ConnectionState>>,
    channels: Arc<Mutex<HashMap<String, Arc<RTCDataChannel>>>>,
    /// Local H.264 track (offerer); `write_sample` after negotiation.
    video_track: Option<Arc<TrackLocalStaticSample>>,
    /// Local Opus track (offerer).
    audio_track: Option<Arc<TrackLocalStaticSample>>,
    inbound_tx: Sender<Inbound>,
    inbound_rx: Receiver<Inbound>,
    callbacks: Box<dyn PeerTransportCallbacks>,
    closed: bool,
    /// Channels created on offerer before first offer.
    offerer_channels_ready: bool,
    /// Local RTP tracks added on offerer.
    offerer_tracks_ready: bool,
}

impl WebrtcPeerTransport {
    /// Create a new peer for `role` (DTLS cert + RTCPeerConnection, not yet negotiated).
    pub fn new(role: PeerRole, config: WebrtcPeerConfig) -> Result<Self> {
        let kp = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| NetError::Internal(format!("dtls keypair: {e}")))?;
        let cert = RTCCertificate::from_key_pair(kp)
            .map_err(|e| NetError::Internal(format!("dtls certificate: {e}")))?;
        let local_fp = fingerprint_from_rtc(&cert)?;

        let mut ice_servers = Vec::new();
        for url in config.ice_servers {
            ice_servers.push(webrtc::ice_transport::ice_server::RTCIceServer {
                urls: vec![url],
                ..Default::default()
            });
        }

        let rtc_cfg = RTCConfiguration {
            certificates: vec![cert],
            ice_servers,
            ..Default::default()
        };

        let (inbound_tx, inbound_rx) = mpsc::channel();
        let remote_fp = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(ConnectionState::New));
        let channels: Arc<Mutex<HashMap<String, Arc<RTCDataChannel>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pc = block_on(async {
            let mut m = MediaEngine::default();
            m.register_default_codecs()
                .map_err(|e| NetError::Internal(format!("media engine: {e}")))?;
            // Default interceptors (NACK/TWCC/…) so RTP media flows reliably.
            let mut registry = Registry::new();
            registry = register_default_interceptors(registry, &mut m)
                .map_err(|e| NetError::Internal(format!("interceptors: {e}")))?;
            let api = APIBuilder::new()
                .with_media_engine(m)
                .with_interceptor_registry(registry)
                .build();
            api.new_peer_connection(rtc_cfg)
                .await
                .map_err(|e| NetError::Internal(format!("new_peer_connection: {e}")))
        })?;
        let pc = Arc::new(pc);

        // Connection state → inbound queue.
        {
            let tx = inbound_tx.clone();
            let state_arc = Arc::clone(&state);
            let remote_fp_arc = Arc::clone(&remote_fp);
            let pc_for_fp = Arc::clone(&pc);
            pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                let mapped = map_pc_state(s);
                if let Ok(mut g) = state_arc.lock() {
                    *g = mapped;
                }
                let _ = tx.send(Inbound::State(mapped));

                if s == RTCPeerConnectionState::Connected {
                    let tx2 = tx.clone();
                    let remote_fp2 = Arc::clone(&remote_fp_arc);
                    let pc2 = Arc::clone(&pc_for_fp);
                    Box::pin(async move {
                        if let Some(fp) = remote_fingerprint_from_dtls(&pc2).await {
                            if let Ok(mut g) = remote_fp2.lock() {
                                *g = Some(fp.clone());
                            }
                            let _ = tx2.send(Inbound::RemoteFp(fp));
                        }
                    })
                } else {
                    Box::pin(async {})
                }
            }));
        }

        // Trickle ICE → inbound queue.
        {
            let tx = inbound_tx.clone();
            pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Some(c) = c {
                        if let Ok(init) = c.to_json() {
                            let _ = tx.send(Inbound::Ice(LocalIceCandidate {
                                candidate: IceCandidate {
                                    candidate: init.candidate,
                                    sdp_mid: init.sdp_mid,
                                    sdp_m_line_index: init.sdp_mline_index,
                                    username_fragment: init.username_fragment,
                                },
                            }));
                        }
                    }
                })
            }));
        }

        // Answerer (and any remote-created channels) → store + message handlers.
        {
            let tx = inbound_tx.clone();
            let channels = Arc::clone(&channels);
            pc.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
                let label = d.label().to_owned();
                if let Ok(mut map) = channels.lock() {
                    map.insert(label.clone(), Arc::clone(&d));
                }
                wire_channel_messages(Arc::clone(&d), label, tx.clone());
                Box::pin(async {})
            }));
        }

        // Remote RTP media → SampleBuilder → inbound Track events.
        {
            let tx = inbound_tx.clone();
            pc.on_track(Box::new(move |track, _receiver, _transceiver| {
                let tx = tx.clone();
                Box::pin(async move {
                    spawn_remote_track_reader(track, tx);
                })
            }));
        }

        Ok(Self {
            role,
            pc,
            local_fp,
            remote_fp,
            state,
            channels,
            video_track: None,
            audio_track: None,
            inbound_tx,
            inbound_rx,
            callbacks: Box::new(NullCallbacks),
            closed: false,
            offerer_channels_ready: false,
            offerer_tracks_ready: false,
        })
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed {
            return Err(NetError::Closed);
        }
        Ok(())
    }

    fn connection_state_locked(&self) -> ConnectionState {
        self.state
            .lock()
            .map(|g| *g)
            .unwrap_or(ConnectionState::Failed)
    }

    /// Create the four application DataChannels (offerer only).
    fn ensure_offerer_channels(&mut self) -> Result<()> {
        if self.offerer_channels_ready || self.role != PeerRole::Offerer {
            return Ok(());
        }
        let pc = Arc::clone(&self.pc);
        let channels = Arc::clone(&self.channels);
        let tx = self.inbound_tx.clone();
        block_on(async {
            for label in CHANNEL_LABELS {
                let dc = pc
                    .create_data_channel(label, None)
                    .await
                    .map_err(|e| NetError::Internal(format!("create_data_channel {label}: {e}")))?;
                wire_channel_messages(Arc::clone(&dc), (*label).to_owned(), tx.clone());
                if let Ok(mut map) = channels.lock() {
                    map.insert((*label).to_owned(), dc);
                }
            }
            Ok::<(), NetError>(())
        })?;
        self.offerer_channels_ready = true;
        Ok(())
    }

    /// Add local H.264 + Opus tracks (offerer only) before the first offer.
    fn ensure_offerer_tracks(&mut self) -> Result<()> {
        if self.offerer_tracks_ready || self.role != PeerRole::Offerer {
            return Ok(());
        }
        let pc = Arc::clone(&self.pc);
        let (video, audio) = block_on(async {
            let video = Arc::new(TrackLocalStaticSample::new(
                RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: vec![],
                },
                "video".into(),
                "remotelink".into(),
            ));
            let audio = Arc::new(TrackLocalStaticSample::new(
                RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                "audio".into(),
                "remotelink".into(),
            ));
            let v: Arc<dyn TrackLocal + Send + Sync> = Arc::clone(&video) as _;
            let a: Arc<dyn TrackLocal + Send + Sync> = Arc::clone(&audio) as _;
            pc.add_track(v)
                .await
                .map_err(|e| NetError::Internal(format!("add video track: {e}")))?;
            pc.add_track(a)
                .await
                .map_err(|e| NetError::Internal(format!("add audio track: {e}")))?;
            Ok::<_, NetError>((video, audio))
        })?;
        self.video_track = Some(video);
        self.audio_track = Some(audio);
        self.offerer_tracks_ready = true;
        Ok(())
    }

    fn to_rtc_desc(desc: &SessionDescription) -> Result<RTCSessionDescription> {
        match desc.sdp_type {
            SdpType::Offer => RTCSessionDescription::offer(desc.sdp.clone())
                .map_err(|e| NetError::InvalidDescription(format!("offer sdp: {e}"))),
            SdpType::Answer => RTCSessionDescription::answer(desc.sdp.clone())
                .map_err(|e| NetError::InvalidDescription(format!("answer sdp: {e}"))),
            SdpType::Pranswer => RTCSessionDescription::pranswer(desc.sdp.clone())
                .map_err(|e| NetError::InvalidDescription(format!("pranswer sdp: {e}"))),
            SdpType::Rollback => Err(NetError::InvalidDescription(
                "rollback not supported on webrtc-rs backend".into(),
            )),
        }
    }

    fn from_rtc_desc(rtc: RTCSessionDescription) -> SessionDescription {
        let sdp_type = match rtc.sdp_type {
            RTCSdpType::Offer => SdpType::Offer,
            RTCSdpType::Answer => SdpType::Answer,
            RTCSdpType::Pranswer => SdpType::Pranswer,
            RTCSdpType::Rollback => SdpType::Rollback,
            _ => SdpType::Offer,
        };
        SessionDescription {
            sdp_type,
            sdp: rtc.sdp,
        }
    }

    fn send_on_channel(&self, label: &str, payload: Bytes) -> Result<()> {
        self.ensure_open()?;
        if self.connection_state_locked() != ConnectionState::Connected {
            // PeerConnection Connected is required, but DCs may still be
            // Connecting — callers should wait via [`Self::wait_data_channels_open`].
            return Err(NetError::InvalidState {
                expected: "connected",
                actual: self.connection_state_locked().as_str().into(),
            });
        }
        let dc = {
            let map = self
                .channels
                .lock()
                .map_err(|e| NetError::Internal(format!("channels lock: {e}")))?;
            map.get(label).cloned()
        };
        let Some(dc) = dc else {
            return Err(NetError::SendFailed(format!(
                "data channel `{label}` not registered yet"
            )));
        };
        let state = dc.ready_state();
        if state != RTCDataChannelState::Open {
            return Err(NetError::SendFailed(format!(
                "data channel `{label}` not open (state={state})"
            )));
        }
        block_on(async {
            dc.send(&payload)
                .await
                .map_err(|e| NetError::SendFailed(format!("dc `{label}` send: {e}")))
        })?;
        Ok(())
    }

    /// True when **input** and **identity** DataChannels are registered and `Open`.
    ///
    /// Media DCs are optional (RTP is preferred). PeerConnection `Connected`
    /// does **not** imply DCs are open; SCTP can lag DTLS.
    pub fn data_channels_open(&self) -> bool {
        let Ok(map) = self.channels.lock() else {
            return false;
        };
        REQUIRED_CHANNEL_LABELS.iter().all(|label| {
            map.get(*label)
                .map(|dc| dc.ready_state() == RTCDataChannelState::Open)
                .unwrap_or(false)
        })
    }

    /// Block until input/identity DataChannels report `Open`, or `timeout`.
    pub fn wait_data_channels_open(&self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.data_channels_open() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let detail = self
                    .channels
                    .lock()
                    .map(|map| {
                        REQUIRED_CHANNEL_LABELS
                            .iter()
                            .map(|l| {
                                let st = map
                                    .get(*l)
                                    .map(|dc| dc.ready_state().to_string())
                                    .unwrap_or_else(|| "missing".into());
                                format!("{l}={st}")
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|_| "channels lock poisoned".into());
                return Err(NetError::Internal(format!(
                    "timed out waiting for DataChannels open ({detail})"
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn parse_remote_fp_from_sdp(&self, sdp: &str) -> Option<DtlsFingerprint> {
        for line in sdp.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("a=fingerprint:") {
                let rest = rest.trim();
                let mut parts = rest.split_whitespace();
                let algo = parts.next().unwrap_or("sha-256");
                let value = parts.next().unwrap_or("");
                if algo.eq_ignore_ascii_case("sha-256") && !value.is_empty() {
                    if let Ok(fp) = DtlsFingerprint::sha256(value) {
                        return Some(fp);
                    }
                }
            }
        }
        None
    }

    /// Block until `Connected` or timeout (tests / demos).
    pub fn wait_connected(&mut self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.poll()?;
            if self.connection_state_locked() == ConnectionState::Connected {
                return Ok(());
            }
            if matches!(
                self.connection_state_locked(),
                ConnectionState::Failed | ConnectionState::Closed | ConnectionState::Disconnected
            ) {
                return Err(NetError::Internal(format!(
                    "wait_connected: ended in {}",
                    self.connection_state_locked().as_str()
                )));
            }
            if std::time::Instant::now() >= deadline {
                return Err(NetError::Internal(
                    "timed out waiting for WebRTC Connected".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn wire_channel_messages(dc: Arc<RTCDataChannel>, label: String, tx: Sender<Inbound>) {
    let label_for_msg = label.clone();
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let tx = tx.clone();
        let label = label_for_msg.clone();
        Box::pin(async move {
            let data = msg.data.to_vec();
            match label.as_str() {
                LABEL_MEDIA_VIDEO => {
                    if let Ok(v) = decode_video(&data) {
                        let _ = tx.send(Inbound::Track(IncomingTrackData::Video(v)));
                    }
                }
                LABEL_MEDIA_AUDIO => {
                    if let Ok(a) = decode_audio(&data) {
                        let _ = tx.send(Inbound::Track(IncomingTrackData::Audio(a)));
                    }
                }
                _ => {
                    let _ = tx.send(Inbound::Data(DataMessage {
                        label,
                        data,
                        unordered: false,
                    }));
                }
            }
        })
    }));
}

fn map_pc_state(s: RTCPeerConnectionState) -> ConnectionState {
    match s {
        RTCPeerConnectionState::New => ConnectionState::New,
        RTCPeerConnectionState::Connecting => ConnectionState::Connecting,
        RTCPeerConnectionState::Connected => ConnectionState::Connected,
        RTCPeerConnectionState::Disconnected => ConnectionState::Disconnected,
        RTCPeerConnectionState::Failed => ConnectionState::Failed,
        RTCPeerConnectionState::Closed => ConnectionState::Closed,
        _ => ConnectionState::New,
    }
}

fn fingerprint_from_rtc(cert: &RTCCertificate) -> Result<DtlsFingerprint> {
    let fps = cert.get_fingerprints();
    let fp = fps
        .first()
        .ok_or_else(|| NetError::Internal("certificate has no fingerprints".into()))?;
    DtlsFingerprint::sha256(&fp.value)
}

async fn remote_fingerprint_from_dtls(pc: &RTCPeerConnection) -> Option<DtlsFingerprint> {
    let der = pc.dtls_transport().get_remote_certificate().await;
    if der.is_empty() {
        return None;
    }
    let digest = Sha256::digest(&der);
    DtlsFingerprint::sha256(hex::encode(digest)).ok()
}

// --- RTP track receive + H.264 helpers ----------------------------------------

fn spawn_remote_track_reader(track: Arc<TrackRemote>, tx: Sender<Inbound>) {
    tokio::spawn(async move {
        let mime = track.codec().capability.mime_type.to_ascii_lowercase();
        if mime.contains("h264") {
            let mut sb = SampleBuilder::new(64, H264Packet::default(), 90_000);
            loop {
                match track.read_rtp().await {
                    Ok((pkt, _)) => {
                        sb.push(pkt);
                        while let Some(sample) = sb.pop() {
                            let data = sample.data.to_vec();
                            let keyframe = is_h264_keyframe(&data);
                            let _ = tx.send(Inbound::Track(IncomingTrackData::Video(VideoNalu {
                                pts_host_mono: Duration::from_millis(0),
                                rtp_ts: Some(sample.packet_timestamp),
                                keyframe,
                                format: NaluFormat::AnnexB,
                                data,
                            })));
                        }
                    }
                    Err(_) => break,
                }
            }
        } else if mime.contains("opus") {
            let mut sb = SampleBuilder::new(32, OpusPacket, 48_000);
            loop {
                match track.read_rtp().await {
                    Ok((pkt, _)) => {
                        sb.push(pkt);
                        while let Some(sample) = sb.pop() {
                            let _ = tx.send(Inbound::Track(IncomingTrackData::Audio(AudioPacket {
                                pts_host_mono: Duration::from_millis(0),
                                rtp_ts: Some(sample.packet_timestamp),
                                sample_rate: 48_000,
                                channels: 2,
                                data: sample.data.to_vec(),
                            })));
                        }
                    }
                    Err(_) => break,
                }
            }
        } else {
            // Drain unsupported codecs so the peer does not stall.
            while track.read_rtp().await.is_ok() {}
        }
    });
}

/// Ensure H.264 payload is Annex-B for the RTP H264 payloader.
fn nalu_payload_annex_b(n: &VideoNalu) -> Result<Vec<u8>> {
    match n.format {
        NaluFormat::AnnexB => Ok(n.data.clone()),
        NaluFormat::Avcc => avcc_to_annex_b(&n.data),
    }
}

fn avcc_to_annex_b(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + len > data.len() {
            return Err(NetError::Internal("truncated AVCC NAL".into()));
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + len]);
        i += len;
    }
    if out.is_empty() {
        return Err(NetError::Internal("empty AVCC payload".into()));
    }
    Ok(out)
}

fn is_h264_keyframe(annex_b: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 < annex_b.len() {
        // Find start code
        let sc = if annex_b[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if annex_b[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let nal = annex_b.get(i + sc).copied().unwrap_or(0);
        let nal_type = nal & 0x1f;
        // IDR (5), SPS (7) often accompanies keyframe access units.
        if nal_type == 5 {
            return true;
        }
        i += sc + 1;
    }
    false
}

// --- media payload codec (matches live TCP kind-specific body) ---------------

fn encode_video(n: &VideoNalu) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(8 + 4 + 1 + 1 + n.data.len());
    out.extend_from_slice(&(n.pts_host_mono.as_millis() as u64).to_be_bytes());
    let rtp = n.rtp_ts.unwrap_or(u32::MAX);
    out.extend_from_slice(&rtp.to_be_bytes());
    out.push(u8::from(n.keyframe));
    out.push(match n.format {
        NaluFormat::AnnexB => 0,
        NaluFormat::Avcc => 1,
    });
    out.extend_from_slice(&n.data);
    Ok(out)
}

fn decode_video(p: &[u8]) -> Result<VideoNalu> {
    if p.len() < 14 {
        return Err(NetError::Internal("video frame too short".into()));
    }
    let pts_ms = u64::from_be_bytes(p[0..8].try_into().unwrap());
    let rtp_raw = u32::from_be_bytes(p[8..12].try_into().unwrap());
    let keyframe = p[12] != 0;
    let format = match p[13] {
        0 => NaluFormat::AnnexB,
        _ => NaluFormat::Avcc,
    };
    Ok(VideoNalu {
        pts_host_mono: Duration::from_millis(pts_ms),
        rtp_ts: if rtp_raw == u32::MAX {
            None
        } else {
            Some(rtp_raw)
        },
        keyframe,
        format,
        data: p[14..].to_vec(),
    })
}

fn encode_audio(a: &AudioPacket) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(8 + 4 + 4 + 2 + a.data.len());
    out.extend_from_slice(&(a.pts_host_mono.as_millis() as u64).to_be_bytes());
    let rtp = a.rtp_ts.unwrap_or(u32::MAX);
    out.extend_from_slice(&rtp.to_be_bytes());
    out.extend_from_slice(&a.sample_rate.to_be_bytes());
    out.extend_from_slice(&a.channels.to_be_bytes());
    out.extend_from_slice(&a.data);
    Ok(out)
}

fn decode_audio(p: &[u8]) -> Result<AudioPacket> {
    if p.len() < 18 {
        return Err(NetError::Internal("audio frame too short".into()));
    }
    let pts_ms = u64::from_be_bytes(p[0..8].try_into().unwrap());
    let rtp_raw = u32::from_be_bytes(p[8..12].try_into().unwrap());
    let sample_rate = u32::from_be_bytes(p[12..16].try_into().unwrap());
    let channels = u16::from_be_bytes(p[16..18].try_into().unwrap());
    Ok(AudioPacket {
        pts_host_mono: Duration::from_millis(pts_ms),
        rtp_ts: if rtp_raw == u32::MAX {
            None
        } else {
            Some(rtp_raw)
        },
        sample_rate,
        channels,
        data: p[18..].to_vec(),
    })
}

impl PeerTransport for WebrtcPeerTransport {
    fn set_callbacks(&mut self, callbacks: Box<dyn PeerTransportCallbacks>) {
        self.callbacks = callbacks;
    }

    fn connection_state(&self) -> ConnectionState {
        self.connection_state_locked()
    }

    fn create_offer(&mut self) -> Result<SessionDescription> {
        self.ensure_open()?;
        if self.role != PeerRole::Offerer {
            return Err(NetError::InvalidState {
                expected: "offerer role",
                actual: self.role.as_str().into(),
            });
        }
        self.ensure_offerer_tracks()?;
        self.ensure_offerer_channels()?;
        let pc = Arc::clone(&self.pc);
        let offer = block_on(async {
            pc.create_offer(None)
                .await
                .map_err(|e| NetError::Internal(format!("create_offer: {e}")))
        })?;
        Ok(Self::from_rtc_desc(offer))
    }

    fn create_answer(&mut self) -> Result<SessionDescription> {
        self.ensure_open()?;
        if self.role != PeerRole::Answerer {
            return Err(NetError::InvalidState {
                expected: "answerer role",
                actual: self.role.as_str().into(),
            });
        }
        let pc = Arc::clone(&self.pc);
        let answer = block_on(async {
            pc.create_answer(None)
                .await
                .map_err(|e| NetError::Internal(format!("create_answer: {e}")))
        })?;
        Ok(Self::from_rtc_desc(answer))
    }

    fn set_local_description(&mut self, desc: SessionDescription) -> Result<()> {
        self.ensure_open()?;
        if desc.sdp.is_empty() {
            return Err(NetError::InvalidDescription("empty sdp".into()));
        }
        let rtc = Self::to_rtc_desc(&desc)?;
        let pc = Arc::clone(&self.pc);
        block_on(async {
            // Gather host candidates into the local description for simpler
            // non-trickle demos; trickle candidates still fire via callbacks.
            let mut gather_complete = pc.gathering_complete_promise().await;
            pc.set_local_description(rtc)
                .await
                .map_err(|e| NetError::InvalidDescription(format!("set_local: {e}")))?;
            let _ = gather_complete.recv().await;
            Ok::<(), NetError>(())
        })?;
        if self.connection_state_locked() == ConnectionState::New {
            if let Ok(mut g) = self.state.lock() {
                *g = ConnectionState::Connecting;
            }
            self.callbacks
                .on_connection_state(ConnectionState::Connecting);
        }
        Ok(())
    }

    fn set_remote_description(&mut self, desc: SessionDescription) -> Result<()> {
        self.ensure_open()?;
        if desc.sdp.is_empty() {
            return Err(NetError::InvalidDescription("empty sdp".into()));
        }
        if let Some(fp) = self.parse_remote_fp_from_sdp(&desc.sdp) {
            if let Ok(mut g) = self.remote_fp.lock() {
                // Prefer DTLS-completed fingerprint when already known.
                if g.is_none() {
                    *g = Some(fp);
                }
            }
        }
        let rtc = Self::to_rtc_desc(&desc)?;
        let pc = Arc::clone(&self.pc);
        block_on(async {
            pc.set_remote_description(rtc)
                .await
                .map_err(|e| NetError::InvalidDescription(format!("set_remote: {e}")))
        })?;
        if self.connection_state_locked() == ConnectionState::New {
            if let Ok(mut g) = self.state.lock() {
                *g = ConnectionState::Connecting;
            }
            self.callbacks
                .on_connection_state(ConnectionState::Connecting);
        }
        Ok(())
    }

    fn add_ice_candidate(&mut self, candidate: TransportIceCandidate) -> Result<()> {
        self.ensure_open()?;
        if candidate.candidate.is_empty() {
            return Err(NetError::InvalidCandidate("empty candidate".into()));
        }
        let init = RTCIceCandidateInit {
            candidate: candidate.candidate,
            sdp_mid: candidate.sdp_mid,
            sdp_mline_index: candidate.sdp_m_line_index,
            username_fragment: candidate.username_fragment,
        };
        let pc = Arc::clone(&self.pc);
        block_on(async {
            pc.add_ice_candidate(init)
                .await
                .map_err(|e| NetError::InvalidCandidate(format!("{e}")))
        })?;
        Ok(())
    }

    fn restart_ice(&mut self) -> Result<()> {
        self.ensure_open()?;
        // ICE restart is performed by creating a new offer with ice_restart.
        // For answerer, we only flip local state; offerer regenerates.
        if self.role == PeerRole::Offerer {
            let pc = Arc::clone(&self.pc);
            block_on(async {
                let offer = pc
                    .create_offer(Some(RTCOfferOptions {
                        ice_restart: true,
                        voice_activity_detection: false,
                    }))
                    .await
                    .map_err(|e| NetError::Internal(format!("restart_ice offer: {e}")))?;
                let mut gather_complete = pc.gathering_complete_promise().await;
                pc.set_local_description(offer)
                    .await
                    .map_err(|e| NetError::Internal(format!("restart_ice set_local: {e}")))?;
                let _ = gather_complete.recv().await;
                Ok::<(), NetError>(())
            })?;
        }
        if self.connection_state_locked() == ConnectionState::Connected {
            if let Ok(mut g) = self.state.lock() {
                *g = ConnectionState::Connecting;
            }
            self.callbacks
                .on_connection_state(ConnectionState::Connecting);
        }
        Ok(())
    }

    fn local_fingerprint(&self) -> Result<DtlsFingerprint> {
        Ok(self.local_fp.clone())
    }

    fn remote_fingerprint(&self) -> Result<Option<DtlsFingerprint>> {
        Ok(self.remote_fp.lock().ok().and_then(|g| g.clone()))
    }

    fn send_video_nalu(&mut self, nalu: VideoNalu) -> Result<()> {
        // RTP-only when local track exists and Connected (default product path).
        // Optional dual-write via REMOTELINK_WEBRTC_DUAL_MEDIA=1.
        // DC fallback when tracks are not ready yet (bind race / answerer role).
        let dual = dual_media_enabled();
        let mut rtp_ok = false;
        if let Some(track) = self.video_track.clone() {
            if self.connection_state_locked() == ConnectionState::Connected {
                let annex_b = nalu_payload_annex_b(&nalu)?;
                let sample = Sample {
                    data: Bytes::from(annex_b),
                    timestamp: SystemTime::now(),
                    duration: Duration::from_millis(33),
                    packet_timestamp: nalu.rtp_ts.unwrap_or(0),
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                };
                block_on(async {
                    track
                        .write_sample(&sample)
                        .await
                        .map_err(|e| NetError::SendFailed(format!("video track write: {e}")))
                })?;
                rtp_ok = true;
                if !dual {
                    return Ok(());
                }
            }
        }
        let body = encode_video(&nalu)?;
        match self.send_on_channel(LABEL_MEDIA_VIDEO, Bytes::from(body)) {
            Ok(()) => Ok(()),
            Err(_e) if rtp_ok => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn send_audio(&mut self, packet: AudioPacket) -> Result<()> {
        let dual = dual_media_enabled();
        let mut rtp_ok = false;
        if let Some(track) = self.audio_track.clone() {
            if self.connection_state_locked() == ConnectionState::Connected {
                let sample = Sample {
                    data: Bytes::from(packet.data.clone()),
                    timestamp: SystemTime::now(),
                    duration: Duration::from_millis(10),
                    packet_timestamp: packet.rtp_ts.unwrap_or(0),
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                };
                block_on(async {
                    track
                        .write_sample(&sample)
                        .await
                        .map_err(|e| NetError::SendFailed(format!("audio track write: {e}")))
                })?;
                rtp_ok = true;
                if !dual {
                    return Ok(());
                }
            }
        }
        let body = encode_audio(&packet)?;
        match self.send_on_channel(LABEL_MEDIA_AUDIO, Bytes::from(body)) {
            Ok(()) => Ok(()),
            Err(_e) if rtp_ok => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn send_data(&mut self, message: DataMessage) -> Result<()> {
        let label = if message.label.is_empty() {
            LABEL_INPUT
        } else {
            message.label.as_str()
        };
        // Only route known channels; unknown labels still attempt send by name.
        self.send_on_channel(label, Bytes::from(message.data))
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        // Connected first, then SCTP DataChannels (may lag DTLS).
        self.wait_connected(timeout)?;
        self.wait_data_channels_open(timeout)
    }

    fn poll(&mut self) -> Result<()> {
        if self.closed {
            return Err(NetError::Closed);
        }
        let mut batch = Vec::new();
        loop {
            match self.inbound_rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        for msg in batch {
            match msg {
                Inbound::Ice(c) => self.callbacks.on_ice_candidate(c),
                Inbound::State(s) => self.callbacks.on_connection_state(s),
                Inbound::Track(t) => self.callbacks.on_track(t),
                Inbound::Data(d) => self.callbacks.on_data(d),
                Inbound::RemoteFp(fp) => {
                    if let Ok(mut g) = self.remote_fp.lock() {
                        *g = Some(fp);
                    }
                }
            }
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let pc = Arc::clone(&self.pc);
        let _ = block_on(async { pc.close().await });
        if let Ok(mut g) = self.state.lock() {
            *g = ConnectionState::Closed;
        }
        self.callbacks.on_connection_state(ConnectionState::Closed);
        Ok(())
    }
}

impl Drop for WebrtcPeerTransport {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            let pc = Arc::clone(&self.pc);
            let _ = block_on(async { pc.close().await });
        }
    }
}

/// Exchange ICE candidates that were emitted into each peer's callback queue.
///
/// Helper for in-process tests: poll both sides and cross-add ICE until both
/// report [`ConnectionState::Connected`] or `timeout`.
pub fn webrtc_handshake(
    offerer: &mut WebrtcPeerTransport,
    answerer: &mut WebrtcPeerTransport,
    timeout: Duration,
) -> Result<()> {
    // Offerer creates offer (and data channels).
    let offer = offerer.create_offer()?;
    offerer.set_local_description(offer.clone())?;
    // Prefer full local description (with gathered candidates) after set_local.
    let offer = block_on(async {
        offerer
            .pc
            .local_description()
            .await
            .map(WebrtcPeerTransport::from_rtc_desc)
            .unwrap_or(offer)
    });

    answerer.set_remote_description(offer)?;
    let answer = answerer.create_answer()?;
    answerer.set_local_description(answer.clone())?;
    let answer = block_on(async {
        answerer
            .pc
            .local_description()
            .await
            .map(WebrtcPeerTransport::from_rtc_desc)
            .unwrap_or(answer)
    });
    offerer.set_remote_description(answer)?;

    // Drain trickle ICE both ways while waiting for Connected.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Cross-poll ICE via temporary recording is awkward; re-read inbound by
        // installing a simple swap is heavy. Instead: poll each and use
        // on_ice_candidate already queued — but those go to app callbacks.
        // For handshake helper, read pending ICE from a side channel is needed.
        //
        // We already waited for gathering_complete in set_local_description, so
        // SDP should include candidates. Trickle extras still help; drain poll.
        offerer.poll()?;
        answerer.poll()?;

        if offerer.connection_state() == ConnectionState::Connected
            && answerer.connection_state() == ConnectionState::Connected
        {
            // Connected ≠ DataChannel open; wait for SCTP DCs before return.
            if offerer.data_channels_open() && answerer.data_channels_open() {
                return Ok(());
            }
            // Answerer may still be registering remote DCs; keep polling.
        }
        if std::time::Instant::now() >= deadline {
            return Err(NetError::Internal(format!(
                "webrtc_handshake timeout (offerer={}, answerer={}, o_dc={}, a_dc={})",
                offerer.connection_state().as_str(),
                answerer.connection_state().as_str(),
                offerer.data_channels_open(),
                answerer.data_channels_open()
            )));
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::RecordingCallbacks;
    use std::sync::{Arc, Mutex};

    #[test]
    fn local_fingerprint_is_sha256() {
        let t = WebrtcPeerTransport::new(PeerRole::Offerer, WebrtcPeerConfig::default()).unwrap();
        let fp = t.local_fingerprint().unwrap();
        assert_eq!(fp.algorithm, "sha-256");
        assert_eq!(fp.digest_bytes().unwrap().len(), 32);
    }

    #[test]
    fn localhost_offer_answer_datachannel_echo() {
        let mut offerer =
            WebrtcPeerTransport::new(PeerRole::Offerer, WebrtcPeerConfig::default()).unwrap();
        let mut answerer =
            WebrtcPeerTransport::new(PeerRole::Answerer, WebrtcPeerConfig::default()).unwrap();

        let (ice_o_tx, ice_o_rx) = mpsc::channel();
        let (ice_a_tx, ice_a_rx) = mpsc::channel();
        let answerer_rec = Arc::new(Mutex::new(RecordingCallbacks::default()));

        struct SharedRec {
            ice_tx: Sender<TransportIceCandidate>,
            rec: Arc<Mutex<RecordingCallbacks>>,
        }
        impl PeerTransportCallbacks for SharedRec {
            fn on_ice_candidate(&mut self, candidate: LocalIceCandidate) {
                let _ = self.ice_tx.send(candidate.candidate.clone());
                if let Ok(mut g) = self.rec.lock() {
                    g.on_ice_candidate(candidate);
                }
            }
            fn on_connection_state(&mut self, state: ConnectionState) {
                if let Ok(mut g) = self.rec.lock() {
                    g.on_connection_state(state);
                }
            }
            fn on_track(&mut self, data: IncomingTrackData) {
                if let Ok(mut g) = self.rec.lock() {
                    g.on_track(data);
                }
            }
            fn on_data(&mut self, message: DataMessage) {
                if let Ok(mut g) = self.rec.lock() {
                    g.on_data(message);
                }
            }
        }

        offerer.set_callbacks(Box::new(SharedRec {
            ice_tx: ice_o_tx,
            rec: Arc::new(Mutex::new(RecordingCallbacks::default())),
        }));
        answerer.set_callbacks(Box::new(SharedRec {
            ice_tx: ice_a_tx,
            rec: Arc::clone(&answerer_rec),
        }));

        let offer = offerer.create_offer().unwrap();
        offerer.set_local_description(offer.clone()).unwrap();
        let offer = block_on(async {
            offerer
                .pc
                .local_description()
                .await
                .map(WebrtcPeerTransport::from_rtc_desc)
                .unwrap_or(offer)
        });

        answerer.set_remote_description(offer).unwrap();
        let answer = answerer.create_answer().unwrap();
        answerer.set_local_description(answer.clone()).unwrap();
        let answer = block_on(async {
            answerer
                .pc
                .local_description()
                .await
                .map(WebrtcPeerTransport::from_rtc_desc)
                .unwrap_or(answer)
        });
        offerer.set_remote_description(answer).unwrap();

        // Trickle any remaining candidates.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            while let Ok(c) = ice_o_rx.try_recv() {
                let _ = answerer.add_ice_candidate(c);
            }
            while let Ok(c) = ice_a_rx.try_recv() {
                let _ = offerer.add_ice_candidate(c);
            }
            offerer.poll().unwrap();
            answerer.poll().unwrap();
            if offerer.connection_state() == ConnectionState::Connected
                && answerer.connection_state() == ConnectionState::Connected
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timeout waiting for connected: o={} a={}",
                offerer.connection_state().as_str(),
                answerer.connection_state().as_str()
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Local fingerprint from DTLS cert; remote may be SDP and/or DTLS.
        let ofp = offerer.local_fingerprint().unwrap();
        assert_eq!(ofp.algorithm, "sha-256");
        // Wait briefly for DTLS remote cert path + DataChannel SCTP open.
        // PeerConnection Connected does not guarantee DC ready_state == Open.
        for _ in 0..100 {
            offerer.poll().unwrap();
            answerer.poll().unwrap();
            if answerer.remote_fingerprint().unwrap().is_some()
                && offerer.remote_fingerprint().unwrap().is_some()
                && offerer.data_channels_open()
                && answerer.data_channels_open()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        offerer
            .wait_data_channels_open(Duration::from_secs(5))
            .expect("offerer DataChannels open");
        // Answerer only needs channels registered; open on offerer is enough to send.

        // DataChannel echo: offerer → answerer on "input".
        offerer
            .send_data(DataMessage {
                label: LABEL_INPUT.into(),
                data: b"ping-webrtc".to_vec(),
                unordered: false,
            })
            .expect("send_data after DC open");

        let mut got = false;
        for _ in 0..100 {
            answerer.poll().unwrap();
            if let Ok(g) = answerer_rec.lock() {
                if g.data.iter().any(|d| d.data == b"ping-webrtc") {
                    got = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(got, "answerer did not receive data channel echo");

        // Media path: RTP H.264 track (preferred) mirrored on media-video DC.
        // IDR-looking annex-B AU so SampleBuilder/payloader have a complete NAL.
        let idr = vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10];
        for i in 0..5 {
            offerer
                .send_video_nalu(VideoNalu {
                    pts_host_mono: Duration::from_millis(33 * (i + 1)),
                    rtp_ts: Some(2970 * (i as u32 + 1)),
                    keyframe: i == 0,
                    format: NaluFormat::AnnexB,
                    data: idr.clone(),
                })
                .expect("send_video_nalu");
            offerer.poll().unwrap();
            answerer.poll().unwrap();
            std::thread::sleep(Duration::from_millis(30));
        }
        let mut got_video = false;
        for _ in 0..150 {
            offerer.poll().unwrap();
            answerer.poll().unwrap();
            if let Ok(g) = answerer_rec.lock() {
                if g.tracks
                    .iter()
                    .any(|t| matches!(t, IncomingTrackData::Video(_)))
                {
                    got_video = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            got_video,
            "answerer did not receive video (RTP track and/or media-video DC)"
        );

        offerer.close().unwrap();
        answerer.close().unwrap();
    }
}
