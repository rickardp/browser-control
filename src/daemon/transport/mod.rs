//! Cross-platform byte-stream transport for the daemon.
//!
//! All daemon RPC rides on a length-framed byte stream provided by a platform
//! transport: Unix Domain Sockets on macOS/Linux and Named Pipes on Windows.
//! Cap'n Proto's `twoparty::VatNetwork` consumes any `AsyncRead + AsyncWrite`,
//! so the transports here just need to expose tokio-flavored stream types.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

/// Connection address. On Unix this is a filesystem path; on Windows it is a
/// pipe name (typically `\\.\pipe\browser-control-<name>`).
#[derive(Debug, Clone)]
pub struct Endpoint(pub std::path::PathBuf);

impl Endpoint {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// A connected duplex byte stream over the chosen platform transport.
///
/// We split this into `(reader, writer)` halves at construction so capnp-rpc's
/// `twoparty::VatNetwork` can consume them without extra plumbing.
pub struct Stream {
    pub reader: Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>,
    pub writer: Box<dyn tokio::io::AsyncWrite + Send + Unpin + 'static>,
}

/// Listener for incoming connections.
#[async_trait::async_trait]
pub trait Listener: Send {
    async fn accept(&mut self) -> io::Result<Stream>;
}

/// Bind a listener to the given endpoint (replaces any stale socket file).
pub async fn listen(endpoint: &Endpoint) -> io::Result<Box<dyn Listener>> {
    #[cfg(unix)]
    {
        unix::listen(endpoint).await
    }
    #[cfg(windows)]
    {
        windows::listen(endpoint).await
    }
}

/// Connect to a previously-bound endpoint.
pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        unix::connect(endpoint).await
    }
    #[cfg(windows)]
    {
        windows::connect(endpoint).await
    }
}
