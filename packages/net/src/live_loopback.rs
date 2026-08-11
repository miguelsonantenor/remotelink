//! Networked TCP [`PeerTransport`] for local multi-process demos.
//!
//! This is **not** full WebRTC (no DTLS-SRTP, no ICE agent, no SRTP). It carries
//! length-prefixed media/data frames over a single TCP connection between two
//! peers so host/viewer can exercise a real socket path without libwebrtc.
//!
//! # Signaling model
//!
//! - SDP body is a small **JSON** document describing the listen address and
//!   SHA-256 fingerprint of an ephemeral identity key (32 random bytes).
//! - ICE candidates are **host TCP** strings derived from the listen/connect
//!   address (for signaling plumbing tests).
//! - Offerer binds `TcpListener` and accepts in a background thread after
//!   `set_local_description`. Answerer dials the offerer's `listen` addr when
//!   both descriptions are set (typically on `set_local_description` of the
//!   answer).
//!
//! # Frame format
//!
//! ```text
//! [u32 BE length of body][u8 kind][kind-specific payload…]
//! ```
//!
//! Kinds: `VideoNalu` (1), `Audio` (2), `Data` (3), `Control` (4).
//!
//! # Event delivery
//!
//! Same pull model as the mock: a reader thread enqueues inbound frames; the
//! application must call [`PeerTransport::poll`] to fire `on_track` / `on_data`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rand::RngCore;
use remotelink_protocol::IceCandidate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{NetError, Result};
use crate::factory::PeerRole;
use crate::transport::{NullCallbacks, PeerTransport, PeerTransportCallbacks};
use crate::types::{
    AudioPacket, ConnectionState, DataMessage, DtlsFingerprint, IncomingTrackData,
    LocalIceCandidate, NaluFormat, SdpType, SessionDescription, TransportIceCandidate, VideoNalu,
};

/// Frame kind byte on the wire.
mod kind {
    pub const VIDEO: u8 = 1;
    pub const AUDIO: u8 = 2;
    pub const DATA: u8 = 3;
    pub const CONTROL: u8 = 4;
}

mod ctrl {
    pub const HELLO: u8 = 1;
    pub const CLOSE: u8 = 2;
}

/// Maximum single-frame payload (body after the 4-byte length), 16 MiB.
const MAX_FRAME_BODY: u32 = 16 * 1024 * 1024;

/// JSON SDP body for the live TCP transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveSdp {
    /// Discriminator (`remotelink-live`).
    #[serde(rename = "type")]
    pub sdp_type_label: String,
    /// Schema version.
    pub v: u32,
    /// `offerer` or `answerer`.
    pub role: String,
    /// `host:port` this peer listens on (offerer required; answerer optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// `sha-256 AA:BB:…` fingerprint attribute.
    pub fingerprint: String,
    /// ICE ufrag (synthetic).
    pub ufrag: String,
    /// ICE pwd (synthetic).
    pub pwd: String,
}

impl LiveSdp {
    /// Parse from SDP string (JSON body, optionally embedded in multi-line SDP).
    pub fn parse(sdp: &str) -> Result<Self> {
        let trimmed = sdp.trim();
        let json = if trimmed.starts_with('{') {
            trimmed
        } else {
            trimmed
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with('{'))
                .ok_or_else(|| NetError::InvalidDescription("live SDP: no JSON body".into()))?
        };
        let parsed: LiveSdp = serde_json::from_str(json)
            .map_err(|e| NetError::InvalidDescription(format!("live SDP JSON: {e}")))?;
        if parsed.sdp_type_label != "remotelink-live" {
            return Err(NetError::InvalidDescription(format!(
                "expected type remotelink-live, got {}",
                parsed.sdp_type_label
            )));
        }
        if parsed.v != 1 {
            return Err(NetError::InvalidDescription(format!(
                "unsupported live SDP version {}",
                parsed.v
            )));
        }
        Ok(parsed)
    }

    /// Serialize to compact JSON string used as [`SessionDescription::sdp`].
    pub fn to_sdp_string(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| NetError::Internal(format!("serialize live SDP: {e}")))
    }

    /// Parse `listen` into a socket address.
    pub fn listen_addr(&self) -> Result<Option<SocketAddr>> {
        let Some(ref s) = self.listen else {
            return Ok(None);
        };
        parse_socket_addr(s).map(Some)
    }
}

