use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::ACCEPT;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::Response;
use axum::routing::post;
use mcp_types::CallToolRequestParams;
use mcp_types::CallToolResult;
use mcp_types::ContentBlock;
use mcp_types::InitializeResult;
use mcp_types::ListToolsResult;
use mcp_types::ServerCapabilities;
use mcp_types::ServerCapabilitiesTools;
use mcp_types::Tool;
use mcp_types::ToolInputSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::task;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    tools: Arc<Vec<Tool>>,
    session_id: Arc<Mutex<Option<String>>>,
}

type HttpResult = Result<Response, (StatusCode, String)>;

const JSONRPC_VERSION: &str = "2.0";

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind_addr = parse_bind_addr()?;
    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "failed to bind to {bind_addr}: {err}. make sure the process has network access"
            );
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    eprintln!("starting rmcp streamable http test server on http://{bind_addr}/mcp");

    let state = AppState {
        tools: Arc::new(vec![echo_tool()]),
        session_id: Arc::new(Mutex::new(None)),
    };

    let router = Router::new()
        .route("/mcp", post(handle_post).get(handle_get))
        .with_state(state);

    axum::serve(listener, router).await?;
    task::yield_now().await;
    Ok(())
}

fn echo_tool() -> Tool {
    Tool {
        annotations: None,
        description: Some("Echo back the provided message and include environment data.".into()),
        input_schema: ToolInputSchema {
            properties: None,
            required: None,
            r#type: "object".into(),
        },
        name: "echo".into(),
        output_schema: None,
        title: None,
    }
}

async fn handle_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<JsonRpcRequest>,
) -> HttpResult {
    let accept_header = headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    if !accept_header.contains("text/event-stream") {
        return Err((
            StatusCode::NOT_ACCEPTABLE,
            "Client must accept text/event-stream".into(),
        ));
    }

    match payload.method.as_str() {
        "initialize" => handle_initialize(state, &payload).await,
        "tools/list" => handle_list_tools(state, &payload, &headers).await,
        "tools/call" => handle_call_tool(state, &payload, &headers).await,
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("unsupported method: {other}"),
        )),
    }
}

async fn handle_initialize(state: AppState, payload: &JsonRpcRequest) -> HttpResult {
    let mut session_guard = state.session_id.lock().await;
    if session_guard.is_some() {
        // allow reinitialize by resetting session id
        *session_guard = None;
    }

    let session = Uuid::new_v4().to_string();
    *session_guard = Some(session.clone());

    let result = InitializeResult {
        capabilities: ServerCapabilities {
            completions: None,
            experimental: None,
            logging: None,
            prompts: None,
            resources: None,
            tools: Some(ServerCapabilitiesTools {
                list_changed: Some(true),
            }),
        },
        instructions: None,
        protocol_version: "2025-03-26".into(),
        server_info: mcp_types::Implementation {
            name: "rmcp".into(),
            version: "0.7.0".into(),
            title: None,
            user_agent: None,
        },
    };

    let payload_value = serde_json::to_value(&result)
        .map_err(|err| internal_error(format!("serialize initialize result: {err}")))?;
    sse_response_with_session(&payload.id, payload_value, Some(session))
}

async fn handle_list_tools(
    state: AppState,
    payload: &JsonRpcRequest,
    headers: &HeaderMap,
) -> HttpResult {
    verify_session(&state, headers).await?;

    let tools = state.tools.clone();
    let result = ListToolsResult {
        tools: (*tools).clone(),
        next_cursor: None,
    };
    let payload_value = serde_json::to_value(&result)
        .map_err(|err| internal_error(format!("serialize list tools result: {err}")))?;
    sse_response(&payload.id, payload_value)
}

async fn handle_call_tool(
    state: AppState,
    payload: &JsonRpcRequest,
    headers: &HeaderMap,
) -> HttpResult {
    verify_session(&state, headers).await?;

    let params_value = payload
        .params
        .clone()
        .ok_or_else(|| bad_request("missing params"))?;

    let params: CallToolRequestParams = serde_json::from_value(params_value)
        .map_err(|err| bad_request(format!("invalid call params: {err}")))?;

    if params.name != "echo" {
        return Err(bad_request(format!("unknown tool: {}", params.name)));
    }

    let message = params
        .arguments
        .and_then(|args| {
            args.get("message")
                .and_then(|value| value.as_str().map(str::to_owned))
        })
        .unwrap_or_default();

    let env_value = std::env::var("MCP_TEST_VALUE").ok();
    let structured_content = json!({
        "echo": format!("ECHOING: {}", message),
        "env": env_value,
    });

    let result = CallToolResult {
        content: Vec::<ContentBlock>::new(),
        structured_content: Some(structured_content),
        is_error: Some(false),
    };

    let payload_value = serde_json::to_value(&result)
        .map_err(|err| internal_error(format!("serialize call tool result: {err}")))?;
    sse_response(&payload.id, payload_value)
}

async fn verify_session(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let header_value = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| bad_request("missing mcp-session-id"))?;

    let guard = state.session_id.lock().await;
    match guard.as_deref() {
        Some(session) if session == header_value => Ok(()),
        Some(_) => Err((StatusCode::UNAUTHORIZED, "invalid session".into())),
        None => Err((StatusCode::UNAUTHORIZED, "session not initialized".into())),
    }
}

fn sse_response(id: &serde_json::Value, result: serde_json::Value) -> HttpResult {
    sse_response_with_session(id, result, None)
}

fn sse_response_with_session(
    id: &serde_json::Value,
    result: serde_json::Value,
    session: Option<String>,
) -> HttpResult {
    let payload = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    });

    let body = format!("data: {payload}\n\n");
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .map_err(|err| internal_error(format!("build SSE response: {err}")))?;

    if let Some(session) = session {
        let header_value = HeaderValue::from_str(&session)
            .map_err(|err| internal_error(format!("invalid session id header: {err}")))?;
        response
            .headers_mut()
            .insert("mcp-session-id", header_value);
    }

    Ok(response)
}

async fn handle_get() -> HttpResult {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(Body::empty())
        .map_err(|err| internal_error(format!("build SSE get response: {err}")))
}

fn parse_bind_addr() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let default_addr = "127.0.0.1:3920";
    let bind_addr = std::env::var("MCP_STREAMABLE_HTTP_BIND_ADDR")
        .or_else(|_| std::env::var("BIND_ADDR"))
        .unwrap_or_else(|_| default_addr.to_string());
    Ok(bind_addr.parse()?)
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

fn internal_error(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, message.into())
}
