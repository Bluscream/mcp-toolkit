//! `--single-tool`: collapse a server's tools into one dispatching tool.
//!
//! Some clients cap how many tools they will accept, or degrade badly past a
//! few dozen. Squashing lets a server with fifteen tools occupy one slot: the
//! caller picks the operation with a `tool` argument and passes that
//! operation's arguments alongside.
//!
//! The interesting problem is the merged schema. Two tools may declare the same
//! property with different types (`hex_view.length` is an integer,
//! `grep.length` might be a string). A silently-wrong merged type would make
//! the model send arguments the tool rejects, so collisions are detected and
//! the conflicting property is widened and documented rather than guessed at.

use serde_json::{Map, Value, json};

use crate::tool::ToolDef;

/// Builds the single dispatching tool from a server's real tools.
pub fn squash(server_name: &str, tools: &[ToolDef]) -> ToolDef {
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    let mut properties = Map::new();
    properties.insert(
        "tool".to_string(),
        json!({
            "type": "string",
            "description": "Which operation to run.",
            "enum": names,
        }),
    );

    let mut conflicts: Vec<String> = Vec::new();
    for tool in tools {
        let Some(source) = tool.schema.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, spec) in source {
            match properties.get(name) {
                None => {
                    properties.insert(name.clone(), annotate(spec, &tool.name));
                }
                Some(existing) if compatible(existing, spec) => {
                    properties.insert(name.clone(), extend_owner(existing, &tool.name));
                }
                Some(_) => {
                    // Same name, different type. Widening is honest; guessing is not.
                    if !conflicts.contains(name) {
                        conflicts.push(name.clone());
                    }
                    properties.insert(name.clone(), widened(name, tools));
                }
            }
        }
    }

    ToolDef::new(
        format!("{server_name}_tool"),
        describe(server_name, tools, &conflicts),
        json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": ["tool"],
        }),
    )
}

/// Records which operations accept a property, so the model can tell what
/// applies to the operation it picked.
fn annotate(spec: &Value, owner: &str) -> Value {
    let mut spec = spec.clone();
    if let Some(object) = spec.as_object_mut() {
        let base = object.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        let text = if base.is_empty() { format!("[{owner}]") } else { format!("[{owner}] {base}") };
        object.insert("description".to_string(), json!(text));
    }
    spec
}

fn extend_owner(existing: &Value, owner: &str) -> Value {
    let mut spec = existing.clone();
    if let Some(object) = spec.as_object_mut() {
        let current = object.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        // "[a] text" -> "[a, b] text"
        let updated = match current.strip_prefix('[').and_then(|r| r.split_once(']')) {
            Some((owners, rest)) if !owners.split(", ").any(|o| o == owner) => {
                format!("[{owners}, {owner}]{rest}")
            }
            Some(_) => current,
            None => format!("[{owner}] {current}"),
        };
        object.insert("description".to_string(), json!(updated));
    }
    spec
}

/// Two declarations agree if they specify the same `type` (or neither does).
fn compatible(a: &Value, b: &Value) -> bool {
    a.get("type") == b.get("type")
}

/// A property claimed by several tools with different types: drop the `type`
/// constraint and say which operation expects which.
fn widened(name: &str, tools: &[ToolDef]) -> Value {
    let mut notes: Vec<String> = Vec::new();
    for tool in tools {
        if let Some(spec) = tool.schema.get("properties").and_then(|p| p.get(name)) {
            let kind = spec.get("type").and_then(Value::as_str).unwrap_or("any");
            notes.push(format!("{}: {kind}", tool.name));
        }
    }
    json!({
        "description": format!(
            "Type depends on the chosen tool ({}). Pass the type that tool expects.",
            notes.join("; ")
        ),
    })
}

fn describe(server_name: &str, tools: &[ToolDef], conflicts: &[String]) -> String {
    use std::fmt::Write as _;

    let mut text = format!(
        "All {server_name} operations behind one tool. Set `tool` to the operation you want and \
         pass that operation's arguments alongside it.\n\nOperations:\n"
    );
    for tool in tools {
        let summary = tool.description.lines().next().unwrap_or_default();
        let args = tool.required();
        let required = if args.is_empty() {
            String::new()
        } else {
            format!(" (requires {})", args.join(", "))
        };
        let _ = writeln!(text, "  {}{required}: {summary}", tool.name);
    }
    if !conflicts.is_empty() {
        let _ = writeln!(
            text,
            "\nNote: {} take different types depending on the operation; see each property's \
             description.",
            conflicts.join(", ")
        );
    }
    text
}

