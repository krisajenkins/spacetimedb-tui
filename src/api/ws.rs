//! WebSocket client for real-time SpacetimeDB subscriptions and log streaming.
//!
//! [`WsClient`] connects to the SpacetimeDB WebSocket endpoint and forwards
//! decoded messages over a [`tokio::sync::mpsc`] channel so that the TUI
//! event loop can consume them without blocking.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Error as TungError, Message,
        handshake::client::{Request, generate_key},
        protocol::WebSocketConfig,
    },
};
use tracing::{debug, error, info, warn};
use url::Url;

use super::types::{LogEntry, WsServerMessage};

// ---------------------------------------------------------------------------
// Public message enum delivered to the TUI
// ---------------------------------------------------------------------------

/// Messages produced by the WebSocket background task and sent over the mpsc
/// channel to the TUI event loop.
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// A decoded server message (subscription update, identity token, …).
    ServerMessage(WsServerMessage),
    /// A log line streamed from the server (log-follow mode).
    LogLine(LogEntry),
    /// Raw text frame that could not be decoded as a known message type.
    /// The inner `String` is preserved for diagnostic logging.
    RawText(String),
    /// The WebSocket connection was successfully established.
    Connected,
    /// The connection was closed. The task will transition to a
    /// `Reconnecting` state automatically unless the reason carries the
    /// `(retries disabled)` marker, in which case it is shutting down for
    /// good. `graceful` is `true` when the server closed the connection
    /// cleanly with a Close frame — the expected behaviour when a module is
    /// republished — as opposed to an abrupt drop or socket error.
    Disconnected { reason: String, graceful: bool },
    /// The task is waiting `delay_ms` before its next reconnect attempt.
    /// `attempt` is 1-indexed.
    Reconnecting { attempt: u32, delay_ms: u64 },
    /// A non-fatal error occurred (e.g. a single bad frame).
    Error(String),
}

// ---------------------------------------------------------------------------
// Client-to-server message types
// ---------------------------------------------------------------------------

/// A subscription request sent to the server.
///
/// SpacetimeDB's `v1.json.spacetimedb` subprotocol uses SATS externally
/// tagged enums for `ClientMessage`, so the on-the-wire form is
/// `{"Subscribe": {"query_strings": [...], "request_id": N}}` rather than
/// the internally-tagged `{"type": "Subscribe", ...}` we sent previously.
/// Getting this wrong caused the server to reply with a long error Close
/// frame, which in turn tripped tungstenite's "Control frame too big"
/// check and wedged the reconnect loop.
#[derive(Debug, Serialize)]
struct SubscribeEnvelope {
    #[serde(rename = "Subscribe")]
    subscribe: SubscribePayload,
}

#[derive(Debug, Serialize)]
struct SubscribePayload {
    query_strings: Vec<String>,
    request_id: u32,
}

impl SubscribeEnvelope {
    fn new(queries: Vec<String>, request_id: u32) -> Self {
        Self {
            subscribe: SubscribePayload {
                query_strings: queries,
                request_id,
            },
        }
    }
}

/// A reducer call request (reserved for future use).
#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct CallReducerMessage {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub reducer: String,
    pub args: serde_json::Value,
    pub request_id: u32,
}

// ---------------------------------------------------------------------------
// WsClient
// ---------------------------------------------------------------------------

/// Configuration for a WebSocket connection.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// WebSocket base URL, e.g. `ws://localhost:3000`.
    pub base_url: String,
    /// Database / module name.
    pub database: String,
    /// Optional bearer token for authentication.
    pub auth_token: Option<String>,
    /// Capacity of the mpsc channel buffer.
    pub channel_capacity: usize,
}

impl WsConfig {
    /// Build the full WebSocket URL for a subscription connection.
    pub fn subscription_url(&self) -> Result<Url> {
        let raw = format!("{}/v1/database/{}/subscribe", self.base_url, self.database);
        Url::parse(&raw).with_context(|| format!("Invalid WebSocket URL: {raw}"))
    }

    /// Build the full WebSocket URL for a log-follow connection.
    ///
    /// Used by [`spawn_log_follow`] for streaming live log output.
    #[allow(dead_code)]
    pub fn log_follow_url(&self) -> Result<Url> {
        let raw = format!(
            "{}/v1/database/{}/logs?follow=true&num_lines=100",
            self.base_url, self.database
        );
        Url::parse(&raw).with_context(|| format!("Invalid log follow URL: {raw}"))
    }
}

