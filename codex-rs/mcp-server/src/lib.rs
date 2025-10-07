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
use codex_core::McpConnectionManager;
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
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

mod codex_tool_config;
mod codex_tool_runner;
mod error_code;
mod exec_approval;
pub(crate) mod message_processor;
mod outgoing_message;
mod patch_approval;
mod redact;
mod session_log;

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

/// Lazily install a `tracing` subscriber that writes to stderr and to
/// `CODEX_HOME/log/codex-rmcp.log`. This function is idempotent across calls.
fn init_tracing() -> IoResult<()> {
    // Compute log file path under CODEX_HOME/log.
    let mut log_dir = codex_core::config::find_codex_home()?;
    log_dir.push("log");

    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::never(log_dir, "codex-rmcp.log");
    let (file_writer, guard): (tracing_appender::non_blocking::NonBlocking, WorkerGuard) =
        tracing_appender::non_blocking(file_appender);

    // Leak the guard so the background worker stays alive for the process lifetime.
    let _ = Box::leak(Box::new(guard));

    let env_filter = EnvFilter::from_default_env();

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file_writer_clone = file_writer.clone();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(move || crate::redact::SanitizingWriter::new(file_writer_clone.clone()));

    // Ignore error if a global subscriber has already been set up.
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();

    Ok(())
}

async fn create_mcp_connection_manager(config: &Config) -> Arc<McpConnectionManager> {
    match McpConnectionManager::new(
        config.mcp_servers.clone(),
        config.use_experimental_use_rmcp_client,
    )
    .await
    {
        Ok((manager, failures)) => {
            if !failures.is_empty() {
                for (server_name, err) in failures {
                    error!("MCP client for `{server_name}` failed to start: {err:#}");
                }
            }
            Arc::new(manager)
        }
        Err(err) => {
            error!("Failed to create MCP connection manager: {err:#}");
            Arc::new(McpConnectionManager::default())
        }
    }
}

