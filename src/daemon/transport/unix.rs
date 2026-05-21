//! Unix Domain Socket transport.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use super::{Endpoint, Listener as TransportListener, Stream};

pub async fn listen(endpoint: &Endpoint) -> io::Result<Box<dyn TransportListener>> {
    let path = endpoint.as_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if path.exists() {
        // Best-effort: remove any stale socket file; if a daemon is actively
        // listening we'll fail on bind below and bringup will handle it.
        let _ = tokio::fs::remove_file(path).await;
    }
    let listener = UnixListener::bind(path)?;
    // Tight permissions: only the owning user may connect.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(Box::new(UnixL(listener)))
}

struct UnixL(UnixListener);

#[async_trait::async_trait]
impl TransportListener for UnixL {
    async fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _addr) = self.0.accept().await?;
        Ok(split(stream))
    }
}

pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    let stream = UnixStream::connect(endpoint.as_path()).await?;
    Ok(split(stream))
}

fn split(stream: UnixStream) -> Stream {
    let (r, w) = stream.into_split();
    Stream {
        reader: Box::new(r),
        writer: Box::new(WriterAdapter(w)),
    }
}

/// Wraps `OwnedWriteHalf` so it implements `AsyncWrite` with the bounds we
/// need (`Send + Unpin + 'static`).
struct WriterAdapter(tokio::net::unix::OwnedWriteHalf);

impl tokio::io::AsyncWrite for WriterAdapter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Unlink the endpoint file if present. Used during graceful shutdown.
pub fn unlink(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::super::{connect, listen, Endpoint};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn uds_loopback_roundtrip() {
        let dir = TempDir::new().unwrap();
        let endpoint = Endpoint::new(dir.path().join("test.sock"));
        let mut listener = listen(&endpoint).await.unwrap();

        let ep2 = endpoint.clone();
        let server = tokio::spawn(async move {
            let mut s = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            s.reader.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
            s.writer.write_all(b"world").await.unwrap();
            s.writer.flush().await.unwrap();
        });

        let mut client = connect(&ep2).await.unwrap();
        client.writer.write_all(b"hello").await.unwrap();
        client.writer.flush().await.unwrap();
        let mut buf = [0u8; 5];
        client.reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");

        server.await.unwrap();
    }
}