/// Extracts the target operation and the arguments to forward to it.
pub fn dispatch(args: &Value) -> Result<(String, Value), crate::tool::ToolFailure> {
    use crate::tool::ToolFailure;

    let name = args
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ToolFailure::InvalidArguments(
                "single-tool mode requires a `tool` argument naming the operation".into(),
            )
        })?
        .to_string();

    let mut forwarded = args.clone();
    if let Some(object) = forwarded.as_object_mut() {
        object.remove("tool");
    }
    Ok((name, forwarded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<ToolDef> {
        vec![
            ToolDef::new(
                "hex_view",
                "Dump bytes.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File to read" },
                        "length": { "type": "integer" }
                    },
                    "required": ["path"]
                }),
            ),
            ToolDef::new(
                "hex_patch",
                "Patch bytes.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File to write" },
                        "hex_data": { "type": "string" }
                    },
                    "required": ["path", "hex_data"]
                }),
            ),
        ]
    }

    #[test]
    fn the_squashed_tool_is_named_after_the_server() {
        assert_eq!(squash("hex", &tools()).name, "hex_tool");
    }

    #[test]
    fn the_tool_selector_enumerates_every_operation() {
        let squashed = squash("hex", &tools());
        let names = &squashed.schema["properties"]["tool"]["enum"];
        assert_eq!(names, &json!(["hex_view", "hex_patch"]));
        assert_eq!(squashed.required(), ["tool"]);
    }

    #[test]
    fn arguments_from_every_operation_are_present() {
        let squashed = squash("hex", &tools());
        let props = squashed.schema["properties"].as_object().unwrap();
        for expected in ["tool", "path", "length", "hex_data"] {
            assert!(props.contains_key(expected), "{expected} missing");
        }
    }

    #[test]
    fn a_shared_property_lists_every_operation_that_accepts_it() {
        let squashed = squash("hex", &tools());
        let description =
            squashed.schema["properties"]["path"]["description"].as_str().unwrap().to_string();
        assert!(description.starts_with("[hex_view, hex_patch]"), "{description}");
    }

    #[test]
    fn an_exclusive_property_names_its_owner() {
        let squashed = squash("hex", &tools());
        let description =
            squashed.schema["properties"]["hex_data"]["description"].as_str().unwrap();
        assert!(description.starts_with("[hex_patch]"), "{description}");
    }

    #[test]
    fn a_type_conflict_is_widened_and_explained_rather_than_guessed() {
        // Two tools disagree on `length`: silently picking one would make the
        // model send arguments the other rejects.
        let conflicting = vec![
            ToolDef::new(
                "a",
                "A",
                json!({ "type": "object", "properties": { "length": { "type": "integer" } } }),
            ),
            ToolDef::new(
                "b",
                "B",
                json!({ "type": "object", "properties": { "length": { "type": "string" } } }),
            ),
        ];

        let squashed = squash("x", &conflicting);
        let spec = &squashed.schema["properties"]["length"];
        assert!(spec.get("type").is_none(), "the conflicting type must not be asserted");

        let description = spec["description"].as_str().unwrap();
        assert!(description.contains("a: integer"), "{description}");
        assert!(description.contains("b: string"), "{description}");
        assert!(squashed.description.contains("different types"), "{}", squashed.description);
    }

    #[test]
    fn the_description_lists_operations_with_their_required_arguments() {
        let text = squash("hex", &tools()).description;
        assert!(text.contains("hex_view (requires path)"), "{text}");
        assert!(text.contains("hex_patch (requires path, hex_data)"), "{text}");
    }

    #[test]
    fn dispatch_splits_the_operation_from_its_arguments() {
        let (name, args) =
            dispatch(&json!({ "tool": "hex_view", "path": "/x", "length": 4 })).unwrap();
        assert_eq!(name, "hex_view");
        assert_eq!(args, json!({ "path": "/x", "length": 4 }));
    }

    #[test]
    fn dispatch_without_a_tool_argument_says_what_is_missing() {
        let err = dispatch(&json!({ "path": "/x" })).unwrap_err();
        assert!(err.to_string().contains("`tool` argument"), "{err}");
    }

    #[test]
    fn squashing_an_empty_server_still_produces_a_valid_schema() {
        let squashed = squash("empty", &[]);
        assert_eq!(squashed.schema["properties"]["tool"]["enum"], json!([]));
        assert_eq!(squashed.schema["type"], "object");
    }

    #[test]
    fn a_tool_with_no_properties_is_tolerated() {
        let bare = vec![ToolDef::new("status", "Status.", json!({ "type": "object" }))];
        let squashed = squash("s", &bare);
        assert_eq!(squashed.schema["properties"]["tool"]["enum"], json!(["status"]));
    }
}
