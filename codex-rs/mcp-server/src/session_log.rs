// Helper macros for session-scoped logging with structured `session_id` fields.
// These macros ensure the `session_id` is always present even when called
// outside of an active tracing span.

#[macro_export]
macro_rules! mcp_session_trace {
    ($session_id:expr, $($arg:tt)*) => {{
        ::tracing::trace!(session_id = %$session_id, $($arg)*);
    }};
}

#[macro_export]
macro_rules! mcp_session_debug {
    ($session_id:expr, $($arg:tt)*) => {{
        ::tracing::debug!(session_id = %$session_id, $($arg)*);
    }};
}

#[macro_export]
macro_rules! mcp_session_info {
    ($session_id:expr, $($arg:tt)*) => {{
        ::tracing::info!(session_id = %$session_id, $($arg)*);
    }};
}

#[macro_export]
macro_rules! mcp_session_warn {
    ($session_id:expr, $($arg:tt)*) => {{
        ::tracing::warn!(session_id = %$session_id, $($arg)*);
    }};
}

#[macro_export]
macro_rules! mcp_session_error {
    ($session_id:expr, $($arg:tt)*) => {{
        ::tracing::error!(session_id = %$session_id, $($arg)*);
    }};
}
