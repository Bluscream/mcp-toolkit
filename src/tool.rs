//! The tool abstraction every server built on this kit implements.
//!
//! Deliberately independent of `rmcp`: a tool group is plain data plus an async
//! function, so tool crates can be unit-tested without standing up a protocol
//! server, and can be embedded directly by a host process (omni-mcp) that never
//! speaks MCP to them at all.

use async_trait::async_trait;
use serde_json::{Value, json};

/// A tool's public description: what it is called, what it does, what it takes.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub schema: Value,
}

impl ToolDef {
    pub fn new(name: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        Self { name: name.into(), description: description.into(), schema }
    }

    /// The declared property names, in schema order.
    pub fn properties(&self) -> Vec<String> {
        self.schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The names listed in `required`.
    pub fn required(&self) -> Vec<String> {
        self.schema
            .get("required")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }
}

/// What a tool produces. Mirrors MCP's content model without depending on it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    /// Machine-readable form, surfaced as `structuredContent`.
    pub structured: Option<Value>,
    /// A tool that ran but failed. This is a *result* the model should read and
    /// react to, not a transport error.
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), structured: None, is_error: false }
    }

    /// Pretty JSON for the model plus the raw value for clients that use it.
    pub fn structured(value: Value) -> Self {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Self { text, structured: Some(value), is_error: false }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), structured: None, is_error: true }
    }

    #[must_use]
    pub fn flagged_as_error(mut self) -> Self {
        self.is_error = true;
        self
    }
}

/// Why a tool call could not be completed.
///
/// The distinction matters on the wire: `InvalidArguments` and `NotFound` are
/// protocol errors the caller got wrong, while `Failed` and `Timeout` are
/// outcomes the model should see as tool results and adapt to.
#[derive(Debug, thiserror::Error)]
pub enum ToolFailure {
    #[error("{0}")]
    InvalidArguments(String),

    #[error("tool {0:?} is not available")]
    NotFound(String),

    #[error("{0}")]
    Failed(String),

    #[error("{tool:?} exceeded its {seconds}s timeout")]
    Timeout { tool: String, seconds: u64 },

    #[error("{0}")]
    Denied(String),
}

impl ToolFailure {
    /// Whether this should be reported as a tool result rather than an RPC error.
    pub fn is_tool_level(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::Timeout { .. } | Self::Denied(_))
    }
}

pub type ToolResult<T> = Result<T, ToolFailure>;

/// A group of related tools.
#[async_trait]
pub trait ToolGroup: Send + Sync + 'static {
    /// Tools this group advertises.
    fn tools(&self) -> Vec<ToolDef>;

    /// Runs one. `name` is one of the advertised names.
    async fn call(&self, name: &str, args: Value) -> ToolResult<ToolOutput>;
}

/// Adds an optional `timeout` property so any caller can bound a slow tool.
/// Returns whether the schema was modified.
pub fn ensure_timeout_property(schema: &mut Value, max_seconds: u64) -> bool {
    let Some(object) = schema.as_object_mut() else { return false };
    let properties = object.entry("properties").or_insert_with(|| json!({}));
    let Some(properties) = properties.as_object_mut() else { return false };

    if properties.contains_key("timeout") || properties.contains_key("timeout_ms") {
        return false;
    }
    properties.insert(
        "timeout".to_string(),
        json!({
            "type": "integer",
            "description":
                format!("Optional. Abort this call after this many seconds (max {max_seconds})."),
        }),
    );
    true
}

/// Reads a caller-supplied timeout, clamped to `[1, max]`.
pub fn requested_timeout(args: &Value, default_seconds: u64, max_seconds: u64) -> u64 {
    args.get("timeout")
        .and_then(Value::as_u64)
        .or_else(|| args.get("timeout_ms").and_then(Value::as_u64).map(|ms| ms.div_ceil(1000)))
        .unwrap_or(default_seconds)
        .clamp(1, max_seconds.max(1))
}

/// Removes the injected `timeout` before the arguments reach a tool that never
/// declared it.
pub fn strip_timeout(mut args: Value) -> Value {
    if let Some(object) = args.as_object_mut() {
        object.remove("timeout");
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "depth": { "type": "integer" } },
            "required": ["path"]
        })
    }

    #[test]
    fn a_definition_exposes_its_properties_and_required_fields() {
        let tool = ToolDef::new("t", "d", schema());
        assert_eq!(tool.properties(), ["path", "depth"]);
        assert_eq!(tool.required(), ["path"]);
    }

    #[test]
    fn a_schema_without_properties_yields_empty_lists() {
        let tool = ToolDef::new("t", "d", json!({ "type": "object" }));
        assert!(tool.properties().is_empty());
        assert!(tool.required().is_empty());
    }

    #[test]
    fn structured_output_carries_both_renderings() {
        let out = ToolOutput::structured(json!({ "count": 2 }));
        assert_eq!(out.structured, Some(json!({ "count": 2 })));
        assert!(out.text.contains("\"count\": 2"));
        assert!(!out.is_error);
    }

    #[test]
    fn failures_are_classified_for_the_wire() {
        assert!(ToolFailure::Failed("x".into()).is_tool_level());
        assert!(ToolFailure::Denied("x".into()).is_tool_level());
        assert!(ToolFailure::Timeout { tool: "t".into(), seconds: 1 }.is_tool_level());
        // These are the caller's mistake, so they surface as protocol errors.
        assert!(!ToolFailure::InvalidArguments("x".into()).is_tool_level());
        assert!(!ToolFailure::NotFound("x".into()).is_tool_level());
    }

    #[test]
    fn a_timeout_property_is_injected_once() {
        let mut s = schema();
        assert!(ensure_timeout_property(&mut s, 300));
        assert_eq!(s["properties"]["timeout"]["type"], "integer");
        // Idempotent.
        assert!(!ensure_timeout_property(&mut s, 300));
    }

    #[test]
    fn a_tool_declaring_its_own_timeout_is_left_alone() {
        let mut s = json!({ "properties": { "timeout": { "type": "string" } } });
        assert!(!ensure_timeout_property(&mut s, 300));
        assert_eq!(s["properties"]["timeout"]["type"], "string");

        let mut s = json!({ "properties": { "timeout_ms": { "type": "integer" } } });
        assert!(!ensure_timeout_property(&mut s, 300));
    }

    #[test]
    fn the_requested_timeout_is_read_and_clamped() {
        assert_eq!(requested_timeout(&json!({}), 30, 300), 30);
        assert_eq!(requested_timeout(&json!({ "timeout": 7 }), 30, 300), 7);
        assert_eq!(requested_timeout(&json!({ "timeout_ms": 4500 }), 30, 300), 5);
        assert_eq!(requested_timeout(&json!({ "timeout": 9999 }), 30, 300), 300);
        assert_eq!(requested_timeout(&json!({ "timeout": 0 }), 30, 300), 1);
    }

    #[test]
    fn the_injected_timeout_is_stripped_before_dispatch() {
        assert_eq!(strip_timeout(json!({ "a": 1, "timeout": 5 })), json!({ "a": 1 }));
        assert_eq!(strip_timeout(json!([1])), json!([1]));
    }
}
