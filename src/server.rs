//! Wires a [`ToolGroup`] onto rmcp's transports.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::cli::ServerOptions;
use crate::single;
use crate::tool::{self as toolmod, ToolDef, ToolFailure, ToolGroup, ToolOutput};

/// A tool server: a group of tools plus the options that shape how they are
/// advertised and bounded.
#[derive(Clone)]
pub struct ToolServer {
    name: String,
    version: String,
    group: Arc<dyn ToolGroup>,
    options: Arc<ServerOptions>,
    /// What `tools/list` advertises: either the real tools, or the single
    /// squashed one, with timeouts injected.
    advertised: Arc<Vec<ToolDef>>,
    /// Advertised names whose schema we augmented with `timeout`.
    augmented: Arc<Vec<String>>,
}

impl ToolServer {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        group: Arc<dyn ToolGroup>,
        options: ServerOptions,
    ) -> Self {
        let name = name.into();
        let real = group.tools();

        let mut advertised =
            if options.single_tool { vec![single::squash(&name, &real)] } else { real };

        let mut augmented = Vec::new();
        for tool in &mut advertised {
            if toolmod::ensure_timeout_property(&mut tool.schema, options.max_timeout) {
                augmented.push(tool.name.clone());
            }
        }

        Self {
            name,
            version: version.into(),
            group,
            options: Arc::new(options),
            advertised: Arc::new(advertised),
            augmented: Arc::new(augmented),
        }
    }

    pub fn advertised(&self) -> &[ToolDef] {
        &self.advertised
    }

    pub fn options(&self) -> &ServerOptions {
        &self.options
    }

    /// Runs one advertised tool: unwraps single-tool dispatch, applies the
    /// deadline, and forwards to the group.
    pub async fn invoke(&self, name: &str, args: Value) -> Result<ToolOutput, ToolFailure> {
        let seconds =
            toolmod::requested_timeout(&args, self.options.timeout, self.options.max_timeout);

        let (target, args) = if self.options.single_tool {
            let (target, rest) = single::dispatch(&args)?;
            (target, rest)
        } else {
            (name.to_string(), args)
        };

        let args = if self.augmented.iter().any(|n| n == name) {
            toolmod::strip_timeout(args)
        } else {
            args
        };

        // Guard against dispatching to something this server does not serve,
        // which in single-tool mode would otherwise be caller-controlled.
        if !self.group.tools().iter().any(|t| t.name == target) {
            return Err(ToolFailure::NotFound(target));
        }

        let call = self.group.call(&target, args);
        match tokio::time::timeout(Duration::from_secs(seconds), call).await {
            Ok(result) => result,
            Err(_) => Err(ToolFailure::Timeout { tool: target, seconds }),
        }
    }
}

fn to_rmcp_tool(def: &ToolDef) -> Tool {
    let schema = def.schema.as_object().cloned().unwrap_or_default();
    Tool::new(Cow::Owned(def.name.clone()), Cow::Owned(def.description.clone()), Arc::new(schema))
}

fn to_rmcp_result(output: ToolOutput) -> CallToolResult {
    let content = vec![ContentBlock::text(output.text)];
    let mut result = if output.is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    if let Some(structured) = output.structured {
        result.structured_content = Some(structured);
    }
    result
}

