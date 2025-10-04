Codex MCP Server — Multi‑Session Design and Usage

This document describes the design, internals, and operational guidance for the Codex MCP server (codex-mcp-server), with emphasis on the multi‑session HTTP transport and streaming behavior.

Scope

- Binary: `codex mcp-server` (also built as `codex-mcp-server`)
- Transport modes:
  - Stdio JSON‑RPC: reads/writes newline‑delimited JSON to stdin/stdout
  - HTTP: JSON‑RPC over HTTP with optional Server‑Sent Events (SSE) streaming
- Multi‑session: concurrently hosts independent client sessions on a single server process

High‑Level Overview

- The server implements the MCP (Model Context Protocol) request/response surface on top of Codex core.
- In HTTP mode, it supports multiple concurrent sessions identified by an `mcp-session-id` header. Each session maintains its own message pump and SSE stream.
- Messages are bridged between HTTP handlers and the Codex `MessageProcessor`. Streaming notifications and responses are fan‑out to any SSE subscribers for that session.

Key Features

- Multi‑session HTTP hosting
- Low‑latency streaming via SSE (GET and optional POST)
- Stdio JSON‑RPC mode for tool inspector / piping
- File logging to `$CODEX_HOME/log/codex-rmcp.log` with bearer‑token redaction
- Compatible with `mcp-types` Initialize, Ping, ListTools, CallTool, and notifications

Architecture

- Entry points
  - `codex-rs/mcp-server/src/main.rs`: CLI, selects stdio vs HTTP by flags
  - `codex-rs/mcp-server/src/lib.rs`: server implementation (stdio and HTTP)
  - `codex-rs/mcp-server/src/message_processor.rs`: MCP request/notification handlers and Codex integration
  - `codex-rs/mcp-server/src/outgoing_message.rs`: outgoing JSON‑RPC builder + sender

- Modes
  - Stdio mode (default): `run_main(...)`
    - Reads newline‑delimited `JSONRPCMessage` from stdin into an `mpsc::Sender`
    - `MessageProcessor` handles messages and emits `OutgoingMessage` to an `mpsc::UnboundedSender`
    - A writer task serializes messages back to stdout as one JSON per line
  - HTTP mode: `run_http_server(host, port, ...)`
    - Axum router exposes `GET /` and `GET /mcp` for SSE, `POST /` and `POST /mcp` for JSON‑RPC
    - Multi‑session routing keyed by `mcp-session-id` header (see below)

- Session management (HTTP)
  - Shared state: `HttpMultiState` maintains a `Mutex<HashMap<String, Arc<SessionState>>>`
  - When a request arrives, `ensure_session(...)` finds or creates a session:
    - A new `SessionState` has:
      - `incoming_tx: mpsc::Sender<JSONRPCMessage>` — bounded channel (capacity 128)
      - `sse_tx: broadcast::Sender<String>` — SSE fan‑out (capacity 128)
      - `pending: Mutex<HashMap<RequestId, oneshot::Sender<String>>>` — responders awaiting a specific JSON‑RPC id
    - Two tasks are spawned per session:
      - Processor task: drains `incoming_rx`, invokes `MessageProcessor`, which calls into Codex core
      - Bridge task: drains `outgoing_rx`, serializes each `OutgoingMessage` to JSON, then:
        - Fulfills a pending oneshot by `id` for Responses/Errors
        - Broadcasts every message JSON to the session’s `sse_tx`

- Message flow (HTTP)
  - `POST` (JSON default):
    1) Parse `JSONRPCMessage`
    2) Resolve session (adds `mcp-session-id` header on response)
    3) For Request: register an oneshot responder in `pending` keyed by `id` and forward to `incoming_tx`
    4) Wait for the bridge to send the matching response JSON; return 200 JSON
    5) For Notification/Response/Error: forward and return 204
  - `POST` (SSE option):
    - If `Accept: text/event-stream` is present, subscribe to `sse_tx`, forward Request, and return an SSE stream rather than a JSON body
    - Note: clients that also hold a background `GET` SSE stream will see duplicate events; see guidance below
  - `GET` SSE: Attach to per‑session broadcast channel; each event is a JSON string with a serialized `JSONRPCMessage`

Concurrency and Backpressure