/// A handle to a running WebSocket background task.
///
/// Dropping this handle does **not** automatically close the connection;
/// call [`WsHandle::close`] explicitly or drop the underlying task.
#[derive(Debug)]
pub struct WsHandle {
    /// Send commands to the background task.
    cmd_tx: mpsc::Sender<WsCommand>,
    /// Receive events from the background task.
    pub event_rx: mpsc::Receiver<WsEvent>,
}

impl WsHandle {
    /// Send a subscription request to the server.
    pub async fn subscribe(&self, queries: Vec<String>, request_id: u32) -> Result<()> {
        self.cmd_tx
            .send(WsCommand::Subscribe {
                queries,
                request_id,
            })
            .await
            .context("WebSocket task has shut down")
    }

    /// Request a graceful shutdown of the background task.
    pub async fn close(&self) {
        if self.cmd_tx.send(WsCommand::Close).await.is_err() {
            tracing::debug!("WS close: command channel already dropped");
        }
    }
}

/// Commands sent from the TUI to the WebSocket background task.
#[derive(Debug)]
enum WsCommand {
    Subscribe {
        queries: Vec<String>,
        request_id: u32,
    },
    Close,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Spawn a WebSocket subscription task for `config.database`.
///
/// Returns a [`WsHandle`] through which the caller can send subscription
/// requests and receive [`WsEvent`]s.
pub fn spawn_subscription(config: WsConfig) -> Result<WsHandle> {
    let url = config.subscription_url()?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(32);
    let (event_tx, event_rx) = mpsc::channel::<WsEvent>(config.channel_capacity);

    tokio::spawn(subscription_task(
        url,
        config.auth_token.clone(),
        cmd_rx,
        event_tx,
    ));

    Ok(WsHandle { cmd_tx, event_rx })
}

/// Spawn a WebSocket log-follow task for `config.database`.
///
/// Log lines are forwarded as [`WsEvent::LogLine`] events.
/// Available for future integration with the Logs tab live streaming.
#[allow(dead_code)]
pub fn spawn_log_follow(config: WsConfig) -> Result<WsHandle> {
    let url = config.log_follow_url()?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(32);
    let (event_tx, event_rx) = mpsc::channel::<WsEvent>(config.channel_capacity);

    tokio::spawn(log_follow_task(
        url,
        config.auth_token.clone(),
        cmd_rx,
        event_tx,
    ));

    Ok(WsHandle { cmd_tx, event_rx })
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Build an HTTP upgrade request with optional auth header.
///
/// When we pass a raw `Request` (rather than a URL string) to
/// `connect_async`, tungstenite treats every header as caller-supplied and
/// will not auto-populate the mandatory WebSocket handshake headers. We have
/// to set `Host`, `Connection`, `Upgrade`, `Sec-WebSocket-Version` and a
/// fresh `Sec-WebSocket-Key` ourselves — otherwise the handshake fails with
/// "Missing, duplicated or incorrect header sec-websocket-key".
fn build_ws_request(url: Url, auth_token: Option<&str>) -> Result<Request> {
    let host = url
        .host_str()
        .context("WebSocket URL is missing a host component")?;
    let host_header = match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };

    let mut builder = Request::builder()
        .method("GET")
        .uri(url.as_str())
        .header("Host", host_header)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        // Request JSON encoding so frames can be decoded with serde_json.
        // SpacetimeDB 2.0 supports both "v1.bsatn.spacetimedb" (binary) and
        // "v1.json.spacetimedb" (JSON). We use JSON to avoid a BSATN decoder.
        .header("Sec-WebSocket-Protocol", "v1.json.spacetimedb");

    if let Some(token) = auth_token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }

    builder
        .body(())
        .context("Failed to build WebSocket upgrade request")
}

/// Outcome of one connection attempt inside the retry loop.
enum ConnectOutcome {
    /// The user (or app) requested a graceful shutdown — exit the retry loop.
    Closed,
    /// The connection was lost transiently; the outer loop should sleep
    /// with backoff and try again (e.g. the server was restarted, the
    /// stream timed out, a socket-level error fired). `graceful` is `true`
    /// when the server closed cleanly with a Close frame (the normal
    /// republish path) rather than dropping unexpectedly.
    Lost { reason: String, graceful: bool },
    /// The connection failed with a permanent server-side error such as
    /// HTTP 401 / 403 / 404 / 5xx. Retrying won't help and would just
    /// spam the log, so the outer loop should exit immediately with a
    /// clear disconnected reason.
    Fatal(String),
}

