//! Control IPC transports.
//!
//! Production Windows path: named pipe (ACL + boot secret) — stubbed under
//! `cfg(windows)` for later wiring. CI / cross-platform path: TCP localhost so
//! the same framed codec is exercised without OS-specific sockets.

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};

use thiserror::Error;

use super::codec::{encode_control, read_control, CodecError};
use super::message::ControlMessage;

/// Default TCP port for local control IPC in CI / dev (ephemeral preferred via `0`).
pub const DEFAULT_TCP_CONTROL_PORT: u16 = 0;

/// How to address a control endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEndpoint {
    /// TCP loopback (CI-friendly, all platforms).
    TcpLocalhost {
        /// Port; `0` when listening means ephemeral.
        port: u16,
    },
    /// Windows named pipe path (e.g. `\\.\pipe\remotelink-host-control`).
    #[cfg(windows)]
    NamedPipe {
        /// Pipe name path.
        path: String,
    },
}

impl ControlEndpoint {
    /// TCP localhost helper.
    pub fn tcp_localhost(port: u16) -> Self {
        Self::TcpLocalhost { port }
    }

    /// Default Windows named pipe path for host control.
    #[cfg(windows)]
    pub fn default_named_pipe() -> Self {
        Self::NamedPipe {
            path: r"\\.\pipe\remotelink-host-control".to_string(),
        }
    }
}

/// Transport errors.
#[derive(Debug, Error)]
pub enum TransportError {
    /// I/O failure.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Codec / framing failure.
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    /// Feature not available on this platform.
    #[error("unsupported control endpoint on this platform")]
    Unsupported,
}

/// Accepted or connected control stream (TCP for now).
#[derive(Debug)]
pub struct ControlStream {
    inner: TcpStream,
}

impl ControlStream {
    /// Send one versioned control message.
    pub fn send(&mut self, msg: &ControlMessage) -> Result<(), TransportError> {
        let frame = encode_control(msg)?;
        self.inner.write_all(&frame)?;
        self.inner.flush()?;
        Ok(())
    }

    /// Receive one versioned control message (blocking).
    pub fn recv(&mut self) -> Result<ControlMessage, TransportError> {
        Ok(read_control(&mut self.inner)?)
    }

    /// Access the underlying TCP stream (tests / advanced).
    pub fn into_tcp(self) -> TcpStream {
        self.inner
    }

    /// Borrow the underlying TCP stream.
    pub fn tcp(&self) -> &TcpStream {
        &self.inner
    }
}

/// Listener for inbound control connections.
#[derive(Debug)]
pub struct ControlListener {
    endpoint: ControlEndpoint,
    tcp: Option<TcpListener>,
}

impl ControlListener {
    /// Local address port when listening on TCP (useful if port was 0).
    pub fn tcp_port(&self) -> Option<u16> {
        self.tcp
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
    }

    /// Endpoint description.
    pub fn endpoint(&self) -> &ControlEndpoint {
        &self.endpoint
    }

    /// Accept one connection (blocking).
    pub fn accept(&self) -> Result<ControlStream, TransportError> {
        let tcp = self
            .tcp
            .as_ref()
            .ok_or_else(|| io::Error::other("listener has no tcp backend"))?;
        let (stream, _) = tcp.accept()?;
        stream.set_nodelay(true)?;
        Ok(ControlStream { inner: stream })
    }
}

/// Bind a control listener for the given endpoint.
pub fn listen_control(endpoint: ControlEndpoint) -> Result<ControlListener, TransportError> {
    match &endpoint {
        ControlEndpoint::TcpLocalhost { port } => {
            let listener = TcpListener::bind(("127.0.0.1", *port))?;
            let bound = ControlEndpoint::TcpLocalhost {
                port: listener.local_addr()?.port(),
            };
            Ok(ControlListener {
                endpoint: bound,
                tcp: Some(listener),
            })
        }
        #[cfg(windows)]
        ControlEndpoint::NamedPipe { .. } => {
            // Full named-pipe server with ACL lands with service install work.
            // For this skeleton, callers should use TCP for tests; production
            // wiring will replace this stub.
            Err(TransportError::Unsupported)
        }
    }
}

/// Connect to a control endpoint.
pub fn connect_control(endpoint: &ControlEndpoint) -> Result<ControlStream, TransportError> {
    match endpoint {
        ControlEndpoint::TcpLocalhost { port } => {
            let stream = TcpStream::connect(("127.0.0.1", *port))?;
            stream.set_nodelay(true)?;
            Ok(ControlStream { inner: stream })
        }
        #[cfg(windows)]
        ControlEndpoint::NamedPipe { .. } => Err(TransportError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::message::{Ack, ControlMessage};

    #[test]
    fn tcp_localhost_send_recv() {
        let listener = listen_control(ControlEndpoint::tcp_localhost(0)).unwrap();
        let port = listener.tcp_port().unwrap();
        let endpoint = ControlEndpoint::tcp_localhost(port);

        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            let msg = conn.recv().unwrap();
            assert!(matches!(msg, ControlMessage::Ack(_)));
            conn.send(&ControlMessage::Ack(Ack {
                for_method: Some("ping".into()),
                session_id: None,
            }))
            .unwrap();
        });

        let mut client = connect_control(&endpoint).unwrap();
        client
            .send(&ControlMessage::Ack(Ack {
                for_method: Some("hello".into()),
                session_id: None,
            }))
            .unwrap();
        let reply = client.recv().unwrap();
        match reply {
            ControlMessage::Ack(a) => assert_eq!(a.for_method.as_deref(), Some("ping")),
            other => panic!("unexpected {other:?}"),
        }
        server.join().unwrap();
    }
}
