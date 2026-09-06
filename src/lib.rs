//! Shared machinery for the omni-mcp family of standalone MCP tool servers.
//!
//! Each tool server (hex-mcp, ssh-mcp, eval-mcp, …) is a thin crate: it
//! implements [`ToolGroup`] and calls [`run`]. Everything else — the command
//! line, transports, `--single-tool` squashing, timeouts, authentication and
//! the MCP protocol itself — lives here, so a fix or a new capability lands in
//! every server at once.
//!
//! The protocol comes from [`rmcp`], the official Rust MCP SDK. Transport-level
//! spec compliance (session handling, required headers, SSE framing) is fiddly
//! and gets revised; it is not worth reimplementing per server.
//!
//! ```no_run
//! use mcp_toolkit::{ServerOptions, ToolDef, ToolGroup, ToolOutput, ToolResult};
//! use async_trait::async_trait;
//! use serde_json::{Value, json};
//!
//! struct Greeter;
//!
//! #[async_trait]
//! impl ToolGroup for Greeter {
//!     fn tools(&self) -> Vec<ToolDef> {
//!         vec![ToolDef::new("greet", "Say hello.", json!({ "type": "object" }))]
//!     }
//!     async fn call(&self, _name: &str, _args: Value) -> ToolResult<ToolOutput> {
//!         Ok(ToolOutput::text("hello"))
//!     }
//! }
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! mcp_toolkit::run("greeter", "0.1.0", std::sync::Arc::new(Greeter), ServerOptions::default())
//!     .await?;
//! # Ok(())
//! # }
//! ```

pub mod cli;
pub mod composite;
pub mod server;
pub mod single;
pub mod tool;

use std::sync::Arc;

pub use cli::{ServerOptions, Transport};
pub use composite::{Composite, Member};
pub use server::ToolServer;
pub use tool::{
    ToolDef, ToolFailure, ToolGroup, ToolOutput, ToolResult, ensure_timeout_property,
    requested_timeout, strip_timeout,
};

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("could not bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "the HTTP transport exposes every tool to any local process. Pass --auth-token \
         (or set MCP_AUTH_TOKEN), or --allow-unauthenticated to acknowledge the risk."
    )]
    UnauthenticatedHttp,

    #[error("transport error: {0}")]
    Transport(String),
}

/// Initialises logging on stderr.
///
/// Never stdout: that carries the JSON-RPC stream, and anything else written
/// there corrupts the frame sequence.
pub fn init_logging(level: Option<&str>) {
    use tracing_subscriber::EnvFilter;

    let filter = level.map_or_else(
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        EnvFilter::new,
    );

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Builds the server and runs it under the selected transport.
///
/// Handles `--list-tools` by printing and returning.
pub async fn run(
    name: impl Into<String>,
    version: impl Into<String>,
    group: Arc<dyn ToolGroup>,
    options: ServerOptions,
) -> Result<(), StartupError> {
    init_logging(options.log.as_deref());

    let server = ToolServer::new(name, version, group, options);

    if server.options().list_tools {
        print_tools(&server);
        return Ok(());
    }

    match server.options().transport {
        Transport::Stdio => serve_stdio(server).await,
        Transport::Http | Transport::Sse => serve_http(server).await,
    }
}

fn print_tools(server: &ToolServer) {
    let tools = server.advertised();
    if server.options().json {
        let rendered: Vec<_> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.schema,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rendered).unwrap_or_else(|_| "[]".to_string())
        );
        return;
    }

    println!("{} tools:\n", tools.len());
    for tool in tools {
        let summary = tool.description.lines().next().unwrap_or_default();
        println!("  {:<24} {summary}", tool.name);
    }
}

async fn serve_stdio(server: ToolServer) -> Result<(), StartupError> {
    use rmcp::ServiceExt;
    use rmcp::transport::stdio;

    tracing::info!("serving MCP over stdio");
    let running =
        server.serve(stdio()).await.map_err(|e| StartupError::Transport(e.to_string()))?;
    running.waiting().await.map_err(|e| StartupError::Transport(e.to_string()))?;
    Ok(())
}

async fn serve_http(server: ToolServer) -> Result<(), StartupError> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };

    let options = server.options().clone();
    if !options.http_start_permitted() {
        return Err(StartupError::UnauthenticatedHttp);
    }

    let token = options.token().map(str::to_string);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default(),
    );

    let app: axum::Router = axum::Router::new().nest_service("/mcp", service);
    let app = match token {
        Some(expected) => app.layer(axum::middleware::from_fn(move |req, next| {
            let expected = expected.clone();
            async move { auth::guard(&expected, req, next).await }
        })),
        None => app,
    };

    let listener = tokio::net::TcpListener::bind(&options.bind)
        .await
        .map_err(|source| StartupError::Bind { addr: options.bind.clone(), source })?;

    tracing::info!(address = %options.bind, authenticated = options.token().is_some(), "serving MCP over HTTP");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(StartupError::Io)
}

mod auth {
    use axum::extract::Request;
    use axum::http::{StatusCode, header};
    use axum::middleware::Next;
    use axum::response::Response;

    /// Rejects requests without a matching bearer token.
    pub async fn guard(
        expected: &str,
        request: Request,
        next: Next,
    ) -> Result<Response, StatusCode> {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .trim();

        if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return Ok(next.run(request).await);
        }
        tracing::warn!("rejected an HTTP request with a missing or incorrect bearer token");
        Err(StatusCode::UNAUTHORIZED)
    }

    /// Compares without leaking the position of the first difference.
    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    #[cfg(test)]
    mod tests {
        use super::constant_time_eq;

        #[test]
        fn comparison_is_still_correct() {
            assert!(constant_time_eq(b"abc", b"abc"));
            assert!(!constant_time_eq(b"abc", b"abd"));
            assert!(!constant_time_eq(b"abc", b"ab"));
            assert!(constant_time_eq(b"", b""));
        }
    }
}