const RECONNECT_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const RECONNECT_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// WebSocket protocol limits for SpacetimeDB connections.
///
/// tungstenite defaults to a 64 MiB max message size and a 16 MiB max frame
/// size. A subscription's *initial* update is a full snapshot of every
/// matched table, which routinely blows past those caps on a busy database —
/// tungstenite then surfaces it as `Error::Capacity` ("Space limit exceeded:
/// Message too long"), kills the frame, and the reconnect churn corrupts the
/// TUI render. We're reading from a database the user already trusts, so we
/// lift both caps entirely (`None`) and let the snapshot through.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None)
}

/// Main loop for a subscription WebSocket connection — wraps a single-attempt
/// connection in an exponential-backoff retry loop. The retry loop exits only
/// when the consumer drops the event channel or sends `WsCommand::Close`.
async fn subscription_task(
    url: Url,
    auth_token: Option<String>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    event_tx: mpsc::Sender<WsEvent>,
) {
    let mut backoff = RECONNECT_INITIAL_DELAY;
    let mut attempt: u32 = 0;
    // Re-applied after every reconnect so the user doesn't have to manually
    // re-subscribe when the server bounces.
    let mut last_subscription: Option<(Vec<String>, u32)> = None;

    loop {
        attempt += 1;
        match connect_subscription_once(
            url.clone(),
            auth_token.as_deref(),
            &mut cmd_rx,
            &event_tx,
            &mut last_subscription,
        )
        .await
        {
            ConnectOutcome::Closed => {
                info!("WebSocket subscription task exiting (closed)");
                return;
            }
            ConnectOutcome::Fatal(reason) => {
                // Permanent server-side refusal — surface it once and
                // walk out of the retry loop. The user can hit Ctrl+R
                // (or switch databases) to try again.
                warn!("WebSocket subscription aborted: {reason}");
                let _ = event_tx
                    .send(WsEvent::Disconnected {
                        reason: format!("{reason} (retries disabled)"),
                        graceful: false,
                    })
                    .await;
                return;
            }
            ConnectOutcome::Lost { reason, graceful } => {
                // A clean server close (module republish) is expected — log it
                // quietly at info level. An unexpected drop is a warning.
                if graceful {
                    info!("WebSocket closed by server: {reason}");
                } else {
                    warn!("WebSocket connection lost: {reason}");
                }
                let _ = event_tx
                    .send(WsEvent::Disconnected { reason, graceful })
                    .await;
                // If the consumer is gone, abort the retry loop too.
                if event_tx.is_closed() {
                    return;
                }
                // A graceful close means the server bounced us on purpose
                // (typically a republish); reconnect briskly from the initial
                // delay rather than carrying over a long accumulated backoff.
                if graceful {
                    backoff = RECONNECT_INITIAL_DELAY;
                }
                let delay_ms = backoff.as_millis() as u64;
                let _ = event_tx
                    .send(WsEvent::Reconnecting { attempt, delay_ms })
                    .await;

                // Wait, but break early if a Close arrives during the sleep.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    cmd = cmd_rx.recv() => {
                        if matches!(cmd, Some(WsCommand::Close) | None) {
                            return;
                        }
                    }
                }
                backoff = (backoff * 2).min(RECONNECT_MAX_DELAY);
            }
        }
    }
}

