//! Shared WebSocket JSON-RPC transport (`WsRpc`) used by both the CDP and
//! BiDi clients.
//!
//! CDP and BiDi are two JSON-RPC-over-WebSocket dialects with identical
//! connect / writer-task / reader-frame-decode / `pending`-correlation /
//! `next_id` / broadcast / timeout-ladder machinery. They previously each
//! reimplemented all of it, and the two copies had drifted in ways that were
//! genuine bugs on the BiDi side (pending entries leaked on writer failure;
//! no typed disconnect error; no `close()`, no stored `JoinHandle`s, so
//! dropping a cached client orphaned parked reader tasks). This module is the
//! single shared implementation; the protocol-specific framing/typing stays in
//! `crate::cdp` / `crate::bidi` via the [`Protocol`] trait.
//!
//! The constants that had drifted (`REQUEST_TIMEOUT` vs `SEND_TIMEOUT`, the
//! event-channel capacity, the connect timeout) are converged here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// Per-request reply timeout. One value for both protocols (previously
/// `REQUEST_TIMEOUT` on the CDP side and `SEND_TIMEOUT` on the BiDi side,
/// both 30s).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the per-client event broadcast channel.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Bound on `connect_async` / HTTP discovery during initial bringup.
///
/// A dead browser process or a stale endpoint can otherwise stall the
/// WebSocket upgrade (or the underlying TCP connect) for the OS's connect
/// timeout — multiple seconds to over a minute on macOS/Linux. Five seconds
/// matches the `--version` probe in `crate::detect` and is short enough that
/// agents don't perceive a hang.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Protocol adapter: the dialect-specific framing/typing that the shared
/// transport delegates to. CDP and BiDi each implement this; everything else
/// (socket, tasks, correlation, timeouts, lifecycle) is shared.
pub trait Protocol: Send + Sync + 'static {
    /// Per-reply protocol error type (e.g. `CdpError` / `BidiError`).
    type ProtoError: Send + 'static;
    /// Broadcast event type delivered to subscribers.
    type Event: Clone + Send + 'static;

    /// Serialize an outbound request to the wire text. `session_id` carries
    /// the CDP flat-session id; BiDi ignores it.
    fn encode_request(
        id: u64,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
    ) -> Result<String>;

    /// Decode an inbound text frame into a [`Decoded`] outcome.
    fn decode_frame(text: &str) -> Decoded<Self::ProtoError, Self::Event>;

    /// Build the protocol's "connection closed" error used to fail pending
    /// requests when the reader task exits (socket close / I/O error). This is
    /// what makes a disconnect surface as the protocol's typed error instead of
    /// silently dropping the oneshot.
    fn closed_error() -> Self::ProtoError;
}

/// Outcome of decoding one inbound frame.
pub enum Decoded<E, Ev> {
    /// A reply correlated to a pending request `id`.
    Reply {
        id: u64,
        result: Result<serde_json::Value, E>,
    },
    /// An unsolicited event for the broadcast channel.
    Event(Ev),
    /// Nothing actionable (event/error without an id, unparseable frame, etc.).
    Ignore,
}

type PendingMap<E> = HashMap<u64, oneshot::Sender<Result<serde_json::Value, E>>>;