- `CHANNEL_CAPACITY` is 128 for bounded incoming and broadcast channels; outgoing is unbounded by design to avoid stalling processors, with the network consumer as the backpressure edge.
- SSE uses a `broadcast::Sender`. Slow subscribers may lag and drop; the server converts broadcast errors to a best‑effort stream (SSE clients are expected to reconnect).
- Pending responders (`oneshot::Sender`) avoid per‑request task creation beyond a single waiter.

Streaming semantics

- Every `OutgoingMessage` is broadcast to the session SSE stream as its JSON serialization:
  - Notifications (e.g., `codex/event`) and tool‑call lifecycles
  - JSON‑RPC Responses and Errors
- For POST Requests in JSON mode, clients get a single JSON body for that id while still receiving duplicates via SSE if they subscribe — clients typically either:
  - Use GET SSE for events and POST for request/response correlation; or
  - Use SSE‑on‑POST exclusively (but avoid keeping a background GET SSE to prevent duplication)

Session identity and lifecycle

- Clients may supply `mcp-session-id` on any HTTP call; if absent, the server generates a UUID and echoes it back in the response headers.
- Sessions live as long as there is activity; the current implementation has no TTL eviction. Future work: idle eviction and a `shutdown` call.

Logging and observability

- File log: `$CODEX_HOME/log/codex-rmcp.log`. Default `CODEX_HOME` is `~/.codex`; set `CODEX_HOME` to override (e.g., `~/.codex_dev_home`).
- Log level via `RUST_LOG` (e.g., `info`, `debug`, `trace`, or targeted `codex_mcp_server=debug,axum=info,hyper=warn`).
- Bearer redaction: all file‑log writes mask tokens following `"Bearer "` (case‑insensitive) to `****`. The stderr console stream remains unredacted by design for local debugging.

Running the server

- Stdio JSON‑RPC (default):
  - `codex mcp-server`
  - With a tool inspector: `npx @modelcontextprotocol/inspector codex mcp-server`

- HTTP (multi‑session):
  - `codex mcp-server --host 127.0.0.1 --port 3333`
  - Open a session stream:
    - `curl -N -H 'Accept: text/event-stream' http://127.0.0.1:3333/mcp`
      - The server responds with `mcp-session-id` header; reuse it below
  - Send a Request (JSON response inline):
    - `curl -X POST -H 'Content-Type: application/json' -H 'mcp-session-id: <id>' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}' http://127.0.0.1:3333/mcp`
  - Stream via POST (advanced):
    - `curl -N -X POST -H 'Content-Type: application/json' -H 'Accept: text/event-stream' -H 'mcp-session-id: <id>' -d '{...}' http://127.0.0.1:3333/mcp`
    - Prefer not to hold a background GET SSE simultaneously to avoid duplicate events

Security considerations

- The server binds to the specified host (default `127.0.0.1`). Use loopback in development to avoid exposing the endpoint.
- File logging masks bearer tokens; sensitive data passed in payloads is not otherwise sanitized. Avoid logging secrets at application level.

Compatibility and protocol

- Supported MCP surface:
  - `initialize`, `ping`, `tools/list`, `tools/call`
  - Notifications: cancellations, progress, and Codex event bridging as `codex/event` notifications with `_meta.requestId` for correlation
- See code for exact types via `mcp-types` and `codex_core::protocol`.

Performance notes

- The per‑session processor stays hot, avoiding repeated configuration parsing.
- Channel sizes (128) were chosen as a throughput vs. memory compromise for interactive workloads; revisit for heavy multi‑client deployments.
- JSON serialization is on the hot path for the HTTP bridge; consider switching to a zero‑copy writer or structured binary framing if future transports (e.g., WebSocket) are introduced.

Known issues and guidance

- SSE on POST vs GET duplication:
  - Current behavior enables SSE on POST when `Accept: text/event-stream` is present, while also broadcasting to the per‑session GET SSE. Clients that use both will see duplicates.
  - Guidance: either (a) use GET SSE + JSON POST, or (b) rely solely on SSE‑on‑POST and avoid maintaining a background GET SSE stream.
  - Future improvement: Gate the POST streaming behind an explicit header and default POST to JSON.

Feature Gate: POST Streaming (Proposed)

