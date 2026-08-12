//! WebSocket signaling client for `/v1/ws`.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use remotelink_protocol::{
    decode_message, encode_message, HelloAuth, Role, SignalMessage, PROTOCOL_VERSION,
};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Signaling client errors.
#[derive(Debug, Error)]
pub enum SignalingError {
    /// Connect / IO failure.
    #[error("connect: {0}")]
    Connect(String),
    /// Protocol encode/decode or unexpected message.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Timed out waiting for a message.
    #[error("timeout: {0}")]
    Timeout(String),
    /// Server `error` message.
    #[error("server error {code}: {message}")]
    Server {
        /// Error code from server.
        code: String,
        /// Human message.
        message: String,
    },
    /// Connection closed unexpectedly.
    #[error("connection closed")]
    Closed,
}

impl SignalingError {
    /// True when the server dropped a late/colliding `signal_seq` (ICE race).
    pub fn is_stale_signal_seq(&self) -> bool {
        matches!(
            self,
            Self::Server { code, .. } if code == "stale_signal_seq"
        )
    }
}

/// Result alias.
pub type SignalingResult<T> = Result<T, SignalingError>;

/// Live WSS signaling session with monotonic client-side `signal_seq` helper.
pub struct SignalingClient {
    ws: WsStream,
    /// Next `signal_seq` this client should use for outbound session messages.
    pub next_seq: u64,
}

impl SignalingClient {
    /// Connect to a `ws://` or `wss://` URL (typically `…/v1/ws`).
    pub async fn connect(ws_url: &str) -> SignalingResult<Self> {
        let (ws, _) = connect_async(ws_url)
            .await
            .map_err(|e| SignalingError::Connect(e.to_string()))?;
        Ok(Self {
            ws,
            next_seq: 1,
        })
    }

    /// Send `hello` and wait for `hello_ok` (or map server error).
    pub async fn hello(&mut self, role: Role, device_token: &str) -> SignalingResult<SignalMessage> {
        self.send(&SignalMessage::Hello {
            role,
            protocol_version: PROTOCOL_VERSION,
            auth: HelloAuth {
                device_token: device_token.into(),
            },
        })
        .await?;
        let msg = self.recv_timeout(Duration::from_secs(10)).await?;
        match msg {
            SignalMessage::HelloOk { .. } => Ok(msg),
            SignalMessage::Error { code, message } => Err(SignalingError::Server { code, message }),
            other => Err(SignalingError::Protocol(format!(
                "expected hello_ok, got {other:?}"
            ))),
        }
    }

    /// Host hello with access token.
    pub async fn hello_host(&mut self, access_token: &str) -> SignalingResult<SignalMessage> {
        self.hello(Role::Host, access_token).await
    }

    /// Anonymous viewer hello (empty device token).
    pub async fn hello_viewer_anonymous(&mut self) -> SignalingResult<SignalMessage> {
        self.hello(Role::Viewer, "").await
    }

    /// Encode and send one signaling message.
    pub async fn send(&mut self, msg: &SignalMessage) -> SignalingResult<()> {
        let text = encode_message(msg).map_err(|e| SignalingError::Protocol(e.to_string()))?;
        self.ws
            .send(Message::Text(text.into()))
            .await
            .map_err(|e| SignalingError::Connect(e.to_string()))?;
        Ok(())
    }

    /// Allocate and return the next outbound `signal_seq`, then advance.
    pub fn take_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        s
    }

    /// Observe a remote `signal_seq` and bump `next_seq` past it.
    pub fn observe_seq(&mut self, signal_seq: u64) {
        let next = signal_seq.saturating_add(1);
        if next > self.next_seq {
            self.next_seq = next;
        }
    }

    /// Receive the next application text message (handles ping/pong).
    pub async fn recv(&mut self) -> SignalingResult<SignalMessage> {
        loop {
            let frame = self
                .ws
                .next()
                .await
                .ok_or(SignalingError::Closed)?
                .map_err(|e| SignalingError::Connect(e.to_string()))?;
            match frame {
                Message::Text(t) => {
                    let msg = decode_message(t.as_str())
                        .map_err(|e| SignalingError::Protocol(e.to_string()))?;
                    if let Some(seq) = msg.signal_seq() {
                        self.observe_seq(seq);
                    }
                    if let SignalMessage::Error { code, message } = msg {
                        return Err(SignalingError::Server { code, message });
                    }
                    return Ok(msg);
                }
                Message::Ping(p) => {
                    self.ws
                        .send(Message::Pong(p))
                        .await
                        .map_err(|e| SignalingError::Connect(e.to_string()))?;
                }
                Message::Pong(_) => {}
                Message::Close(_) => return Err(SignalingError::Closed),
                Message::Binary(_) => {
                    return Err(SignalingError::Protocol(
                        "binary frames not supported".into(),
                    ));
                }
                Message::Frame(_) => {}
            }
        }
    }

    /// [`Self::recv`] with a timeout.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> SignalingResult<SignalMessage> {
        tokio::time::timeout(timeout, self.recv())
            .await
            .map_err(|_| SignalingError::Timeout("waiting for signaling message".into()))?
    }

    /// Receive until `pred` returns true (or timeout).
    pub async fn recv_until<F>(
        &mut self,
        timeout: Duration,
        mut pred: F,
    ) -> SignalingResult<SignalMessage>
    where
        F: FnMut(&SignalMessage) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(SignalingError::Timeout(
                    "recv_until deadline exceeded".into(),
                ));
            }
            let msg = self.recv_timeout(remaining).await?;
            if pred(&msg) {
                return Ok(msg);
            }
        }
    }

    /// Close the WebSocket gracefully.
    pub async fn close(mut self) -> SignalingResult<()> {
        let _ = self.ws.close(None).await;
        Ok(())
    }
}