impl ServerHandler for ToolServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(self.name.clone(), self.version.clone()))
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.advertised.iter().map(to_rmcp_tool).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = params.arguments.map_or_else(|| json!({}), Value::Object);
        debug!(tool = %params.name, "call");

        match self.invoke(&params.name, args).await {
            Ok(output) => Ok(to_rmcp_result(output).into()),
            // A tool that ran and failed is a result the model should read.
            Err(failure) if failure.is_tool_level() => {
                warn!(tool = %params.name, %failure, "tool failed");
                Ok(to_rmcp_result(ToolOutput::error(failure.to_string())).into())
            }
            // The caller got the request wrong: that is a protocol error.
            Err(failure) => Err(ErrorData::invalid_params(failure.to_string(), None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Fake;

    #[async_trait]
    impl ToolGroup for Fake {
        fn tools(&self) -> Vec<ToolDef> {
            vec![
                ToolDef::new(
                    "echo",
                    "Echo text.",
                    json!({
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }),
                ),
                ToolDef::new(
                    "slow",
                    "Sleep forever.",
                    json!({ "type": "object", "properties": {} }),
                ),
                ToolDef::new("boom", "Fail.", json!({ "type": "object", "properties": {} })),
            ]
        }

        async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolFailure> {
            match name {
                "echo" => Ok(ToolOutput::text(
                    args.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                )),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(ToolOutput::text("never"))
                }
                "boom" => Err(ToolFailure::Failed("exploded".into())),
                other => Err(ToolFailure::NotFound(other.to_string())),
            }
        }
    }

    fn server(options: ServerOptions) -> ToolServer {
        ToolServer::new("fake", "1.0", Arc::new(Fake), options)
    }

    #[tokio::test]
    async fn normal_mode_advertises_every_tool() {
        let s = server(ServerOptions::default());
        let names: Vec<&str> = s.advertised().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["echo", "slow", "boom"]);
    }

    #[tokio::test]
    async fn single_tool_mode_advertises_exactly_one() {
        let s = server(ServerOptions { single_tool: true, ..Default::default() });
        assert_eq!(s.advertised().len(), 1);
        assert_eq!(s.advertised()[0].name, "fake_tool");
    }

    #[tokio::test]
    async fn a_tool_runs_and_returns_its_output() {
        let s = server(ServerOptions::default());
        let out = s.invoke("echo", json!({ "text": "hi" })).await.unwrap();
        assert_eq!(out.text, "hi");
    }

    #[tokio::test]
    async fn single_tool_mode_dispatches_through_the_tool_argument() {
        let s = server(ServerOptions { single_tool: true, ..Default::default() });
        let out =
            s.invoke("fake_tool", json!({ "tool": "echo", "text": "via single" })).await.unwrap();
        assert_eq!(out.text, "via single");
    }

    #[tokio::test]
    async fn single_tool_mode_rejects_an_unknown_operation() {
        let s = server(ServerOptions { single_tool: true, ..Default::default() });
        let err = s.invoke("fake_tool", json!({ "tool": "nope" })).await.unwrap_err();
        assert!(matches!(err, ToolFailure::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn single_tool_mode_requires_the_tool_argument() {
        let s = server(ServerOptions { single_tool: true, ..Default::default() });
        let err = s.invoke("fake_tool", json!({ "text": "x" })).await.unwrap_err();
        assert!(matches!(err, ToolFailure::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn a_slow_tool_is_bounded_by_the_default_timeout() {
        let s = server(ServerOptions { timeout: 1, ..Default::default() });
        let err = s.invoke("slow", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolFailure::Timeout { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn a_caller_supplied_timeout_is_honoured_and_clamped() {
        let s = server(ServerOptions { timeout: 3600, max_timeout: 1, ..Default::default() });
        // max_timeout clamps the caller's request down to 1s.
        let started = std::time::Instant::now();
        let err = s.invoke("slow", json!({ "timeout": 900 })).await.unwrap_err();
        assert!(matches!(err, ToolFailure::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_injected_timeout_is_not_forwarded_to_the_tool() {
        let s = server(ServerOptions::default());
        // `echo` would echo nothing extra; the point is it must not error on an
        // undeclared property once schemas are validated upstream.
        let out = s.invoke("echo", json!({ "text": "x", "timeout": 5 })).await.unwrap();
        assert_eq!(out.text, "x");
    }

    #[tokio::test]
    async fn a_failing_tool_yields_a_tool_level_failure() {
        let s = server(ServerOptions::default());
        let err = s.invoke("boom", json!({})).await.unwrap_err();
        assert!(err.is_tool_level(), "a failure the model should read, not a protocol error");
    }

    #[test]
    fn every_advertised_tool_gains_a_timeout_property() {
        let s = server(ServerOptions::default());
        for tool in s.advertised() {
            assert_eq!(
                tool.schema["properties"]["timeout"]["type"], "integer",
                "{} lacks timeout",
                tool.name
            );
        }
    }

    #[test]
    fn conversion_to_rmcp_preserves_name_description_and_schema() {
        let def = ToolDef::new("t", "does a thing", json!({ "type": "object", "properties": {} }));
        let tool = to_rmcp_tool(&def);
        assert_eq!(tool.name, "t");
        assert_eq!(tool.description.as_deref(), Some("does a thing"));
        assert_eq!(tool.input_schema.get("type").and_then(Value::as_str), Some("object"));
    }

    #[test]
    fn an_error_output_converts_to_an_is_error_result() {
        assert_eq!(to_rmcp_result(ToolOutput::error("x")).is_error, Some(true));
        let ok = to_rmcp_result(ToolOutput::structured(json!({ "a": 1 })));
        assert_eq!(ok.structured_content, Some(json!({ "a": 1 })));
    }
}
