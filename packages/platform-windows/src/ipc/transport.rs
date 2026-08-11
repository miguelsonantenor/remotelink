//! Control IPC transports.
//!
//! Production Windows path: **named pipe** with restrictive SDDL ACL +
//! `PIPE_REJECT_REMOTE_CLIENTS` (see [`super::pipe`]).
//! CI / cross-platform path: TCP localhost so the same framed codec is
//! exercised without OS-specific sockets.

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

use thiserror::Error;

use super::codec::{encode_control, read_control, CodecError};
use super::message::ControlMessage;

#[cfg(windows)]
use super::pipe::{
    connect_named_pipe, normalize_pipe_path, set_pipe_read_timeout, set_pipe_write_timeout,
    NamedPipeListener, PipeStream, DEFAULT_PIPE_NAME,
};

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
        /// Pipe name path (`\\.\pipe\…`).
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
            path: normalize_pipe_path(DEFAULT_PIPE_NAME),
        }
    }

    /// Named pipe from a leaf name or full `\\.\pipe\…` path.
    #[cfg(windows)]
    pub fn named_pipe(name_or_path: &str) -> Self {
        Self::NamedPipe {
            path: normalize_pipe_path(name_or_path),
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

enum StreamInner {
    Tcp(TcpStream),
    #[cfg(windows)]
    Pipe(PipeStream),
}

/// Accepted or connected control stream (TCP or Windows named pipe).
#[derive(Debug)]
pub struct ControlStream {
    inner: StreamInner,
}

// Manual Debug for StreamInner would be noisy; derive on outer needs Debug on inner.
impl std::fmt::Debug for StreamInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamInner::Tcp(_) => f.write_str("Tcp(..)"),
            #[cfg(windows)]
            StreamInner::Pipe(_) => f.write_str("Pipe(..)"),
        }
    }
}

impl ControlStream {
    /// Send one versioned control message.
    pub fn send(&mut self, msg: &ControlMessage) -> Result<(), TransportError> {
        let frame = encode_control(msg)?;
        match &mut self.inner {
            StreamInner::Tcp(s) => {
                s.write_all(&frame)?;
                s.flush()?;
            }
            #[cfg(windows)]
            StreamInner::Pipe(s) => {
                s.write_all(&frame)?;
                s.flush()?;
            }
        }
        Ok(())
    }

    /// Receive one versioned control message (blocking / timeout if configured).
    pub fn recv(&mut self) -> Result<ControlMessage, TransportError> {
        match &mut self.inner {
            StreamInner::Tcp(s) => Ok(read_control(s)?),
            #[cfg(windows)]
            StreamInner::Pipe(s) => Ok(read_control(s)?),
        }
    }

    /// Set read timeout (TCP or named pipe COMMTIMEOUTS).
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            StreamInner::Tcp(s) => s.set_read_timeout(timeout),
            #[cfg(windows)]
            StreamInner::Pipe(s) => set_pipe_read_timeout(s.file(), timeout),
        }
    }

    /// Set write timeout (TCP or named pipe COMMTIMEOUTS).
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.inner {
            StreamInner::Tcp(s) => s.set_write_timeout(timeout),
            #[cfg(windows)]
            StreamInner::Pipe(s) => set_pipe_write_timeout(s.file(), timeout),
        }
    }

    /// Graceful half-close / drop of the underlying transport.
    pub fn shutdown(self) {
        match self.inner {
            StreamInner::Tcp(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            #[cfg(windows)]
            StreamInner::Pipe(s) => {
                drop(s);
            }
        }
    }

    /// Access the underlying TCP stream when this connection is TCP-backed.
    ///
    /// Prefer [`Self::set_read_timeout`] / [`Self::shutdown`] for backend-agnostic code.
    pub fn into_tcp(self) -> Option<TcpStream> {
        match self.inner {
            StreamInner::Tcp(s) => Some(s),
            #[cfg(windows)]
            StreamInner::Pipe(_) => None,
        }
    }

    /// Borrow the underlying TCP stream when TCP-backed.
    pub fn tcp(&self) -> Option<&TcpStream> {
        match &self.inner {
            StreamInner::Tcp(s) => Some(s),
            #[cfg(windows)]
            StreamInner::Pipe(_) => None,
        }
    }

    /// Whether this stream is a Windows named pipe.
    #[cfg(windows)]
    pub fn is_named_pipe(&self) -> bool {
        matches!(self.inner, StreamInner::Pipe(_))
    }
}

enum ListenerInner {
    Tcp(TcpListener),
    /// Mutex so [`ControlListener::accept`] can take `&self` (pipe needs mut state).
    #[cfg(windows)]
    Pipe(Mutex<NamedPipeListener>),
}

/// Listener for inbound control connections.
#[derive(Debug)]
pub struct ControlListener {
    endpoint: ControlEndpoint,
    inner: ListenerInner,
}

impl std::fmt::Debug for ListenerInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerInner::Tcp(_) => f.write_str("Tcp(..)"),
            #[cfg(windows)]
            ListenerInner::Pipe(_) => f.write_str("Pipe(..)"),
        }
    }
}

