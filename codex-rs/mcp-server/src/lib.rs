//! Prototype MCP server.
#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::Event;
use axum::response::sse::Sse;
use axum::routing::get;
use mcp_types::JSONRPCMessage;
use mcp_types::RequestId;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::{self};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod codex_tool_config;
mod codex_tool_runner;
mod error_code;
mod exec_approval;
pub(crate) mod message_processor;
mod outgoing_message;
mod patch_approval;

use crate::message_processor::MessageProcessor;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;

pub use crate::codex_tool_config::CodexToolCallParam;
pub use crate::codex_tool_config::CodexToolCallReplyParam;
pub use crate::exec_approval::ExecApprovalElicitRequestParams;
pub use crate::exec_approval::ExecApprovalResponse;
pub use crate::patch_approval::PatchApprovalElicitRequestParams;
pub use crate::patch_approval::PatchApprovalResponse;

/// Size of the bounded channels used to communicate between tasks. The value
/// is a balance between throughput and memory usage – 128 messages should be
/// plenty for an interactive CLI.
const CHANNEL_CAPACITY: usize = 128;

pub async fn run_main(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
) -> IoResult<()> {
    // Install a simple subscriber so `tracing` output is visible.  Users can
    // control the log level with `RUST_LOG`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Set up channels.
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<JSONRPCMessage>(CHANNEL_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();

    // Task: read from stdin, push to `incoming_tx`.
    let stdin_reader_handle = tokio::spawn({
        async move {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin);
            let mut lines = reader.lines();

            while let Some(line) = lines.next_line().await.unwrap_or_default() {
                match serde_json::from_str::<JSONRPCMessage>(&line) {
                    Ok(msg) => {
                        if incoming_tx.send(msg).await.is_err() {
                            // Receiver gone – nothing left to do.
                            break;
                        }
                    }
                    Err(e) => error!("Failed to deserialize JSONRPCMessage: {e}"),
                }
            }

            debug!("stdin reader finished (EOF)");
        }
    });

    // Parse CLI overrides once and derive the base Config eagerly so later
    // components do not need to work with raw TOML values.
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;
    let config = Config::load_with_cli_overrides(cli_kv_overrides, ConfigOverrides::default())
        .await
        .map_err(|e| {
            std::io::Error::new(ErrorKind::InvalidData, format!("error loading config: {e}"))
        })?;

    // Task: process incoming messages.
    let processor_handle = tokio::spawn({
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);
        let mut processor = MessageProcessor::new(
            outgoing_message_sender,
            codex_linux_sandbox_exe,
            std::sync::Arc::new(config),
        );
        async move {
            while let Some(msg) = incoming_rx.recv().await {
                match msg {
                    JSONRPCMessage::Request(r) => processor.process_request(r).await,
                    JSONRPCMessage::Response(r) => processor.process_response(r).await,
                    JSONRPCMessage::Notification(n) => processor.process_notification(n).await,
                    JSONRPCMessage::Error(e) => processor.process_error(e),
                }
            }

            info!("processor task exited (channel closed)");
        }
    });

    // Task: write outgoing messages to stdout.
    let stdout_writer_handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(outgoing_message) = outgoing_rx.recv().await {
            let msg: JSONRPCMessage = outgoing_message.into();
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    if let Err(e) = stdout.write_all(json.as_bytes()).await {
                        error!("Failed to write to stdout: {e}");
                        break;
                    }
                    if let Err(e) = stdout.write_all(b"\n").await {
                        error!("Failed to write newline to stdout: {e}");
                        break;
                    }
                }
                Err(e) => error!("Failed to serialize JSONRPCMessage: {e}"),
            }
        }

        info!("stdout writer exited (channel closed)");
    });

    // Wait for all tasks to finish.  The typical exit path is the stdin reader
    // hitting EOF which, once it drops `incoming_tx`, propagates shutdown to
    // the processor and then to the stdout task.
    let _ = tokio::join!(stdin_reader_handle, processor_handle, stdout_writer_handle);

    Ok(())
}

/// HTTP server state for MCP (streamable HTTP-like) transport.
struct HttpState {
    incoming_tx: mpsc::Sender<JSONRPCMessage>,
    /// Broadcast channel for SSE notifications/responses.
    sse_tx: broadcast::Sender<String>,
    /// Pending HTTP responders keyed by client request id.
    pending: tokio::sync::Mutex<HashMap<RequestId, tokio::sync::oneshot::Sender<String>>>,
    /// Single session id used for simple, single-tenant server mode.
    session_id: String,
}