/// One connection attempt for the subscription task. Returns when the
/// connection is lost or the consumer asks for a shutdown.
async fn connect_subscription_once(
    url: Url,
    auth_token: Option<&str>,
    cmd_rx: &mut mpsc::Receiver<WsCommand>,
    event_tx: &mpsc::Sender<WsEvent>,
    last_subscription: &mut Option<(Vec<String>, u32)>,
) -> ConnectOutcome {
    info!("Connecting to subscription WebSocket: {}", url);

    let request = match build_ws_request(url.clone(), auth_token) {
        Ok(r) => r,
        Err(e) => {
            return ConnectOutcome::Lost {
                reason: format!("Request build error: {e}"),
                graceful: false,
            };
        }
    };

    let (ws_stream, _) = match connect_async_with_config(request, Some(ws_config()), false).await {
        Ok(pair) => pair,
        Err(e) => {
            // Classify the handshake failure. A server-side HTTP error
            // (401 / 403 / 404 / 5xx) is almost always permanent for a
            // given database: retrying it would just spam the logs
            // every second forever. A socket-level error (connection
            // refused, reset, DNS) is transient — worth a backoff retry.
            if let TungError::Http(resp) = &e {
                let status = resp.status();
                if status.is_client_error() || status.is_server_error() {
                    error!("WebSocket handshake returned HTTP {status}");
                    return ConnectOutcome::Fatal(format!("Server returned HTTP {status}"));
                }
            }
            error!("WebSocket connect failed: {e}");
            return ConnectOutcome::Lost {
                reason: format!("Connect error: {e}"),
                graceful: false,
            };
        }
    };

    info!("WebSocket connected: {}", url);
    if event_tx.send(WsEvent::Connected).await.is_err() {
        return ConnectOutcome::Closed;
    }

    let (mut sink, mut stream) = ws_stream.split();

    // After a reconnect, automatically re-subscribe with the queries the
    // user issued before the connection dropped.
    if let Some((queries, request_id)) = last_subscription.clone() {
        let msg = SubscribeEnvelope::new(queries, request_id);
        if let Ok(json) = serde_json::to_string(&msg) {
            if let Err(e) = sink.send(Message::Text(json.into())).await {
                return ConnectOutcome::Lost {
                    reason: format!("Re-subscribe send error: {e}"),
                    graceful: false,
                };
            }
        }
    }

    loop {
        tokio::select! {
            // Inbound frames from the server.
            msg = stream.next() => {
                match msg {
                    // A Close frame is the server shutting us down cleanly —
                    // the normal path when a module is republished. Treat it as
                    // a graceful loss and return immediately so we don't also
                    // emit a second "stream ended" event when `next()` yields
                    // `None` on the following iteration.
                    Some(Ok(Message::Close(cf))) => {
                        let reason = cf
                            .as_ref()
                            .map(|f| f.reason.to_string())
                            .filter(|r| !r.is_empty())
                            .unwrap_or_else(|| "module exited".to_string());
                        info!("Server sent Close frame: {reason}");
                        return ConnectOutcome::Lost {
                            reason,
                            graceful: true,
                        };
                    }
                    Some(Ok(frame)) => {
                        if let Some(event) = decode_subscription_frame(frame) {
                            if event_tx.send(event).await.is_err() {
                                debug!("Event receiver dropped; closing WebSocket task");
                                return ConnectOutcome::Closed;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket frame error: {e}");
                        if event_tx.send(WsEvent::Error(e.to_string())).await.is_err() {
                            debug!("WS error event dropped — receiver gone");
                            return ConnectOutcome::Closed;
                        }
                        // A fatal error will surface as `None` on the next
                        // iteration; transient frame errors are tolerated.
                    }
                    None => {
                        // The stream ended without a Close frame — an abrupt
                        // drop (TCP reset, server killed). Not graceful.
                        info!("WebSocket stream ended without a Close frame");
                        return ConnectOutcome::Lost {
                            reason: "Connection dropped".to_string(),
                            graceful: false,
                        };
                    }
                }
            }

            // Commands from the TUI.
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCommand::Subscribe { queries, request_id }) => {
                        let msg = SubscribeEnvelope::new(queries.clone(), request_id);
                        *last_subscription = Some((queries, request_id));
                        let json = match serde_json::to_string(&msg) {
                            Ok(j) => j,
                            Err(e) => {
                                warn!("Failed to serialise Subscribe message: {e}");
                                continue;
                            }
                        };
                        if let Err(e) = sink.send(Message::Text(json.into())).await {
                            error!("Failed to send Subscribe frame: {e}");
                            return ConnectOutcome::Lost {
                                reason: format!("Send error: {e}"),
                                graceful: false,
                            };
                        }
                    }
                    Some(WsCommand::Close) | None => {
                        info!("WebSocket task received close command");
                        if let Err(e) = sink.send(Message::Close(None)).await {
                            debug!("WS close frame send failed: {e}");
                        }
                        if event_tx
                            .send(WsEvent::Disconnected {
                                reason: "Client requested close".to_string(),
                                graceful: true,
                            })
                            .await
                            .is_err()
                        {
                            debug!("WS disconnect event dropped — receiver gone");
                        }
                        return ConnectOutcome::Closed;
                    }
                }
            }
        }
    }
}

