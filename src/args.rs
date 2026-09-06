//! Typed argument extraction.
//!
//! Shared by every tool server, so a given crate uses only part of it.
//!
//! The old modules used `.unwrap_or("")` for required arguments, so a caller
//! that omitted `pattern` silently searched for the empty string instead of
//! being told what it got wrong. Every accessor here either yields the right
//! type or an error naming the field.

use serde_json::Value;

use crate::tool::{ToolFailure, ToolResult};

/// Names the field a caller left out.
pub fn missing(field: &str) -> ToolFailure {
    ToolFailure::InvalidArguments(format!("missing required argument {field:?}"))
}

/// Reads a required string.
pub fn string<'a>(args: &'a Value, field: &str) -> ToolResult<&'a str> {
    match args.get(field) {
        None | Some(Value::Null) => Err(missing(field)),
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(type_error(field, "string", other)),
    }
}

/// Reads an optional string. A JSON `null` is treated as absent.
pub fn opt_string<'a>(args: &'a Value, field: &str) -> ToolResult<Option<&'a str>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(type_error(field, "string", other)),
    }
}

/// Reads an optional non-negative integer.
pub fn opt_u64(args: &Value, field: &str) -> ToolResult<Option<u64>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| {
                ToolFailure::InvalidArguments(format!(
                    "argument {field:?} must be a non-negative whole number, got {n}"
                ))
            })
            .map(Some),
        Some(other) => Err(type_error(field, "integer", other)),
    }
}

/// Reads an optional integer, falling back to `default`.
pub fn u64_or(args: &Value, field: &str, default: u64) -> ToolResult<u64> {
    Ok(opt_u64(args, field)?.unwrap_or(default))
}

/// Reads an optional boolean. A JSON `null` is treated as absent.
pub fn opt_bool(args: &Value, field: &str) -> ToolResult<Option<bool>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(type_error(field, "boolean", other)),
    }
}

/// Reads an optional boolean, falling back to `default`.
pub fn bool_or(args: &Value, field: &str, default: bool) -> ToolResult<bool> {
    Ok(opt_bool(args, field)?.unwrap_or(default))
}

fn type_error(field: &str, expected: &str, actual: &Value) -> ToolFailure {
    ToolFailure::InvalidArguments(format!(
        "argument {field:?} must be {} {expected}, got {}",
        if expected == "integer" { "an" } else { "a" },
        describe(actual)
    ))
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_present_values() {
        let args = json!({ "s": "x", "n": 5, "b": true });
        assert_eq!(string(&args, "s").unwrap(), "x");
        assert_eq!(opt_u64(&args, "n").unwrap(), Some(5));
        assert!(bool_or(&args, "b", false).unwrap());
    }

    #[test]
    fn a_missing_required_string_names_the_field() {
        let err = string(&json!({}), "pattern").unwrap_err();
        assert!(err.to_string().contains("\"pattern\""));
        assert!(matches!(err, ToolFailure::InvalidArguments(_)));
    }

    #[test]
    fn null_counts_as_absent_for_optional_fields() {
        let args = json!({ "s": null, "n": null, "b": null });
        assert_eq!(opt_string(&args, "s").unwrap(), None);
        assert_eq!(opt_u64(&args, "n").unwrap(), None);
        assert!(bool_or(&args, "b", true).unwrap());
    }

    #[test]
    fn null_does_not_satisfy_a_required_field() {
        assert!(string(&json!({ "s": null }), "s").is_err());
    }

    #[test]
    fn wrong_types_are_rejected_rather_than_coerced() {
        assert!(string(&json!({ "s": 1 }), "s").is_err());
        assert!(opt_u64(&json!({ "n": "5" }), "n").is_err());
        assert!(bool_or(&json!({ "b": "true" }), "b", false).is_err());
    }

    #[test]
    fn negative_and_fractional_numbers_are_rejected_for_integer_fields() {
        assert!(opt_u64(&json!({ "n": -1 }), "n").is_err());
        assert!(opt_u64(&json!({ "n": 1.5 }), "n").is_err());
    }

    #[test]
    fn defaults_apply_only_when_absent() {
        assert_eq!(u64_or(&json!({}), "n", 42).unwrap(), 42);
        assert_eq!(u64_or(&json!({ "n": 0 }), "n", 42).unwrap(), 0);
    }

    #[test]
    fn error_messages_describe_the_actual_type() {
        let err = string(&json!({ "s": [] }), "s").unwrap_err().to_string();
        assert!(err.contains("an array"), "{err}");
    }
}