/// Configuration for a live TCP peer.
#[derive(Debug, Clone)]
pub struct LivePeerConfig {
    /// Bind address for the offerer listener (`127.0.0.1:0` default).
    pub bind: String,
    /// Host/IP advertised in SDP and ICE (defaults to bound IP).
    pub advertise_host: Option<String>,
    /// Fixed fingerprint; if `None`, SHA-256 of 32 random bytes.
    pub fingerprint: Option<DtlsFingerprint>,
    /// TCP connect/accept timeout.
    pub connect_timeout: Duration,
}

impl Default for LivePeerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:0".into(),
            advertise_host: None,
            fingerprint: None,
            connect_timeout: Duration::from_secs(5),
        }
    }
}

impl LivePeerConfig {
    /// Read optional `REMOTELINK_LIVE_BIND` / `REMOTELINK_LIVE_ADVERTISE`.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(b) = std::env::var("REMOTELINK_LIVE_BIND") {
            if !b.trim().is_empty() {
                cfg.bind = b;
            }
        }
        if let Ok(a) = std::env::var("REMOTELINK_LIVE_ADVERTISE") {
            if !a.trim().is_empty() {
                cfg.advertise_host = Some(a);
            }
        }
        cfg
    }
}

#[derive(Debug)]
enum WireMsg {
    Video(VideoNalu),
    Audio(AudioPacket),
    Data(DataMessage),
    /// Remote closed cleanly.
    PeerClose,
    /// Remote hello with fingerprint hex (64 chars, no colons).
    Hello {
        fingerprint_hex: String,
    },
}

/// Live TCP peer connection.
pub struct LivePeerTransport {
    role: PeerRole,
    config: LivePeerConfig,
    local_fp: DtlsFingerprint,
    remote_fp: Option<DtlsFingerprint>,
    /// Fingerprint expected from remote SDP (checked on Hello).
    expected_remote_fp: Option<DtlsFingerprint>,
    state: ConnectionState,
    local_desc: Option<SessionDescription>,
    remote_desc: Option<SessionDescription>,
    remote_listen: Option<SocketAddr>,
    /// Kept so Drop can shut down the accept path; accept thread owns a dup.
    listen_addr: Option<SocketAddr>,
    /// Incoming accepted stream from background accept thread (offerer).
    accept_rx: Option<Receiver<std::result::Result<TcpStream, String>>>,
    accept_join: Option<JoinHandle<()>>,
    writer: Option<Arc<Mutex<TcpStream>>>,
    inbound_rx: Option<Receiver<WireMsg>>,
    /// Signal reader thread to stop.
    stop: Arc<AtomicBool>,
    reader_join: Option<JoinHandle<()>>,
    callbacks: Box<dyn PeerTransportCallbacks>,
    closed: bool,
    ice_restart_count: u32,
    last_local_ice: Option<IceCandidate>,
    ufrag: String,
    pwd: String,
    /// Offerer accept thread already spawned.
    accept_started: bool,
}