/// Shared JSON-RPC-over-WebSocket transport. Owns the socket, the reader and
/// writer tasks (and their `JoinHandle`s), the pending-request correlation
/// map, the id counter, and the event broadcast channel.
pub struct WsRpc<P: Protocol> {
    next_id: Mutex<u64>,
    pending: Arc<Mutex<PendingMap<P::ProtoError>>>,
    events_tx: broadcast::Sender<P::Event>,
    write_tx: mpsc::UnboundedSender<String>,
    // `Option` so both the graceful `close()` and the `Drop` guard can take the
    // handles without moving out of a `Drop` type.
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl<P: Protocol> WsRpc<P> {
    /// Connect by full WebSocket URL (ws:// or wss://) and spawn the reader and
    /// writer tasks. `label` is used only in the connect-timeout error message
    /// (e.g. `"CDP"` / `"BiDi"`).
    pub async fn connect(ws_url: &str, label: &str) -> Result<Self> {
        let (ws_stream, _) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
                .await
                .map_err(|_| {
                    anyhow!(
                        "{label} WebSocket connect to {ws_url} timed out after {:?}",
                        CONNECT_TIMEOUT
                    )
                })??;
        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        let pending: Arc<Mutex<PendingMap<P::ProtoError>>> = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();

        let writer_handle = tokio::spawn(async move {
            while let Some(text) = write_rx.recv().await {
                if ws_sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = ws_sink.close().await;
        });

        let pending_r = pending.clone();
        let events_r = events_tx.clone();
        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = ws_stream.next().await {
                let text = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Binary(b)) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!(
                                bytes = e.as_bytes().len(),
                                "dropping non-UTF-8 binary frame"
                            );
                            continue;
                        }
                    },
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                match P::decode_frame(&text) {
                    Decoded::Reply { id, result } => {
                        if let Some(tx) = pending_r.lock().await.remove(&id) {
                            let _ = tx.send(result);
                        }
                    }
                    Decoded::Event(ev) => {
                        let _ = events_r.send(ev);
                    }
                    // Unparseable or id-less frame: can't be correlated to any
                    // waiter, so it's dropped — but log it (truncated) so a
                    // hung request has a breadcrumb instead of silence.
                    Decoded::Ignore => {
                        tracing::debug!(frame = %truncate_frame(&text), "dropping undecodable/idless frame");
                    }
                }
            }
            // Reader closed (socket close / I/O error): fail every pending
            // request with the protocol's typed "connection closed" error so
            // no waiter rides the full request timeout and no oneshot is
            // dropped silently.
            let mut p = pending_r.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(P::closed_error()));
            }
        });

        Ok(Self {
            next_id: Mutex::new(1),
            pending,
            events_tx,
            write_tx,
            reader_handle: Some(reader_handle),
            writer_handle: Some(writer_handle),
        })
    }

    /// Allocate the next monotonically increasing request id.
    async fn next_id(&self) -> u64 {
        let mut n = self.next_id.lock().await;
        let id = *n;
        *n += 1;
        id
    }

    /// Send a request and await its correlated reply, bounded by
    /// [`REQUEST_TIMEOUT`]. On writer-channel failure the pending entry is
    /// removed before returning (no leak); on timeout it is likewise removed.
    /// A protocol error reply comes back as [`RequestError::Protocol`];
    /// transport faults (writer closed / channel dropped / serialization) as
    /// [`RequestError::Transport`]; a no-reply timeout as
    /// [`RequestError::Timeout`].
    ///
    /// Callers map these through their own classifier so the protocol-specific
    /// typing stays in `crate::cdp` / `crate::bidi`.
    #[allow(clippy::result_large_err)]
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
    ) -> std::result::Result<serde_json::Value, RequestError<P::ProtoError>> {
        let id = self.next_id().await;
        let text =
            P::encode_request(id, method, params, session_id).map_err(RequestError::Transport)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if self.write_tx.send(text).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(RequestError::Transport(anyhow!("writer task closed")));
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(RequestError::Protocol(e)),
            Ok(Err(_)) => Err(RequestError::Transport(anyhow!("response channel dropped"))),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RequestError::Timeout)
            }
        }
    }

    /// Subscribe to the broadcast event stream. Drop the receiver to
    /// unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<P::Event> {
        self.events_tx.subscribe()
    }

    /// Gracefully shut down: close the writer (flushing the socket), then abort
    /// and join the reader. Mirrors the previous `CdpClient::close`. Taking the
    /// handles here leaves `Drop` with nothing to abort.
    pub async fn close(mut self) {
        // Closing the writer channel lets the writer task drain and close the
        // socket before we await it.
        let (write_tx, _) = mpsc::unbounded_channel::<String>();
        let dead = std::mem::replace(&mut self.write_tx, write_tx);
        drop(dead);
        if let Some(h) = self.writer_handle.take() {
            let _ = h.await;
        }
        if let Some(h) = self.reader_handle.take() {
            h.abort();
            let _ = h.await;
        }
    }
}

impl<P: Protocol> Drop for WsRpc<P> {
    /// Ensure dropping the transport (e.g. a cached `Arc<CdpClient>` cleared by
    /// `switch_browser`) never leaks the parked reader/writer tasks, even when
    /// the graceful `close()` was not called.
    fn drop(&mut self) {
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
        if let Some(h) = self.writer_handle.take() {
            h.abort();
        }
    }
}

/// Truncate a frame to a bounded prefix for logging, so a multi-megabyte
/// screenshot payload or a runaway log line never floods the diagnostics.
fn truncate_frame(text: &str) -> std::borrow::Cow<'_, str> {
    const MAX: usize = 200;
    if text.len() <= MAX {
        std::borrow::Cow::Borrowed(text)
    } else {
        let end = text
            .char_indices()
            .take_while(|(i, _)| *i < MAX)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        std::borrow::Cow::Owned(format!("{}… ({} bytes total)", &text[..end], text.len()))
    }
}

/// Result of a [`WsRpc::request`]: either a protocol error reply, or a
/// transport-level fault (timeout / writer closed / serialization).
pub enum RequestError<E> {
    /// The peer replied with a protocol error (e.g. `CdpError` / `BidiError`).
    Protocol(E),
    /// The request exceeded [`REQUEST_TIMEOUT`] with no reply.
    Timeout,
    /// A transport fault: serialization failed, the writer task is gone, or the
    /// reply channel was dropped (typically a disconnect).
    Transport(anyhow::Error),
}