- Problem: Today, POST returns SSE when the client sends `Accept: text/event-stream` (and we also broadcast to the GET SSE stream). Clients that open both streams see duplicates.
- Goal: Make JSON the default for POST and require an explicit, opt‑in gate to stream on POST. This de‑duplicates events for clients that already rely on GET SSE.

- Proposed API contract
  - Header: `X-MCP-POST-STREAM: 1`
  - Behavior:
    - If header present on POST Requests: return `200` with `text/event-stream` and stream all messages (notifications + the final response) for that session.
    - If header absent: ignore `Accept: text/event-stream` and return a single JSON body for the id (legacy behavior).
    - GET SSE remains unchanged and continues to subscribe to the session broadcast.
  - Backward compatibility: Existing clients that only use GET SSE + JSON POST are unaffected. Clients that previously relied on `Accept` alone must set the new header to re‑enable SSE on POST.

- Client recipes
  - GET SSE + JSON POST (recommended default):
    - `GET` SSE (no header changes)
    - `POST` with `Content-Type: application/json` (no `X-MCP-POST-STREAM` header)
  - POST‑only streaming (advanced):
    - `POST` with `X-MCP-POST-STREAM: 1` and optionally `Accept: text/event-stream`
    - Do not hold a background GET SSE for the same session unless the client de‑duplicates by id

- Implementation notes
  - The HTTP POST handler will check `X-MCP-POST-STREAM` first; only when present will it attach a `BroadcastStream` receiver and stream events as SSE.
  - The Accept header will no longer trigger POST SSE by itself.
  - We will keep GET SSE broadcast semantics intact for simplicity and compatibility.


Future work

- Session lifecycle: idle TTL, explicit close, and metrics per session
- Optional WebSocket transport with subprotocol; unified stream without duplication
- Backpressure and drop‑policy tunables for the broadcast channel
- Richer auth and CORS guardrails for browser‑based clients
- Structured metrics and tracing spans per request id

Code map

- CLI and entry:
  - `codex-rs/mcp-server/src/main.rs`
  - `codex-rs/mcp-server/src/lib.rs`
- HTTP internals:
  - Router + handlers: `codex-rs/mcp-server/src/lib.rs`
  - Session state and bridge: `codex-rs/mcp-server/src/lib.rs`
- MCP processing:
  - `codex-rs/mcp-server/src/message_processor.rs`
  - `codex-rs/mcp-server/src/outgoing_message.rs`
- Logging + redaction:
  - `codex-rs/mcp-server/src/lib.rs`
  - `codex-rs/mcp-server/src/redact.rs`

Extended Session Logging Coverage

The server tags logs from per‑session processor and HTTP bridge tasks with `session_id` via tracing spans and helper macros (`mcp_session_*`). For even broader coverage:

- codex_tool_runner (approvals + event loop)
  - We can thread `session_id` into `run_codex_tool_session` and `run_codex_tool_session_reply` and use the `mcp_session_*` macros for:
    - Approval elicitations (exec/patch): creation, acceptance/denial, and error paths
    - Event loop transitions: `TaskComplete`, `Error`, and unusual `SessionConfigured` cases
  - Implementation outline:
    - Add a `session_id: String` argument to the functions in `codex_tool_runner.rs`, passed from the per‑session `MessageProcessor`
    - Replace direct `tracing::*` calls with the `mcp_session_*` macros where `session_id` is available
    - Keep existing structured fields (`call_id`, `conversation_id`, etc.) for queryable context
  - Outcome: approval flows and event emissions are session‑tagged, simplifying correlation across many concurrent sessions.

Session State, Context, and Isolation

This section details how per‑session state is created, how context is tracked across turns, and what constitutes an isolation boundary between sessions.

Per‑Session Components

- Message pump
  - Each session created by `ensure_session(...)` owns:
    - `incoming_tx`/`incoming_rx` (bounded): RPC ingress for that session only
    - `outgoing_rx` (unbounded): server→client messages that will be bridged
    - `sse_tx` (broadcast): fan‑out for all serialized `JSONRPCMessage` for this session
    - `pending` (oneshot map by RequestId): POST responders waiting for a matching id
  - A dedicated `MessageProcessor` instance is constructed per session; it is not shared across sessions.