impl LivePeerTransport {
    /// Create a new live peer for `role` (not yet listening/connected).
    pub fn new(role: PeerRole, config: LivePeerConfig) -> Result<Self> {
        let local_fp = match config.fingerprint.clone() {
            Some(fp) => fp,
            None => {
                let mut key = [0u8; 32];
                rand::thread_rng().fill_bytes(&mut key);
                fingerprint_from_key(&key)?
            }
        };
        let mut ufrag_bytes = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut ufrag_bytes);
        let mut pwd_bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut pwd_bytes);
        Ok(Self {
            role,
            config,
            local_fp,
            remote_fp: None,
            expected_remote_fp: None,
            state: ConnectionState::New,
            local_desc: None,
            remote_desc: None,
            remote_listen: None,
            listen_addr: None,
            accept_rx: None,
            accept_join: None,
            writer: None,
            inbound_rx: None,
            stop: Arc::new(AtomicBool::new(false)),
            reader_join: None,
            callbacks: Box::new(NullCallbacks),
            closed: false,
            ice_restart_count: 0,
            last_local_ice: None,
            ufrag: hex::encode(ufrag_bytes),
            pwd: hex::encode(pwd_bytes),
            accept_started: false,
        })
    }

    /// Last ICE candidate emitted (tests).
    pub fn last_local_ice(&self) -> Option<&IceCandidate> {
        self.last_local_ice.as_ref()
    }

    /// Bound listen address after offer creation, if any.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen_addr
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed || self.state == ConnectionState::Closed {
            return Err(NetError::Closed);
        }
        Ok(())
    }

    fn emit_state(&mut self, state: ConnectionState) {
        self.state = state;
        self.callbacks.on_connection_state(state);
    }

    fn build_live_sdp(&self, listen: Option<SocketAddr>) -> Result<LiveSdp> {
        let listen_str = listen.map(|a| {
            let host = self
                .config
                .advertise_host
                .clone()
                .unwrap_or_else(|| a.ip().to_string());
            format!("{host}:{}", a.port())
        });
        Ok(LiveSdp {
            sdp_type_label: "remotelink-live".into(),
            v: 1,
            role: self.role.as_str().into(),
            listen: listen_str,
            fingerprint: self.local_fp.sdp_attribute(),
            ufrag: self.ufrag.clone(),
            pwd: self.pwd.clone(),
        })
    }

    fn session_desc(&self, sdp_type: SdpType, live: &LiveSdp) -> Result<SessionDescription> {
        Ok(SessionDescription {
            sdp_type,
            sdp: live.to_sdp_string()?,
        })
    }

    fn bind_listener(&mut self) -> Result<SocketAddr> {
        if let Some(addr) = self.listen_addr {
            return Ok(addr);
        }
        let listener = TcpListener::bind(&self.config.bind)
            .map_err(|e| NetError::Internal(format!("live bind {}: {e}", self.config.bind)))?;
        let addr = listener
            .local_addr()
            .map_err(|e| NetError::Internal(format!("local_addr: {e}")))?;
        self.listen_addr = Some(addr);
        // Spawn accept thread immediately for offerer path; answerer may also
        // bind for ICE advertisement only (no accept needed for answerer dial).
        if self.role == PeerRole::Offerer {
            self.spawn_accept(listener)?;
        }
        Ok(addr)
    }

    fn spawn_accept(&mut self, listener: TcpListener) -> Result<()> {
        if self.accept_started {
            return Ok(());
        }
        let timeout = self.config.connect_timeout;
        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("remotelink-live-accept".into())
            .spawn(move || {
                // Blocking accept with overall timeout via set_nonblocking poll.
                let _ = listener.set_nonblocking(true);
                let deadline = std::time::Instant::now() + timeout;
                let result = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break Ok(stream),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                break Err("accept timed out".into());
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => break Err(format!("accept: {e}")),
                    }
                };
                let _ = tx.send(result);
            })
            .map_err(|e| NetError::Internal(format!("spawn accept: {e}")))?;
        self.accept_rx = Some(rx);
        self.accept_join = Some(join);
        self.accept_started = true;
        Ok(())
    }

    fn emit_local_host_candidate(&mut self, addr: SocketAddr) {
        let host = self
            .config
            .advertise_host
            .clone()
            .unwrap_or_else(|| addr.ip().to_string());
        let tcptype = match self.role {
            PeerRole::Offerer => "passive",
            PeerRole::Answerer => "active",
        };
        let ice = IceCandidate {
            candidate: format!(
                "candidate:1 1 TCP 2122252543 {host} {} typ host tcptype {tcptype}",
                addr.port()
            ),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
            username_fragment: Some(self.ufrag.clone()),
        };
        self.last_local_ice = Some(ice.clone());
        self.callbacks
            .on_ice_candidate(LocalIceCandidate { candidate: ice });
    }

    fn apply_remote_sdp(&mut self, desc: &SessionDescription) -> Result<()> {
        let live = LiveSdp::parse(&desc.sdp)?;
        if let Some(fp_attr) = live.fingerprint.strip_prefix("sha-256 ") {
            self.expected_remote_fp = Some(DtlsFingerprint::sha256(fp_attr)?);
        } else {
            self.expected_remote_fp = Some(DtlsFingerprint::sha256(&live.fingerprint)?);
        }
        if let Some(addr) = live.listen_addr()? {
            self.remote_listen = Some(addr);
        }
        Ok(())
    }

    /// Establish TCP once both descriptions are known (answerer dials; offerer
    /// finishes accept).
    fn try_establish(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }
        if self.local_desc.is_none() || self.remote_desc.is_none() {
            return Ok(());
        }
        if self.state == ConnectionState::Closed || self.closed {
            return Ok(());
        }

        match self.role {
            PeerRole::Offerer => self.finish_accept(),
            PeerRole::Answerer => self.dial_remote(),
        }
    }

    fn finish_accept(&mut self) -> Result<()> {
        let rx = self
            .accept_rx
            .as_ref()
            .ok_or_else(|| NetError::InvalidState {
                expected: "offerer accept thread",
                actual: "no accept_rx".into(),
            })?;
        let timeout = self.config.connect_timeout;
        let stream = match rx.recv_timeout(timeout) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                return Err(NetError::Internal(format!("live accept failed: {e}")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(NetError::Internal(
                    "live offerer: accept timed out waiting for answerer".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(NetError::Internal(
                    "live offerer: accept thread disconnected".into(),
                ));
            }
        };
        if let Some(j) = self.accept_join.take() {
            let _ = j.join();
        }
        self.accept_rx = None;
        self.attach_stream(stream)
    }

    fn dial_remote(&mut self) -> Result<()> {
        let addr = self.remote_listen.ok_or_else(|| NetError::InvalidState {
            expected: "remote offer listen address",
            actual: "no remote listen".into(),
        })?;
        let timeout = self.config.connect_timeout;
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|e| NetError::Internal(format!("live connect {addr}: {e}")))?;
        self.attach_stream(stream)
    }

    fn attach_stream(&mut self, stream: TcpStream) -> Result<()> {
        stream
            .set_nodelay(true)
            .map_err(|e| NetError::Internal(format!("set_nodelay: {e}")))?;
        let reader = stream
            .try_clone()
            .map_err(|e| NetError::Internal(format!("try_clone: {e}")))?;
        let writer = Arc::new(Mutex::new(stream));

        // Send Hello control frame first.
        {
            let mut w = writer
                .lock()
                .map_err(|e| NetError::Internal(format!("writer lock: {e}")))?;
            write_frame(
                &mut *w,
                kind::CONTROL,
                &encode_hello(&self.local_fp, self.role)?,
            )?;
        }

        let (tx, rx) = mpsc::channel();
        self.stop.store(false, Ordering::SeqCst);
        let stop = Arc::clone(&self.stop);
        let join = thread::Builder::new()
            .name("remotelink-live-reader".into())
            .spawn(move || reader_loop(reader, tx, stop))
            .map_err(|e| NetError::Internal(format!("spawn reader: {e}")))?;

        self.writer = Some(writer);
        self.inbound_rx = Some(rx);
        self.reader_join = Some(join);
        // Connection completes when remote Hello is observed in `poll` (avoids
        // deadlock: answerer must not block before offerer finishes accept).
        Ok(())
    }

    /// Apply a remote Hello control frame (fingerprint check + Connected).
    fn on_hello(&mut self, fingerprint_hex: &str) -> Result<()> {
        let fp = DtlsFingerprint::sha256(fingerprint_hex)?;
        if let Some(ref expected) = self.expected_remote_fp {
            if expected != &fp {
                return Err(NetError::InvalidFingerprint(format!(
                    "remote hello fingerprint mismatch: sdp={} hello={}",
                    expected.value, fp.value
                )));
            }
        }
        self.remote_fp = Some(fp);
        if self.state != ConnectionState::Connected
            && self.state != ConnectionState::Closed
            && self.state != ConnectionState::Disconnected
        {
            self.emit_state(ConnectionState::Connected);
        }
        Ok(())
    }

    /// Block until Connected (or timeout). Used by tests / demos after establish.
    pub fn wait_connected(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + self.config.connect_timeout;
        loop {
            self.poll()?;
            if self.state == ConnectionState::Connected {
                return Ok(());
            }
            if self.state == ConnectionState::Failed
                || self.state == ConnectionState::Closed
                || self.state == ConnectionState::Disconnected
            {
                return Err(NetError::Internal(format!(
                    "wait_connected: ended in {}",
                    self.state.as_str()
                )));
            }
            if std::time::Instant::now() >= deadline {
                return Err(NetError::Internal(
                    "timed out waiting for Connected (remote Hello)".into(),
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn send_frame(&mut self, kind_byte: u8, body: &[u8]) -> Result<()> {
        self.ensure_open()?;
        if self.state != ConnectionState::Connected {
            return Err(NetError::InvalidState {
                expected: "connected",
                actual: self.state.as_str().into(),
            });
        }
        let writer = self.writer.as_ref().ok_or_else(|| NetError::InvalidState {
            expected: "connected stream",
            actual: "no writer".into(),
        })?;
        let mut w = writer
            .lock()
            .map_err(|e| NetError::Internal(format!("writer lock: {e}")))?;
        write_frame(&mut *w, kind_byte, body)
    }

    fn shutdown_io(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.writer.take() {
            if let Ok(mut g) = w.lock() {
                let _ = write_frame(&mut *g, kind::CONTROL, &[ctrl::CLOSE]);
                let _ = g.shutdown(std::net::Shutdown::Both);
            }
        }
        if let Some(handle) = self.reader_join.take() {
            let _ = handle.join();
        }
        self.inbound_rx = None;
        // Wake / drop accept side.
        self.accept_rx = None;
        if let Some(j) = self.accept_join.take() {
            let _ = j.join();
        }
    }
}

impl PeerTransport for LivePeerTransport {
    fn set_callbacks(&mut self, callbacks: Box<dyn PeerTransportCallbacks>) {
        self.callbacks = callbacks;
    }

    fn connection_state(&self) -> ConnectionState {
        self.state
    }

    fn create_offer(&mut self) -> Result<SessionDescription> {
        self.ensure_open()?;
        if self.role != PeerRole::Offerer {
            return Err(NetError::InvalidState {
                expected: "offerer role",
                actual: self.role.as_str().into(),
            });
        }
        let addr = self.bind_listener()?;
        let live = self.build_live_sdp(Some(addr))?;
        self.session_desc(SdpType::Offer, &live)
    }

    fn create_answer(&mut self) -> Result<SessionDescription> {
        self.ensure_open()?;
        if self.role != PeerRole::Answerer {
            return Err(NetError::InvalidState {
                expected: "answerer role",
                actual: self.role.as_str().into(),
            });
        }
        if self.remote_desc.is_none() {
            return Err(NetError::InvalidState {
                expected: "remote offer set",
                actual: "no remote description".into(),
            });
        }
        // Answerer may advertise an ephemeral local port for ICE completeness.
        let addr = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => {
                let a = l.local_addr().ok();
                // Drop listener — answerer is active (dials); no accept.
                a
            }
            Err(_) => None,
        };
        if let Some(a) = addr {
            self.listen_addr = Some(a);
        }
        let live = self.build_live_sdp(addr)?;
        self.session_desc(SdpType::Answer, &live)
    }

    fn set_local_description(&mut self, desc: SessionDescription) -> Result<()> {
        self.ensure_open()?;
        if desc.sdp.is_empty() {
            return Err(NetError::InvalidDescription("empty sdp".into()));
        }
        let _ = LiveSdp::parse(&desc.sdp)?;
        self.local_desc = Some(desc);
        if self.state == ConnectionState::New {
            self.emit_state(ConnectionState::Connecting);
        }
        if let Some(addr) = self.listen_addr {
            self.emit_local_host_candidate(addr);
        }
        // Answerer dials when both sides' descriptions are present.
        // Offerer finishes accept when remote answer is already set.
        self.try_establish()?;
        Ok(())
    }

    fn set_remote_description(&mut self, desc: SessionDescription) -> Result<()> {
        self.ensure_open()?;
        if desc.sdp.is_empty() {
            return Err(NetError::InvalidDescription("empty sdp".into()));
        }
        self.apply_remote_sdp(&desc)?;
        self.remote_desc = Some(desc);
        if self.state == ConnectionState::New {
            self.emit_state(ConnectionState::Connecting);
        }
        self.try_establish()?;
        Ok(())
    }

    fn add_ice_candidate(&mut self, candidate: TransportIceCandidate) -> Result<()> {
        self.ensure_open()?;
        if candidate.candidate.is_empty() {
            return Err(NetError::InvalidCandidate("empty candidate".into()));
        }
        if self.remote_listen.is_none() {
            if let Some(addr) = parse_ice_host_addr(&candidate.candidate) {
                self.remote_listen = Some(addr);
            }
        }
        self.try_establish()?;
        Ok(())
    }

    fn restart_ice(&mut self) -> Result<()> {
        self.ensure_open()?;
        self.ice_restart_count = self.ice_restart_count.saturating_add(1);
        if let Some(addr) = self.listen_addr {
            if self.state == ConnectionState::Connected {
                self.emit_state(ConnectionState::Connecting);
                self.emit_local_host_candidate(addr);
                self.emit_state(ConnectionState::Connected);
            } else {
                self.emit_local_host_candidate(addr);
            }
        }
        Ok(())
    }

    fn local_fingerprint(&self) -> Result<DtlsFingerprint> {
        Ok(self.local_fp.clone())
    }

    fn remote_fingerprint(&self) -> Result<Option<DtlsFingerprint>> {
        Ok(self.remote_fp.clone())
    }

    fn send_video_nalu(&mut self, nalu: VideoNalu) -> Result<()> {
        self.send_frame(kind::VIDEO, &encode_video(&nalu)?)
    }

    fn send_audio(&mut self, packet: AudioPacket) -> Result<()> {
        self.send_frame(kind::AUDIO, &encode_audio(&packet)?)
    }

    fn send_data(&mut self, message: DataMessage) -> Result<()> {
        self.send_frame(kind::DATA, &encode_data(&message)?)
    }

    fn poll(&mut self) -> Result<()> {
        if self.closed || self.state == ConnectionState::Closed {
            return Err(NetError::Closed);
        }
        // Drain into a local buffer so we can mutably use `self` for callbacks.
        let mut batch = Vec::new();
        let mut peer_gone = false;
        if let Some(rx) = self.inbound_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(msg) => batch.push(msg),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        peer_gone = true;
                        break;
                    }
                }
            }
        } else {
            return Ok(());
        }

        for msg in batch {
            match msg {
                WireMsg::Video(v) => {
                    self.callbacks.on_track(IncomingTrackData::Video(v));
                }
                WireMsg::Audio(a) => {
                    self.callbacks.on_track(IncomingTrackData::Audio(a));
                }
                WireMsg::Data(d) => {
                    self.callbacks.on_data(d);
                }
                WireMsg::Hello { fingerprint_hex } => {
                    self.on_hello(&fingerprint_hex)?;
                }
                WireMsg::PeerClose => {
                    if self.state == ConnectionState::Connected
                        || self.state == ConnectionState::Connecting
                    {
                        self.emit_state(ConnectionState::Disconnected);
                    }
                    return Ok(());
                }
            }
        }
        if peer_gone
            && (self.state == ConnectionState::Connected
                || self.state == ConnectionState::Connecting)
        {
            self.emit_state(ConnectionState::Disconnected);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutdown_io();
        self.emit_state(ConnectionState::Closed);
        Ok(())
    }
}

impl Drop for LivePeerTransport {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            self.shutdown_io();
        }
    }
}