/// Run the MCP server over HTTP on the specified bind address.
/// When running in this mode, requests are accepted via POST and responses/notifications
/// are returned synchronously as JSON. An SSE endpoint is exposed for clients that
/// prefer a streaming channel, though responses are still sent inline for simplicity.
pub async fn run_http_server(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
    host: String,
    port: u16,
) -> IoResult<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Channels to/from the Codex message processor.
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<JSONRPCMessage>(CHANNEL_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();

    // Shared HTTP state and simple single-session id.
    let (sse_tx, _sse_rx) = broadcast::channel::<String>(CHANNEL_CAPACITY);
    let session_id = Uuid::new_v4().to_string();
    let state = Arc::new(HttpState {
        incoming_tx,
        sse_tx,
        pending: tokio::sync::Mutex::new(HashMap::new()),
        session_id,
    });

    // Parse CLI overrides and derive base Config.
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;
    let config = Config::load_with_cli_overrides(cli_kv_overrides, ConfigOverrides::default())
        .await
        .map_err(|e| {
            std::io::Error::new(ErrorKind::InvalidData, format!("error loading config: {e}"))
        })?;

    // Task: process incoming messages.
    let processor_handle = tokio::spawn({
        let outgoing_for_processor = outgoing_tx.clone();
        let mut processor = MessageProcessor::new(
            OutgoingMessageSender::new(outgoing_for_processor),
            codex_linux_sandbox_exe.clone(),
            std::sync::Arc::new(config),
        );
        async move {
            while let Some(msg) = incoming_rx.recv().await {
                match msg {
                    JSONRPCMessage::Request(r) => processor.process_request(r).await,
                    JSONRPCMessage::Response(r) => processor.process_response(r).await,
                    JSONRPCMessage::Notification(n) => processor.process_notification(n).await,
                    JSONRPCMessage::Error(e) => processor.process_error(e),
                }
            }
            info!("processor task exited (channel closed)");
        }
    });

    // Task: bridge OutgoingMessage -> HTTP responders and SSE stream.
    let bridge_state = Arc::clone(&state);
    let bridge_handle = tokio::spawn(async move {
        while let Some(outgoing_message) = outgoing_rx.recv().await {
            let msg: JSONRPCMessage = outgoing_message.into();
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    // Try to fulfill an awaiting HTTP request by id.
                    let maybe_id = match &msg {
                        JSONRPCMessage::Response(r) => Some(r.id.clone()),
                        JSONRPCMessage::Error(e) => Some(e.id.clone()),
                        _ => None,
                    };

                    if let Some(id) = maybe_id {
                        let sender = {
                            let mut guard = bridge_state.pending.lock().await;
                            guard.remove(&id)
                        };
                        if let Some(tx) = sender {
                            let _ = tx.send(json.clone());
                        }
                    }

                    // Broadcast all messages to SSE subscribers as well.
                    let _ = bridge_state.sse_tx.send(json);
                }
                Err(e) => error!("Failed to serialize JSONRPCMessage for HTTP/SSE: {e}"),
            }
        }
        info!("HTTP bridge task exited (channel closed)");
    });

    // Build HTTP router.
    let app = Router::new()
        .route("/", get(handle_sse).post(handle_post))
        .route("/mcp", get(handle_sse).post(handle_post))
        .with_state(state);

    // Start server.
    let bind_addr: SocketAddr = format!("{host}:{port}").parse().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid bind address: {e}"),
        )
    })?;
    let listener = TcpListener::bind(bind_addr).await.map_err(|e| {
        std::io::Error::new(
            ErrorKind::AddrInUse,
            format!("failed to bind {bind_addr}: {e}"),
        )
    })?;
    info!("codex MCP server (HTTP) listening on {bind_addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| std::io::Error::other(format!("server error: {e}")))?;

    // Graceful shutdown of tasks if server exits.
    let _ = tokio::join!(processor_handle, bridge_handle);

    Ok(())
}

async fn handle_post(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Always echo a stable session id for the client to re-use.
    let mut response_headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&state.session_id) {
        response_headers.insert("mcp-session-id", value);
    }

    // Parse the incoming message and forward to the processor.
    let msg: JSONRPCMessage = match serde_json::from_value(body) {
        Ok(m) => m,
        Err(err) => {
            error!("Failed to parse JSONRPCMessage from HTTP POST: {err}");
            return (
                StatusCode::BAD_REQUEST,
                response_headers,
                "invalid JSON-RPC",
            )
                .into_response();
        }
    };

    // Optionally honor the provided session id (single-session server: ignore).
    let _client_session = headers.get("mcp-session-id").and_then(|v| v.to_str().ok());

    match msg.clone() {
        JSONRPCMessage::Request(r) => {
            // Track a responder for this id and forward.
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            {
                let mut guard = state.pending.lock().await;
                guard.insert(r.id.clone(), tx);
            }
            if state
                .incoming_tx
                .send(JSONRPCMessage::Request(r))
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    response_headers,
                    "processor unavailable",
                )
                    .into_response();
            }

            // Wait for the matching response serialized as a JSON string.
            match rx.await {
                Ok(json) => (
                    StatusCode::OK,
                    response_headers,
                    Json(serde_json::from_str::<serde_json::Value>(&json).unwrap_or_default()),
                )
                    .into_response(),
                Err(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    response_headers,
                    "no response",
                )
                    .into_response(),
            }
        }
        // Forward and acknowledge without waiting.
        JSONRPCMessage::Response(r) => {
            let _ = state.incoming_tx.send(JSONRPCMessage::Response(r)).await;
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
        JSONRPCMessage::Notification(n) => {
            let _ = state
                .incoming_tx
                .send(JSONRPCMessage::Notification(n))
                .await;
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
        JSONRPCMessage::Error(e) => {
            let _ = state.incoming_tx.send(JSONRPCMessage::Error(e)).await;
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
    }
}

async fn handle_sse(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    // If client provides a session id ensure it matches (single-session server).
    if let Some(id) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok())
        && id != state.session_id
    {
        debug!("client provided mismatched session id; proceeding with server session");
    }

    let rx = state.sse_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|res| {
        let json = res.unwrap_or_default();
        let evt = Event::default().data(json);
        Ok::<Event, Infallible>(evt)
    });
    Sse::new(stream)
}