- Codex core integration
  - Each `MessageProcessor` constructs its own `ConversationManager` (via `ConversationManager::new(...)`). This manager is the authoritative registry for conversations created by that session.
  - Conversations are created by `ConversationManager::new_conversation(config)` inside `run_codex_tool_session(...)` and retrieved via `get_conversation(...)` for follow‑ups.
  - A per‑processor in‑flight map `running_requests_id_to_codex_uuid: HashMap<RequestId, ConversationId>` binds an MCP `tools/call` request id to the Codex `ConversationId` while the call is active. This powers cancellation and reply routing.

Context Propagation and Correlation

- Sub‑id scheme
  - For an MCP `tools/call`, the server uses the incoming JSON‑RPC `id` as the Codex submission id (`sub_id`). This ensures every Codex event emitted during that call carries a stable, caller‑controlled correlation id.
  - All Codex events are sent to the client as `codex/event` notifications with `_meta.requestId` set to the original JSON‑RPC id, so clients can multiplex many tool calls over one session.

- Conversation lifecycle
  - First `codex` tool call creates a new Codex conversation (model, cwd, sandbox policy, user instructions, etc. derived from `CodexToolCallParam`).
  - Subsequent `codex-reply` calls address an existing conversation by `conversation_id`, allowing multi‑turn flows within the same MCP session.
  - On `TaskComplete`, the server sends a final `CallToolResult` and removes the `id → conversation_id` entry from the in‑flight map. The conversation itself remains available for future `codex-reply` calls until the process exits or a future explicit close is implemented.

- Cancellation
  - MCP `notifications/cancelled` includes a `request_id`. The server looks up `request_id → conversation_id` in the session’s map and submits `Op::Interrupt` to that Codex conversation using the same `sub_id`.
  - If there is no mapping (e.g., already completed), the notification is ignored.

- Approval flows (exec and patch)
  - Exec: upon `ExecApprovalRequestEvent`, the server elicits a client response via `ElicitRequest` over MCP (sent through `OutgoingMessageSender::send_request`). On approval, execution proceeds; on denial, the tool call concludes with an error result.
  - Patch: upon `ApplyPatchApprovalRequestEvent`, the server elicits approval similarly; if approved, the patch is applied in‑process (no external binary spawns) and the tool call returns the apply‑patch summary output.
  - All elicitations and approvals are correlated by the originating `JSONRPC id` (the sub‑id) and the server’s per‑session `running_requests_id_to_codex_uuid` mapping.

Isolation Guarantees

- Data isolation
  - `MessageProcessor`, `ConversationManager`, `pending` responders, and `sse_tx` are all per‑session.
  - Events and responses for a session are only published to that session’s `sse_tx`; other sessions cannot observe them.
  - POST responders are stored in the session’s `pending` map; a response `id` cannot accidentally resolve across sessions.

- Execution isolation
  - Codex conversations are owned by the session’s `ConversationManager` and are not visible to other sessions.
  - Execution (shell/patch) uses per‑conversation context (cwd, sandbox policy, approval policy) derived from that session’s `config`.

- Shared process state (non‑isolation)
  - `AuthManager::shared(codex_home, ...)` is a process‑wide cache keyed by `CODEX_HOME`. If two sessions intentionally point at the same `CODEX_HOME`, they share credentials on disk. To isolate credentials across sessions, run separate server processes with distinct `CODEX_HOME` values.
  - File logging is shared (single process log); individual lines carry sufficient context (timestamps, module names) but not session ids at present. Adding `session_id` to log context is a reasonable future improvement.

Memory and Lifetime Management

- Each active session holds a bounded inbox, an unbounded outbox bridge, a broadcast channel, and a processor. These are dropped when the last reference is dropped (i.e., when the session is evicted or the server shuts down).
- The current implementation does not evict idle sessions. For long‑running multi‑tenant deployments, add an idle TTL and explicit close APIs.

Client Guidance for Multi‑Session

- Use `mcp-session-id` to scope all HTTP requests and SSE subscriptions. Treat the value as an opaque, server‑generated key.
- Do not mix `GET` SSE with SSE‑on‑`POST` for the same session unless the client de‑duplicates by `JSONRPC id`. Prefer one streaming path:
  - Simpler: `GET` SSE + JSON `POST`
  - Alternative: SSE‑on‑`POST` only (no background `GET` SSE)
- For multi‑turn conversations, use `codex-reply` and persist the `conversation_id` returned by the initial `codex` tool call.