pub async fn run_main(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
) -> IoResult<()> {
    // Install logging to stderr and CODEX_HOME/log/codex-rmcp.log.
    init_tracing()?;

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

    let mcp_connection_manager = create_mcp_connection_manager(&config).await;
    let config = Arc::new(config);

    // Task: process incoming messages.
    let processor_handle = tokio::spawn({
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);
        let mut processor = MessageProcessor::new(
            "stdio".to_string(),
            outgoing_message_sender,
            codex_linux_sandbox_exe,
            Arc::clone(&config),
            Arc::clone(&mcp_connection_manager),
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
struct SessionState {
    incoming_tx: mpsc::Sender<JSONRPCMessage>,
    /// Broadcast channel for SSE notifications/responses for this session.
    sse_tx: broadcast::Sender<String>,
    /// Pending HTTP responders keyed by client request id for this session.
    pending: tokio::sync::Mutex<HashMap<RequestId, tokio::sync::oneshot::Sender<String>>>,
}

/// Shared HTTP server state managing multiple concurrent sessions.
struct HttpMultiState {
    sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionState>>>,
    /// Fixed process-wide components used to create new sessions.
    codex_linux_sandbox_exe: Option<PathBuf>,
    config: Arc<Config>,
    mcp_connection_manager: Arc<McpConnectionManager>,
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
    init_tracing()?;

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

    let mcp_connection_manager = create_mcp_connection_manager(&config).await;
    let config = Arc::new(config);

    // Shared HTTP state (multi-session).
    let state = Arc::new(HttpMultiState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        codex_linux_sandbox_exe,
        config: Arc::clone(&config),
        mcp_connection_manager,
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
    // No global tasks to await; sessions own their tasks.

    Ok(())
}

async fn handle_post(
    State(state): State<Arc<HttpMultiState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Parse the incoming message and forward to the processor for this session.
    let msg: JSONRPCMessage = match serde_json::from_value(body) {
        Ok(m) => m,
        Err(err) => {
            error!("Failed to parse JSONRPCMessage from HTTP POST: {err}");
            return (
                StatusCode::BAD_REQUEST,
                HeaderMap::new(),
                "invalid JSON-RPC",
            )
                .into_response();
        }
    };

    // Resolve or create a session based on optional header.
    let mut response_headers = HeaderMap::new();
    let client_session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);
    let (session_id, session) = ensure_session(Arc::clone(&state), client_session).await;
    if let Ok(value) = HeaderValue::from_str(&session_id) {
        response_headers.insert("mcp-session-id", value);
    }

    match msg.clone() {
        JSONRPCMessage::Request(r) => {
            // If client prefers SSE for the POST, stream the response and any
            // notifications over SSE for low latency.
            let accept_header = headers
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_lowercase();
            let wants_sse = accept_header.contains("text/event-stream");
            mcp_session_info!(session_id, "HTTP POST: request wants_sse={}", wants_sse);

            if wants_sse {
                // Subscribe first to avoid missing early events, then forward request.
                let rx = session.sse_tx.subscribe();
                if session
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

                let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|res| {
                    let json = res.unwrap_or_default();
                    let evt = Event::default().data(json);
                    Ok::<Event, Infallible>(evt)
                });
                return (StatusCode::OK, response_headers, Sse::new(stream)).into_response();
            }

            // Default: keep legacy behavior and return JSON response inline.
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            {
                let mut guard = session.pending.lock().await;
                guard.insert(r.id.clone(), tx);
            }
            if session
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
            mcp_session_info!(session_id, "HTTP POST: response");
            let _ = session.incoming_tx.send(JSONRPCMessage::Response(r)).await;
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
        JSONRPCMessage::Notification(n) => {
            mcp_session_info!(session_id, "HTTP POST: notification");
            let _ = session
                .incoming_tx
                .send(JSONRPCMessage::Notification(n))
                .await;
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
        JSONRPCMessage::Error(e) => {
            mcp_session_info!(session_id, "HTTP POST: error");
            let _ = session.incoming_tx.send(JSONRPCMessage::Error(e)).await;
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
    }
}

async fn handle_sse(
    State(state): State<Arc<HttpMultiState>>,
    headers: HeaderMap,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    // Use client-provided session id or create a new one.
    let client_session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);
    let (session_id, session) = ensure_session(Arc::clone(&state), client_session).await;
    mcp_session_info!(session_id, "HTTP GET: SSE subscribe");

    let rx = session.sse_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|res| {
        let json = res.unwrap_or_default();
        let evt = Event::default().data(json);
        Ok::<Event, Infallible>(evt)
    });
    Sse::new(stream)
}

/// Ensure a session exists for the provided id or create a new one. Returns the
/// session id and an `Arc` to its state.
async fn ensure_session(
    state: Arc<HttpMultiState>,
    id: Option<String>,
) -> (String, Arc<SessionState>) {
    // Fast path: find existing without creating.
    if let Some(id) = id.clone()
        && let Some(existing) = state.sessions.lock().await.get(&id).cloned()
    {
        return (id, existing);
    }

    // Otherwise, create a new session id (or re-use provided) and wire tasks.
    let session_id = id.unwrap_or_else(|| Uuid::new_v4().to_string());

    // Double-checked insert: if another task created it in between, return that.
    {
        let existing = state.sessions.lock().await.get(&session_id).cloned();
        if let Some(existing) = existing {
            return (session_id, existing);
        }
    }

    let (incoming_tx, mut incoming_rx) = mpsc::channel::<JSONRPCMessage>(CHANNEL_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
    let (sse_tx, _sse_rx) = broadcast::channel::<String>(CHANNEL_CAPACITY);

    let session = Arc::new(SessionState {
        incoming_tx,
        sse_tx,
        pending: tokio::sync::Mutex::new(HashMap::new()),
    });

    // Spawn processor for this session.
    let mut processor = MessageProcessor::new(
        session_id.clone(),
        OutgoingMessageSender::new(outgoing_tx.clone()),
        state.codex_linux_sandbox_exe.clone(),
        Arc::clone(&state.config),
        Arc::clone(&state.mcp_connection_manager),
    );
    let session_id_for_proc = session_id.clone();
    let proc_span = info_span!("mcp_session", session_id = %session_id_for_proc);
    tokio::spawn(
        async move {
            while let Some(msg) = incoming_rx.recv().await {
                match msg {
                    JSONRPCMessage::Request(r) => processor.process_request(r).await,
                    JSONRPCMessage::Response(r) => processor.process_response(r).await,
                    JSONRPCMessage::Notification(n) => processor.process_notification(n).await,
                    JSONRPCMessage::Error(e) => processor.process_error(e),
                }
            }
            mcp_session_info!(
                session_id_for_proc,
                "processor task exited (channel closed)"
            );
        }
        .instrument(proc_span),
    );

    // Bridge outgoing -> pending/sse for this session.
    let session_for_bridge = Arc::clone(&session);
    let session_id_for_bridge = session_id.clone();
    let bridge_span = info_span!("mcp_session", session_id = %session_id_for_bridge);
    tokio::spawn(
        async move {
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
                                let mut guard = session_for_bridge.pending.lock().await;
                                guard.remove(&id)
                            };
                            if let Some(tx) = sender {
                                let _ = tx.send(json.clone());
                            }
                        }

                        // Broadcast all messages to SSE subscribers as well.
                        let _ = session_for_bridge.sse_tx.send(json);
                    }
                    Err(e) => mcp_session_error!(
                        session_id_for_bridge,
                        "Failed to serialize JSONRPCMessage for HTTP/SSE: {e}"
                    ),
                }
            }
            mcp_session_info!(
                session_id_for_bridge,
                "HTTP bridge task exited (channel closed)"
            );
        }
        .instrument(bridge_span),
    );

    // Publish into map and return.
    {
        let mut guard = state.sessions.lock().await;
        guard.insert(session_id.clone(), Arc::clone(&session));
    }

    (session_id, session)
}
