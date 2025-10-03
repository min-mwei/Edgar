use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use futures::StreamExt;
use mcp_types::CallToolRequestParams;
use mcp_types::CallToolResult;
use mcp_types::InitializeRequestParams;
use mcp_types::InitializeResult;
use mcp_types::ListToolsRequestParams;
use mcp_types::ListToolsResult;
use reqwest::Client;
use reqwest::header::ACCEPT;
use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sse_stream::SseStream;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time;
use tracing::debug;
use tracing::info;
use tracing::warn;

const JSONRPC_VERSION: &str = "2.0";
const SESSION_HEADER: &str = "mcp-session-id";
const ACCEPT_HEADER_VALUE: &str = "text/event-stream, application/json";
const SSE_ACCEPT_HEADER_VALUE: &str = "text/event-stream";

#[derive(Clone)]
pub struct StreamableHttpClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
pub struct RpcResponse<R> {
    pub result: R,
    pub notifications: Vec<Value>,
}

struct Inner {
    client: Client,
    url: reqwest::Url,
    /// Stored bearer token without the `Bearer ` prefix.
    auth_token: Option<String>,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    stream_task: Mutex<Option<JoinHandle<()>>>,
}

impl StreamableHttpClient {
    pub async fn connect(
        url: String,
        bearer_token: Option<String>,
        params: InitializeRequestParams,
        timeout: Duration,
    ) -> Result<Self> {
        let client = Client::builder()
            .http1_only()
            .build()
            .context("failed to build HTTP client for streamable MCP")?;

        let url = reqwest::Url::parse(&url)
            .with_context(|| format!("invalid MCP streamable HTTP URL `{url}`"))?;

        let auth_token = bearer_token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .map(|token| {
                token
                    .trim_start_matches("Bearer ")
                    .trim_start_matches("bearer ")
                    .to_owned()
            })
            .filter(|token| {
                if token.is_empty() {
                    warn!("ignoring empty bearer token for MCP server");
                    false
                } else {
                    true
                }
            });

        let client = Self {
            inner: Arc::new(Inner {
                client,
                url,
                auth_token,
                next_id: AtomicU64::new(1),
                session_id: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                stream_task: Mutex::new(None),
            }),
        };

        let initialize_response: RpcResponse<InitializeResult> = client
            .send_request(
                "initialize",
                Some(serde_json::to_value(params)?),
                timeout,
                false,
            )
            .await?;

        info!(
            protocol = %initialize_response.result.protocol_version,
            "streamable HTTP MCP handshake completed"
        );

        if !initialize_response.notifications.is_empty() {
            debug!(
                notification_count = initialize_response.notifications.len(),
                "received {} pre-handshake notifications",
                initialize_response.notifications.len()
            );
        }

        client.ensure_stream_listener().await?;

        Ok(client)
    }

    pub async fn list_tools(
        &self,
        params: Option<ListToolsRequestParams>,
        timeout: Duration,
    ) -> Result<ListToolsResult> {
        let payload = params.map(serde_json::to_value).transpose()?;
        let response: RpcResponse<ListToolsResult> = self
            .send_request("tools/list", payload, timeout, true)
            .await?;

        if !response.notifications.is_empty() {
            debug!(
                notification_count = response.notifications.len(),
                "list_tools received {} streaming notifications",
                response.notifications.len()
            );
        }

        Ok(response.result)
    }

    pub async fn call_tool(
        &self,
        params: CallToolRequestParams,
        timeout: Duration,
    ) -> Result<CallToolResult> {
        let response: RpcResponse<CallToolResult> = self
            .send_request(
                "tools/call",
                Some(serde_json::to_value(params)?),
                timeout,
                true,
            )
            .await?;

        if !response.notifications.is_empty() {
            debug!(
                notification_count = response.notifications.len(),
                "call_tool received {} streaming notifications",
                response.notifications.len()
            );
        }

        Ok(response.result)
    }