// --- wire codec ----------------------------------------------------------------

fn write_frame(w: &mut dyn Write, kind_byte: u8, body: &[u8]) -> Result<()> {
    let len = 1u32
        .checked_add(body.len() as u32)
        .ok_or_else(|| NetError::SendFailed("frame too large".into()))?;
    if len > MAX_FRAME_BODY {
        return Err(NetError::SendFailed(format!(
            "frame body {len} exceeds max {MAX_FRAME_BODY}"
        )));
    }
    w.write_all(&len.to_be_bytes())
        .map_err(|e| NetError::SendFailed(format!("write len: {e}")))?;
    w.write_all(&[kind_byte])
        .map_err(|e| NetError::SendFailed(format!("write kind: {e}")))?;
    w.write_all(body)
        .map_err(|e| NetError::SendFailed(format!("write body: {e}")))?;
    w.flush()
        .map_err(|e| NetError::SendFailed(format!("flush: {e}")))?;
    Ok(())
}

fn is_transient_read(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

/// Read exactly `buf.len()` bytes, retrying transient timeouts without losing progress.
fn read_exact_resilient(
    stream: &mut TcpStream,
    buf: &mut [u8],
    stop: &AtomicBool,
) -> std::result::Result<(), ()> {
    let mut got = 0usize;
    while got < buf.len() {
        if stop.load(Ordering::SeqCst) {
            return Err(());
        }
        match stream.read(&mut buf[got..]) {
            Ok(0) => return Err(()), // EOF
            Ok(n) => got += n,
            Err(ref e) if is_transient_read(e) => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

fn reader_loop(mut stream: TcpStream, tx: Sender<WireMsg>, stop: Arc<AtomicBool>) {
    // Short timeout so we can observe `stop` without busy-spinning; partial
    // progress is preserved by read_exact_resilient (unlike naked read_exact).
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    while !stop.load(Ordering::SeqCst) {
        let mut len_buf = [0u8; 4];
        if read_exact_resilient(&mut stream, &mut len_buf, &stop).is_err() {
            let _ = tx.send(WireMsg::PeerClose);
            break;
        }
        let len = u32::from_be_bytes(len_buf);
        if len == 0 || len > MAX_FRAME_BODY {
            let _ = tx.send(WireMsg::PeerClose);
            break;
        }
        let mut body = vec![0u8; len as usize];
        if read_exact_resilient(&mut stream, &mut body, &stop).is_err() {
            let _ = tx.send(WireMsg::PeerClose);
            break;
        }
        let kind_byte = body[0];
        let payload = &body[1..];
        let msg = match kind_byte {
            kind::VIDEO => match decode_video(payload) {
                Ok(v) => WireMsg::Video(v),
                Err(_) => continue,
            },
            kind::AUDIO => match decode_audio(payload) {
                Ok(a) => WireMsg::Audio(a),
                Err(_) => continue,
            },
            kind::DATA => match decode_data(payload) {
                Ok(d) => WireMsg::Data(d),
                Err(_) => continue,
            },
            kind::CONTROL => {
                if payload.is_empty() {
                    continue;
                }
                match payload[0] {
                    ctrl::HELLO => match decode_hello(&payload[1..]) {
                        Ok(fingerprint_hex) => WireMsg::Hello { fingerprint_hex },
                        Err(_) => continue,
                    },
                    ctrl::CLOSE => WireMsg::PeerClose,
                    _ => continue,
                }
            }
            _ => continue,
        };
        if tx.send(msg).is_err() {
            break;
        }
    }
}

fn encode_hello(fp: &DtlsFingerprint, role: PeerRole) -> Result<Vec<u8>> {
    let digest = fp.digest_bytes()?;
    let mut out = Vec::with_capacity(1 + 32 + 1);
    out.push(ctrl::HELLO);
    out.extend_from_slice(&digest);
    out.push(match role {
        PeerRole::Offerer => 1,
        PeerRole::Answerer => 2,
    });
    Ok(out)
}

fn decode_hello(payload: &[u8]) -> Result<String> {
    if payload.len() < 32 {
        return Err(NetError::Internal("hello too short".into()));
    }
    Ok(hex::encode(&payload[..32]))
}

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

fn encode_data(m: &DataMessage) -> Result<Vec<u8>> {
    let label = m.label.as_bytes();
    if label.len() > u16::MAX as usize {
        return Err(NetError::SendFailed("data label too long".into()));
    }
    let mut out = Vec::with_capacity(1 + 2 + label.len() + m.data.len());
    out.push(u8::from(m.unordered));
    out.extend_from_slice(&(label.len() as u16).to_be_bytes());
    out.extend_from_slice(label);
    out.extend_from_slice(&m.data);
    Ok(out)
}

fn decode_data(p: &[u8]) -> Result<DataMessage> {
    if p.len() < 3 {
        return Err(NetError::Internal("data frame too short".into()));
    }
    let unordered = p[0] != 0;
    let label_len = u16::from_be_bytes(p[1..3].try_into().unwrap()) as usize;
    if p.len() < 3 + label_len {
        return Err(NetError::Internal("data label truncated".into()));
    }
    let label = String::from_utf8_lossy(&p[3..3 + label_len]).into_owned();
    let data = p[3 + label_len..].to_vec();
    Ok(DataMessage {
        label,
        data,
        unordered,
    })
}

fn fingerprint_from_key(key: &[u8; 32]) -> Result<DtlsFingerprint> {
    let digest = Sha256::digest(key);
    DtlsFingerprint::sha256(hex::encode(digest))
}

fn parse_socket_addr(s: &str) -> Result<SocketAddr> {
    s.to_socket_addrs()
        .map_err(|e| NetError::InvalidDescription(format!("bad listen addr `{s}`: {e}")))?
        .next()
        .ok_or_else(|| NetError::InvalidDescription(format!("listen addr `{s}` resolved empty")))
}

/// Extract `ip port` from a host TCP candidate string.
fn parse_ice_host_addr(candidate: &str) -> Option<SocketAddr> {
    // candidate:1 1 TCP 2122252543 127.0.0.1 54321 typ host …
    let parts: Vec<&str> = candidate.split_whitespace().collect();
    if let Some(typ_idx) = parts.iter().position(|p| *p == "typ") {
        if typ_idx >= 2 {
            let ip = parts[typ_idx - 2];
            let port = parts[typ_idx - 1];
            if let Ok(addr) = format!("{ip}:{port}").parse() {
                return Some(addr);
            }
        }
    }
    None
}

/// Run offer/answer + TCP connect between two live peers (test / demo helper).
///
/// Order is sequential and safe: offerer starts a background accept thread in
/// `create_offer`; answerer dials on `set_local_description`; offerer completes
/// accept on `set_remote_description(answer)`; both wait for Hello → Connected.
pub fn live_handshake(
    offerer: &mut LivePeerTransport,
    answerer: &mut LivePeerTransport,
) -> Result<()> {
    let offer = offerer.create_offer()?;
    offerer.set_local_description(offer.clone())?;
    answerer.set_remote_description(offer)?;

    let answer = answerer.create_answer()?;
    // Dial (answerer) then accept-complete (offerer). Accept thread already
    // waiting since create_offer; dial first so accept does not time out.
    answerer.set_local_description(answer.clone())?;
    offerer.set_remote_description(answer)?;

    // Hello frames complete the connection (non-blocking establish above).
    answerer.wait_connected()?;
    offerer.wait_connected()?;

    // Exchange ICE candidates for signaling completeness.
    if let Some(c) = offerer.last_local_ice.clone() {
        answerer.add_ice_candidate(c)?;
    }
    if let Some(c) = answerer.last_local_ice.clone() {
        offerer.add_ice_candidate(c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::SharedRecording;
    use crate::transport::PeerTransport;

    #[test]
    fn live_sdp_roundtrip() {
        let sdp = LiveSdp {
            sdp_type_label: "remotelink-live".into(),
            v: 1,
            role: "offerer".into(),
            listen: Some("127.0.0.1:9".into()),
            fingerprint:
                "sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99"
                    .into(),
            ufrag: "ab".into(),
            pwd: "cd".into(),
        };
        let s = sdp.to_sdp_string().unwrap();
        let parsed = LiveSdp::parse(&s).unwrap();
        assert_eq!(parsed.listen.as_deref(), Some("127.0.0.1:9"));
    }

    #[test]
    fn fingerprint_from_random_key() {
        let key = [0x42u8; 32];
        let fp = fingerprint_from_key(&key).unwrap();
        assert_eq!(fp.algorithm, "sha-256");
        assert_eq!(fp.digest_bytes().unwrap().len(), 32);
    }

    #[test]
    fn localhost_client_server_media_and_data() {
        let mut offerer =
            LivePeerTransport::new(PeerRole::Offerer, LivePeerConfig::default()).unwrap();
        let mut answerer =
            LivePeerTransport::new(PeerRole::Answerer, LivePeerConfig::default()).unwrap();

        let rec = SharedRecording::new();
        answerer.set_callbacks(Box::new(rec.clone()));

        live_handshake(&mut offerer, &mut answerer).unwrap();
        assert_eq!(offerer.connection_state(), ConnectionState::Connected);
        assert_eq!(answerer.connection_state(), ConnectionState::Connected);

        let local_o = offerer.local_fingerprint().unwrap();
        let remote_a = answerer.remote_fingerprint().unwrap().unwrap();
        assert_eq!(local_o, remote_a);

        offerer
            .send_video_nalu(VideoNalu {
                pts_host_mono: Duration::from_millis(33),
                rtp_ts: Some(2970),
                keyframe: true,
                format: NaluFormat::AnnexB,
                data: vec![0, 0, 0, 1, 0x67, 1, 2, 3],
            })
            .unwrap();
        offerer
            .send_audio(AudioPacket {
                pts_host_mono: Duration::from_millis(33),
                rtp_ts: Some(1584),
                sample_rate: 48_000,
                channels: 2,
                data: vec![0xde, 0xad, 0xbe, 0xef],
            })
            .unwrap();
        offerer
            .send_data(DataMessage {
                label: "input".into(),
                data: br#"{"seq":1}"#.to_vec(),
                unordered: true,
            })
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            answerer.poll().unwrap();
            let snap = rec.snapshot();
            if snap.tracks.len() >= 2 && !snap.data.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timeout waiting for media; tracks={} data={}",
                    snap.tracks.len(),
                    snap.data.len()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }

        let snap = rec.snapshot();
        assert_eq!(snap.tracks.len(), 2);
        assert_eq!(snap.data.len(), 1);
        assert_eq!(snap.data[0].label, "input");
        match &snap.tracks[0] {
            IncomingTrackData::Video(v) => {
                assert!(v.keyframe);
                assert_eq!(v.data[4], 0x67);
            }
            other => panic!("expected video first, got {other:?}"),
        }

        offerer.close().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let _ = answerer.poll();
            if answerer.connection_state() == ConnectionState::Disconnected
                || answerer.connection_state() == ConnectionState::Closed
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        answerer.close().unwrap();
    }

    #[test]
    fn reverse_direction_data() {
        let mut offerer =
            LivePeerTransport::new(PeerRole::Offerer, LivePeerConfig::default()).unwrap();
        let mut answerer =
            LivePeerTransport::new(PeerRole::Answerer, LivePeerConfig::default()).unwrap();
        let rec = SharedRecording::new();
        offerer.set_callbacks(Box::new(rec.clone()));
        live_handshake(&mut offerer, &mut answerer).unwrap();

        assert_eq!(offerer.connection_state(), ConnectionState::Connected);
        assert_eq!(answerer.connection_state(), ConnectionState::Connected);

        answerer
            .send_data(DataMessage {
                label: "control".into(),
                data: b"ping".to_vec(),
                unordered: false,
            })
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            offerer.poll().unwrap();
            answerer.poll().unwrap(); // keep reader side healthy
            if !rec.snapshot().data.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "no reverse data; offerer_state={} answerer_state={}",
                    offerer.connection_state().as_str(),
                    answerer.connection_state().as_str()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rec.snapshot().data[0].label, "control");
        offerer.close().unwrap();
        answerer.close().unwrap();
    }

    #[test]
    fn parse_ice_host() {
        let c = "candidate:1 1 TCP 2122252543 127.0.0.1 54321 typ host tcptype passive";
        let addr = parse_ice_host_addr(c).unwrap();
        assert_eq!(addr.port(), 54321);
    }
}
