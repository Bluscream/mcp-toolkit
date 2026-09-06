//! The command line every server built on this kit shares.

use clap::{Parser, ValueEnum};

/// Standard options for an MCP tool server.
///
/// Clippy's `struct_excessive_bools` is allowed deliberately: these are
/// independent command-line switches, and folding them into enums would make
/// the call sites harder to read, not easier.
#[allow(clippy::struct_excessive_bools)]
///
/// A tool crate embeds this with `#[command(flatten)]` and adds its own
/// options, so every server in the family accepts the same core flags.
#[derive(Debug, Clone, Parser)]
pub struct ServerOptions {
    /// How to speak MCP.
    #[arg(long, value_enum, default_value_t = Transport::Stdio, env = "MCP_TRANSPORT")]
    pub transport: Transport,

    /// Address for the HTTP transports.
    #[arg(long, default_value = "127.0.0.1:8080", env = "MCP_BIND")]
    pub bind: String,

    /// Collapse every tool into one dispatching tool, for clients that limit
    /// how many tools they accept.
    #[arg(long, env = "MCP_SINGLE_TOOL")]
    pub single_tool: bool,

    /// Default per-call timeout in seconds.
    #[arg(long, default_value_t = 30, env = "MCP_TIMEOUT")]
    pub timeout: u64,

    /// Upper bound a caller's own `timeout` argument is clamped to.
    #[arg(long, default_value_t = 300, env = "MCP_MAX_TIMEOUT")]
    pub max_timeout: u64,

    /// Bearer token required by the HTTP transports. Without one, HTTP mode
    /// refuses to start unless --allow-unauthenticated is given.
    #[arg(long, env = "MCP_AUTH_TOKEN")]
    pub auth_token: Option<String>,

    /// Serve HTTP without authentication. Every tool becomes reachable by any
    /// local process.
    #[arg(long)]
    pub allow_unauthenticated: bool,

    /// Log verbosity; overrides `RUST_LOG`.
    #[arg(long, env = "MCP_LOG")]
    pub log: Option<String>,

    /// Print the tools this server would expose, then exit.
    #[arg(long)]
    pub list_tools: bool,

    /// With --list-tools, emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            transport: Transport::Stdio,
            bind: "127.0.0.1:8080".into(),
            single_tool: false,
            timeout: 30,
            max_timeout: 300,
            auth_token: None,
            allow_unauthenticated: false,
            log: None,
            list_tools: false,
            json: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout. What an IDE launches.
    Stdio,
    /// MCP Streamable HTTP.
    Http,
    /// Accepted as an alias for `http`.
    ///
    /// The standalone SSE transport was deprecated in the MCP specification in
    /// favour of Streamable HTTP, which uses SSE for its streaming responses.
    /// Kept so existing `--transport sse` invocations keep working.
    Sse,
}

impl Transport {
    /// Whether this transport listens on a socket.
    pub fn is_http(self) -> bool {
        matches!(self, Self::Http | Self::Sse)
    }
}

impl ServerOptions {
    /// The effective bearer token, treating blank as absent.
    pub fn token(&self) -> Option<&str> {
        self.auth_token.as_deref().map(str::trim).filter(|t| !t.is_empty())
    }

    /// Whether the HTTP transport may start given the auth settings.
    pub fn http_start_permitted(&self) -> bool {
        self.token().is_some() || self.allow_unauthenticated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[derive(Parser, Debug)]
    struct Harness {
        #[command(flatten)]
        options: ServerOptions,
    }

    fn parse(args: &[&str]) -> ServerOptions {
        let mut full = vec!["server"];
        full.extend_from_slice(args);
        Harness::parse_from(full).options
    }

    #[test]
    fn the_definition_is_internally_consistent() {
        Harness::command().debug_assert();
    }

    #[test]
    fn stdio_is_the_default_transport() {
        let options = parse(&[]);
        assert_eq!(options.transport, Transport::Stdio);
        assert!(!options.transport.is_http());
        assert!(!options.single_tool);
        assert_eq!(options.timeout, 30);
    }

    #[test]
    fn sse_is_accepted_as_an_alias_for_http() {
        // The standalone SSE transport is deprecated; existing invocations
        // must keep working rather than erroring on an unknown value.
        assert!(parse(&["--transport", "sse"]).transport.is_http());
        assert!(parse(&["--transport", "http"]).transport.is_http());
    }

    #[test]
    fn every_core_flag_parses() {
        let options = parse(&[
            "--transport",
            "http",
            "--bind",
            "0.0.0.0:9000",
            "--single-tool",
            "--timeout",
            "5",
            "--max-timeout",
            "60",
            "--auth-token",
            "secret",
        ]);
        assert_eq!(options.bind, "0.0.0.0:9000");
        assert!(options.single_tool);
        assert_eq!(options.timeout, 5);
        assert_eq!(options.max_timeout, 60);
        assert_eq!(options.token(), Some("secret"));
    }

    #[test]
    fn a_blank_token_counts_as_absent() {
        assert_eq!(parse(&["--auth-token", "   "]).token(), None);
        assert_eq!(parse(&["--auth-token", " real "]).token(), Some("real"));
    }

    #[test]
    fn http_refuses_to_start_unauthenticated_unless_acknowledged() {
        assert!(!parse(&["--transport", "http"]).http_start_permitted());
        assert!(parse(&["--transport", "http", "--auth-token", "t"]).http_start_permitted());
        assert!(parse(&["--transport", "http", "--allow-unauthenticated"]).http_start_permitted());
    }

    #[test]
    fn list_tools_is_available_for_inspection() {
        let options = parse(&["--list-tools", "--json"]);
        assert!(options.list_tools);
        assert!(options.json);
    }
}