/// Main loop for a log-follow WebSocket connection.
#[allow(dead_code)]
async fn log_follow_task(
    url: Url,
    auth_token: Option<String>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    event_tx: mpsc::Sender<WsEvent>,
) {
    info!("Connecting to log-follow WebSocket: {}", url);

    let request = match build_ws_request(url.clone(), auth_token.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx
                .send(WsEvent::Disconnected {
                    reason: format!("Request build error: {e}"),
                    graceful: false,
                })
                .await;
            return;
        }
    };

    let (ws_stream, _) = match connect_async_with_config(request, Some(ws_config()), false).await {
        Ok(pair) => pair,
        Err(e) => {
            error!("Log WebSocket connect failed: {e}");
            let _ = event_tx
                .send(WsEvent::Disconnected {
                    reason: format!("Connect error: {e}"),
                    graceful: false,
                })
                .await;
            return;
        }
    };

    info!("Log WebSocket connected: {}", url);
    let _ = event_tx.send(WsEvent::Connected).await;

    let (mut sink, mut stream) = ws_stream.split();

    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(frame)) => {
                        match frame {
                            Message::Text(text) => {
                                let text_str = text.as_str();
                                match serde_json::from_str::<LogEntry>(text_str) {
                                    Ok(entry) => {
                                        if event_tx.send(WsEvent::LogLine(entry)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        // Not a structured log entry — forward as raw text.
                                        if event_tx
                                            .send(WsEvent::RawText(text_str.to_owned()))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                            Message::Close(_) => {
                                let _ = event_tx
                                    .send(WsEvent::Disconnected {
                                        reason: "Server closed log stream".to_string(),
                                        graceful: true,
                                    })
                                    .await;
                                break;
                            }
                            Message::Ping(data) => {
                                // Respond to pings to keep the connection alive.
                                let _ = sink.send(Message::Pong(data)).await;
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => {
                        warn!("Log WebSocket frame error: {e}");
                        let _ = event_tx.send(WsEvent::Error(e.to_string())).await;
                    }
                    None => {
                        let _ = event_tx
                            .send(WsEvent::Disconnected {
                                reason: "Log stream ended".to_string(),
                                graceful: false,
                            })
                            .await;
                        break;
                    }
                }
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCommand::Close) | None => {
                        let _ = sink.send(Message::Close(None)).await;
                        let _ = event_tx
                            .send(WsEvent::Disconnected {
                                reason: "Client requested close".to_string(),
                                graceful: true,
                            })
                            .await;
                        break;
                    }
                    _ => {} // Log-follow doesn't handle Subscribe commands.
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frame decoders
// ---------------------------------------------------------------------------

/// Decode an inbound WebSocket frame from the subscription endpoint.
fn decode_subscription_frame(frame: Message) -> Option<WsEvent> {
    match frame {
        Message::Text(text) => {
            let text_str = text.as_str();
            match serde_json::from_str::<WsServerMessage>(text_str) {
                Ok(msg) => Some(WsEvent::ServerMessage(msg)),
                Err(e) => {
                    debug!("Could not decode server message: {e} — raw: {}", text_str);
                    Some(WsEvent::RawText(text_str.to_owned()))
                }
            }
        }
        Message::Binary(bytes) => {
            // BSATN binary frames — attempt UTF-8 fallback for diagnostics.
            match std::str::from_utf8(&bytes) {
                Ok(s) => Some(WsEvent::RawText(s.to_owned())),
                Err(_) => {
                    debug!("Received {} binary bytes (BSATN)", bytes.len());
                    None
                }
            }
        }
        Message::Ping(_) | Message::Pong(_) => None,
        Message::Close(frame) => {
            // Note: the subscription loop intercepts Close frames before
            // calling this decoder (see `connect_subscription_once`), so this
            // arm is effectively unreachable there. Kept for completeness.
            let reason = frame
                .as_ref()
                .map(|f| f.reason.to_string())
                .unwrap_or_else(|| "no reason".to_string());
            Some(WsEvent::Disconnected {
                reason,
                graceful: true,
            })
        }
        Message::Frame(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscription_url() {
        let cfg = WsConfig {
            base_url: "ws://localhost:3000".to_string(),
            database: "mydb".to_string(),
            auth_token: None,
            channel_capacity: 64,
        };
        let url = cfg.subscription_url().unwrap();
        assert_eq!(
            url.as_str(),
            "ws://localhost:3000/v1/database/mydb/subscribe"
        );
    }

    #[test]
    fn test_log_follow_url() {
        let cfg = WsConfig {
            base_url: "ws://localhost:3000".to_string(),
            database: "mydb".to_string(),
            auth_token: None,
            channel_capacity: 64,
        };
        let url = cfg.log_follow_url().unwrap();
        assert!(url.as_str().contains("/v1/database/mydb/logs"));
        assert!(url.as_str().contains("follow=true"));
    }

    #[test]
    fn test_build_ws_request_no_auth() {
        let url = Url::parse("ws://localhost:3000/v1/database/test/subscribe").unwrap();
        let req = build_ws_request(url, None).unwrap();
        assert!(req.headers().get("Authorization").is_none());
    }

    #[test]
    fn test_build_ws_request_with_auth() {
        let url = Url::parse("ws://localhost:3000/v1/database/test/subscribe").unwrap();
        let req = build_ws_request(url, Some("mytoken")).unwrap();
        let auth = req.headers().get("Authorization").unwrap();
        assert_eq!(auth, "Bearer mytoken");
    }

    #[test]
    fn connect_outcome_fatal_is_distinct_from_lost() {
        // Sanity check that the discriminants don't accidentally
        // collapse — Fatal must round-trip through `matches!` so the
        // retry loop in subscription_task can reliably skip backoff.
        let lost = ConnectOutcome::Lost {
            reason: "transient".to_string(),
            graceful: false,
        };
        let fatal = ConnectOutcome::Fatal("HTTP 500".to_string());
        assert!(matches!(lost, ConnectOutcome::Lost { .. }));
        assert!(matches!(fatal, ConnectOutcome::Fatal(_)));
        assert!(!matches!(lost, ConnectOutcome::Fatal(_)));
        assert!(!matches!(fatal, ConnectOutcome::Lost { .. }));
    }

    #[test]
    fn test_subscribe_envelope_json_format() {
        // SpacetimeDB's v1.json.spacetimedb protocol uses SATS externally
        // tagged enums for ClientMessage. If the format drifts back to
        // `{"type":"Subscribe",...}` the server rejects the message and
        // replies with an oversized Close frame, which tungstenite surfaces
        // as "Control frame too big". Guard against that regression.
        let env = SubscribeEnvelope::new(vec!["SELECT * FROM users".to_string()], 7);
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(
            json,
            r#"{"Subscribe":{"query_strings":["SELECT * FROM users"],"request_id":7}}"#
        );
    }

    #[test]
    fn test_ws_server_message_identity_token_parses() {
        // Server sends `{"IdentityToken": {...}}`, externally tagged.
        let payload = r#"{
            "IdentityToken": {
                "identity": "0xdeadbeef",
                "token": "abc",
                "connection_id": "0xfeed"
            }
        }"#;
        let decoded: WsServerMessage = serde_json::from_str(payload).unwrap();
        assert!(matches!(decoded, WsServerMessage::IdentityToken(_)));
    }

    #[test]
    fn test_ws_server_message_unknown_variant_is_rejected() {
        // Future server versions may add new variants; we don't map them
        // to a catch-all (externally tagged `#[serde(other)]` doesn't
        // tolerate map payloads), but `decode_subscription_frame` converts
        // the decode error into a `RawText` event so the connection stays
        // open.
        let payload = r#"{"SomeBrandNewMessage": {"foo": 1}}"#;
        let decoded: Result<WsServerMessage, _> = serde_json::from_str(payload);
        assert!(decoded.is_err(), "expected unknown variant to fail decode");
    }

    #[test]
    fn test_build_ws_request_has_handshake_headers() {
        // Regression test for the handshake failure
        // "Missing, duplicated or incorrect header sec-websocket-key":
        // when tungstenite is handed a raw Request it does not auto-populate
        // these headers, so we must set them ourselves.
        let url = Url::parse("ws://localhost:3000/v1/database/test/subscribe").unwrap();
        let req = build_ws_request(url, None).unwrap();
        let headers = req.headers();
        assert_eq!(headers.get("Host").unwrap(), "localhost:3000");
        assert_eq!(headers.get("Connection").unwrap(), "Upgrade");
        assert_eq!(headers.get("Upgrade").unwrap(), "websocket");
        assert_eq!(headers.get("Sec-WebSocket-Version").unwrap(), "13");
        assert!(
            headers.get("Sec-WebSocket-Key").is_some(),
            "Sec-WebSocket-Key is required for the tungstenite handshake"
        );
        assert_eq!(
            headers.get("Sec-WebSocket-Protocol").unwrap(),
            "v1.json.spacetimedb"
        );
    }
}
