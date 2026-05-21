//! Windows Named Pipe transport.

use std::io;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use super::{Endpoint, Listener as TransportListener, Stream};

fn pipe_name(endpoint: &Endpoint) -> String {
    // Endpoint path on Windows is expected to be a full pipe name already
    // (e.g. `\\.\pipe\browser-control-firefox-lemur`). If a bare filesystem
    // path was supplied (developer convenience), translate it to a pipe name.
    let s = endpoint.as_path().to_string_lossy();
    if s.starts_with("\\\\.\\pipe\\") || s.starts_with("//./pipe/") {
        s.into_owned()
    } else {
        let stem = endpoint
            .as_path()
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "browser-control".to_string());
        format!("\\\\.\\pipe\\{stem}")
    }
}

pub async fn listen(endpoint: &Endpoint) -> io::Result<Box<dyn TransportListener>> {
    let name = pipe_name(endpoint);
    // First instance creates the pipe.
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&name)?;
    Ok(Box::new(WinL {
        name,
        current: Some(server),
    }))
}

struct WinL {
    name: String,
    current: Option<NamedPipeServer>,
}

#[async_trait::async_trait]
impl TransportListener for WinL {
    async fn accept(&mut self) -> io::Result<Stream> {
        let server = self
            .current
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "listener exhausted"))?;
        server.connect().await?;
        // Pre-create the next server instance so a subsequent client can connect.
        let next = ServerOptions::new()
            .reject_remote_clients(true)
            .create(&self.name)?;
        self.current = Some(next);
        Ok(split(server))
    }
}

pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    let name = pipe_name(endpoint);
    // Retry briefly while the server creates a new instance.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => return Ok(split_client(client)),
            Err(e) if e.raw_os_error() == Some(231) /* ERROR_PIPE_BUSY */ => {
                if std::time::Instant::now() > deadline {
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn split(pipe: NamedPipeServer) -> Stream {
    // Pipes are full-duplex; we wrap once in an Arc-like shareable adapter
    // by splitting via tokio::io::split.
    let (r, w) = tokio::io::split(pipe);
    Stream {
        reader: Box::new(r),
        writer: Box::new(w),
    }
}

fn split_client(pipe: tokio::net::windows::named_pipe::NamedPipeClient) -> Stream {
    let (r, w) = tokio::io::split(pipe);
    Stream {
        reader: Box::new(r),
        writer: Box::new(w),
    }
}