impl ControlListener {
    /// Local address port when listening on TCP (useful if port was 0).
    pub fn tcp_port(&self) -> Option<u16> {
        match &self.inner {
            ListenerInner::Tcp(l) => l.local_addr().ok().map(|a| a.port()),
            #[cfg(windows)]
            ListenerInner::Pipe(_) => None,
        }
    }

    /// Bound named-pipe path when listening on a pipe.
    #[cfg(windows)]
    pub fn pipe_path(&self) -> Option<String> {
        match &self.inner {
            ListenerInner::Pipe(p) => p.lock().ok().map(|g| g.path().to_string()),
            ListenerInner::Tcp(_) => None,
        }
    }

    /// Endpoint description (bound port filled in for TCP `0`).
    pub fn endpoint(&self) -> &ControlEndpoint {
        &self.endpoint
    }

    /// Accept one connection (blocking).
    pub fn accept(&self) -> Result<ControlStream, TransportError> {
        match &self.inner {
            ListenerInner::Tcp(tcp) => {
                let (stream, _) = tcp.accept()?;
                stream.set_nodelay(true)?;
                Ok(ControlStream {
                    inner: StreamInner::Tcp(stream),
                })
            }
            #[cfg(windows)]
            ListenerInner::Pipe(pipe) => {
                let mut g = pipe.lock().map_err(|_| {
                    TransportError::Io(io::Error::other("named pipe listener lock poisoned"))
                })?;
                let file = g.accept()?;
                Ok(ControlStream {
                    inner: StreamInner::Pipe(PipeStream::from_file(file)),
                })
            }
        }
    }
}

/// Bind a control listener for the given endpoint.
pub fn listen_control(endpoint: ControlEndpoint) -> Result<ControlListener, TransportError> {
    match endpoint {
        ControlEndpoint::TcpLocalhost { port } => {
            let listener = TcpListener::bind(("127.0.0.1", port))?;
            let bound = ControlEndpoint::TcpLocalhost {
                port: listener.local_addr()?.port(),
            };
            Ok(ControlListener {
                endpoint: bound,
                inner: ListenerInner::Tcp(listener),
            })
        }
        #[cfg(windows)]
        ControlEndpoint::NamedPipe { path } => {
            let path = normalize_pipe_path(&path);
            let pipe = NamedPipeListener::bind(&path)?;
            let bound = ControlEndpoint::NamedPipe {
                path: pipe.path().to_string(),
            };
            Ok(ControlListener {
                endpoint: bound,
                inner: ListenerInner::Pipe(Mutex::new(pipe)),
            })
        }
    }
}

/// Connect to a control endpoint.
pub fn connect_control(endpoint: &ControlEndpoint) -> Result<ControlStream, TransportError> {
    match endpoint {
        ControlEndpoint::TcpLocalhost { port } => {
            let stream = TcpStream::connect(("127.0.0.1", *port))?;
            stream.set_nodelay(true)?;
            Ok(ControlStream {
                inner: StreamInner::Tcp(stream),
            })
        }
        #[cfg(windows)]
        ControlEndpoint::NamedPipe { path } => {
            let file = connect_named_pipe(path)?;
            Ok(ControlStream {
                inner: StreamInner::Pipe(PipeStream::from_file(file)),
            })
        }
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

    #[cfg(windows)]
    #[test]
    fn named_pipe_send_recv_with_acl() {
        use crate::ipc::pipe::unique_test_pipe_path;

        let path = unique_test_pipe_path();
        let listener = listen_control(ControlEndpoint::named_pipe(&path)).unwrap();
        assert_eq!(listener.pipe_path().as_deref(), Some(path.as_str()));
        let endpoint = listener.endpoint().clone();

        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            assert!(conn.is_named_pipe());
            let msg = conn.recv().unwrap();
            assert!(matches!(msg, ControlMessage::Ack(_)));
            conn.send(&ControlMessage::Ack(Ack {
                for_method: Some("pipe-pong".into()),
                session_id: None,
            }))
            .unwrap();
        });

        // Give the server a moment to sit in ConnectNamedPipe.
        std::thread::sleep(Duration::from_millis(50));
        let mut client = connect_control(&endpoint).unwrap();
        assert!(client.is_named_pipe());
        client
            .send(&ControlMessage::Ack(Ack {
                for_method: Some("pipe-hello".into()),
                session_id: None,
            }))
            .unwrap();
        let reply = client.recv().unwrap();
        match reply {
            ControlMessage::Ack(a) => assert_eq!(a.for_method.as_deref(), Some("pipe-pong")),
            other => panic!("unexpected {other:?}"),
        }
        server.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn normalize_pipe_paths() {
        use crate::ipc::pipe::normalize_pipe_path;
        assert_eq!(
            normalize_pipe_path("remotelink-host-control"),
            r"\\.\pipe\remotelink-host-control"
        );
        assert_eq!(
            normalize_pipe_path(r"\\.\pipe\foo"),
            r"\\.\pipe\foo"
        );
    }
}