    async fn send_request<R: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        use_background_stream: bool,
    ) -> Result<RpcResponse<R>> {
        let id = self
            .inner
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();

        let pending_receiver = if use_background_stream {
            let (tx, rx) = oneshot::channel();
            self.inner.register_pending(id.clone(), tx).await;
            Some(rx)
        } else {
            None
        };

        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION,
            id: &id,
            method,
            params: params.as_ref(),
        };

        let mut http_request = self
            .inner
            .client
            .post(self.inner.url.clone())
            .header(ACCEPT, ACCEPT_HEADER_VALUE)
            .header(CONTENT_TYPE, "application/json")
            .json(&request);

        if let Some(token) = &self.inner.auth_token {
            http_request = http_request.bearer_auth(token);
        }

        if let Some(session_id) = self.current_session_id().await? {
            http_request = http_request.header(SESSION_HEADER, session_id);
        }

        let response = time::timeout(timeout, http_request.send())
            .await
            .context("timed out awaiting streamable HTTP response")??;

        self.maybe_update_session(&response).await?;

        if !response.status().is_success() {
            if use_background_stream {
                self.inner.remove_pending(&id).await;
            }
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read response body>".to_string());
            return Err(anyhow!(
                "streamable HTTP request failed with status {status}: {body}"
            ));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_lowercase();

        let expected_id = id.as_str();

        if content_type.starts_with("application/json") {
            let value: Value = response.json().await?;
            if use_background_stream {
                self.inner.remove_pending(&id).await;
            }
            self.parse_json_response(value, expected_id)
        } else if content_type.starts_with("text/event-stream") {
            match self
                .parse_sse_response(response, expected_id, timeout)
                .await
            {
                Err(error) => {
                    if use_background_stream {
                        self.inner.remove_pending(&id).await;
                    }
                    Err(error)
                }
                Ok(Some(result)) => {
                    if use_background_stream {
                        self.inner.remove_pending(&id).await;
                    }
                    Ok(result)
                }
                Ok(None) => {
                    let Some(rx) = pending_receiver else {
                        return Err(anyhow!("stream closed before receiving response"));
                    };
                    match time::timeout(timeout, rx).await {
                        Ok(Ok(value)) => self.parse_json_response(value, expected_id),
                        Ok(Err(_)) => {
                            self.inner.remove_pending(&id).await;
                            Err(anyhow!("event stream closed before delivering response"))
                        }
                        Err(_) => {
                            self.inner.remove_pending(&id).await;
                            Err(anyhow!(
                                "timed out awaiting streamable HTTP response over event stream"
                            ))
                        }
                    }
                }
            }
        } else {
            if use_background_stream {
                self.inner.remove_pending(&id).await;
            }
            Err(anyhow!(
                "unsupported response content type `{content_type}` from streamable HTTP server"
            ))
        }
    }

    async fn parse_sse_response<R: DeserializeOwned + Send + 'static>(
        &self,
        response: reqwest::Response,
        expected_id: &str,
        timeout: Duration,
    ) -> Result<Option<RpcResponse<R>>> {
        let mut stream = SseStream::from_byte_stream(response.bytes_stream());
        let mut notifications = Vec::new();

        let fut = async {
            while let Some(event) = stream.next().await {
                let event = event.context("failed to parse SSE event")?;
                if let Some(event_type) = event.event.as_deref()
                    && event_type != "message"
                {
                    debug!(event = event_type, "ignoring non-message SSE event");
                    continue;
                }

                let data = event
                    .data
                    .as_deref()
                    .ok_or_else(|| anyhow!("SSE event missing data payload"))?;

                debug!(payload = data, "received streamable HTTP SSE payload");

                let value: Value =
                    serde_json::from_str(data).context("failed to deserialize SSE JSON payload")?;

                if let Some(error) = value.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    let code = error
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or_default();
                    return Err(anyhow!("MCP error {code}: {message}"));
                }

                if let Some(id_value) = value.get("id") {
                    let matches = match id_value {
                        Value::String(s) => s == expected_id,
                        Value::Number(num) => num.to_string() == expected_id,
                        _ => false,
                    };

                    if matches {
                        let result_value = value
                            .get("result")
                            .cloned()
                            .ok_or_else(|| anyhow!("JSON-RPC response missing `result` field"))?;
                        let typed: R = serde_json::from_value(result_value)
                            .context("failed to deserialize JSON-RPC result payload")?;
                        return Ok(Some(RpcResponse {
                            result: typed,
                            notifications,
                        }));
                    }
                }

                notifications.push(value);
            }

            Ok(None)
        };

        time::timeout(timeout, fut)
            .await
            .context("timed out awaiting streamable HTTP SSE response")?
    }

    fn parse_json_response<R: DeserializeOwned + Send + 'static>(
        &self,
        value: Value,
        expected_id: &str,
    ) -> Result<RpcResponse<R>> {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            return Err(anyhow!("MCP error {code}: {message}"));
        }

        let id_matches = value
            .get("id")
            .map(|id| match id {
                Value::String(s) => s == expected_id,
                Value::Number(num) => num.to_string() == expected_id,
                _ => false,
            })
            .unwrap_or(false);

        if !id_matches {
            return Err(anyhow!(
                "JSON response did not contain the expected result for id {expected_id}"
            ));
        }

        let result_value = value
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("JSON-RPC response missing `result` field"))?;

        let typed: R = serde_json::from_value(result_value)
            .context("failed to deserialize JSON-RPC result payload")?;

        Ok(RpcResponse {
            result: typed,
            notifications: Vec::new(),
        })
    }

    async fn current_session_id(&self) -> Result<Option<String>> {
        let guard = self.inner.session_id.lock().await;
        Ok(guard.clone())
    }

    async fn maybe_update_session(&self, response: &reqwest::Response) -> Result<()> {
        if let Some(header_value) = response.headers().get(SESSION_HEADER) {
            let value = header_value
                .to_str()
                .context("invalid session header value")?
                .to_owned();
            let mut guard = self.inner.session_id.lock().await;
            if guard.as_ref() != Some(&value) {
                info!(session = %value, "updated MCP streamable HTTP session id");
                *guard = Some(value);
            }
        }
        Ok(())
    }

    async fn ensure_stream_listener(&self) -> Result<()> {
        self.inner.ensure_stream_listener().await
    }
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: &'a str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

impl Inner {
    async fn register_pending(&self, id: String, sender: oneshot::Sender<Value>) {
        let mut pending = self.pending.lock().await;
        if pending.insert(id.clone(), sender).is_some() {
            warn!(id = %id, "replacing existing pending response sender");
        }
    }

    async fn remove_pending(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }

    async fn ensure_stream_listener(self: &Arc<Self>) -> Result<()> {
        let mut guard = self.stream_task.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        let session_id = self
            .session_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("missing MCP session id for streamable HTTP listener"))?;

        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            this.run_stream_listener(session_id).await;
        });
        *guard = Some(handle);
        Ok(())
    }

    async fn run_stream_listener(self: Arc<Self>, session_id: String) {
        loop {
            let mut request = self
                .client
                .get(self.url.clone())
                .header(ACCEPT, SSE_ACCEPT_HEADER_VALUE)
                .header(SESSION_HEADER, &session_id);

            if let Some(token) = &self.auth_token {
                request = request.bearer_auth(token);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    warn!("failed to fetch streamable HTTP event stream: {error}");
                    time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            if !response.status().is_success() {
                warn!(
                    status = %response.status(),
                    "streamable HTTP event stream returned non-success status"
                );
                time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            let mut stream = SseStream::from_byte_stream(response.bytes_stream());
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        if let Some(event_type) = event.event.as_deref()
                            && event_type != "message"
                        {
                            debug!(event = event_type, "ignoring non-message SSE event");
                            continue;
                        }

                        let Some(data) = event.data.as_deref() else {
                            debug!("ignoring SSE event without data payload");
                            continue;
                        };

                        match serde_json::from_str::<Value>(data) {
                            Ok(value) => self.handle_stream_value(value).await,
                            Err(error) => {
                                warn!("failed to deserialize streamable HTTP SSE payload: {error}")
                            }
                        }
                    }
                    Err(error) => {
                        warn!("error reading streamable HTTP SSE stream: {error}");
                        break;
                    }
                }
            }

            time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn handle_stream_value(&self, value: Value) {
        if let Some(id_value) = value.get("id")
            && let Some(id) = Self::extract_id(id_value)
        {
            let sender = self.pending.lock().await.remove(&id);
            if let Some(sender) = sender {
                let _ = sender.send(value);
            } else {
                debug!(id = %id, "received response for unknown request id");
            }
            return;
        }

        debug!(payload = %value, "received streamable HTTP notification without id");
    }

    fn extract_id(id_value: &Value) -> Option<String> {
        match id_value {
            Value::String(id) => Some(id.clone()),
            Value::Number(num) => Some(num.to_string()),
            _ => None,
        }
    }
}
