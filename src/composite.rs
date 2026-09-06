//! Combining several [`ToolGroup`]s into one.
//!
//! Used by aggregate servers that expose the whole family's tools from a single
//! binary, and by hosts that embed several groups as library calls rather than
//! running each as a subprocess.
//!
//! Name collisions are the thing to get right. Two groups may legitimately both
//! want `search`, and silently letting the first win means the second's tool is
//! unreachable with no indication why. A prefix per group makes both available;
//! without one, the collision is reported rather than hidden.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::tool::{ToolDef, ToolFailure, ToolGroup, ToolOutput, ToolResult};

/// One member of a composite: a group plus an optional name prefix.
pub struct Member {
    pub prefix: Option<String>,
    pub group: Arc<dyn ToolGroup>,
}

impl Member {
    pub fn new(group: Arc<dyn ToolGroup>) -> Self {
        Self { prefix: None, group }
    }

    /// Namespaces this group's tools, e.g. `hex_` turns `view` into `hex_view`.
    #[must_use]
    pub fn prefixed(group: Arc<dyn ToolGroup>, prefix: impl Into<String>) -> Self {
        Self { prefix: Some(prefix.into()), group }
    }

    fn advertised(&self, name: &str) -> String {
        match self.prefix.as_deref().filter(|p| !p.is_empty()) {
            Some(prefix) if !name.starts_with(prefix) => format!("{prefix}{name}"),
            _ => name.to_string(),
        }
    }

    fn local(&self, advertised: &str) -> String {
        match self.prefix.as_deref().filter(|p| !p.is_empty()) {
            Some(prefix) => advertised.strip_prefix(prefix).unwrap_or(advertised).to_string(),
            None => advertised.to_string(),
        }
    }
}

/// Several tool groups presented as one.
pub struct Composite {
    members: Vec<Member>,
    /// Advertised tool name to the member that serves it, resolved once.
    routes: HashMap<String, usize>,
    tools: Vec<ToolDef>,
    /// Names more than one member claimed. Reported, never silently dropped.
    collisions: Vec<String>,
}

impl Composite {
    pub fn new(members: Vec<Member>) -> Self {
        let mut routes = HashMap::new();
        let mut tools = Vec::new();
        let mut collisions = Vec::new();

        for (index, member) in members.iter().enumerate() {
            for mut tool in member.group.tools() {
                let advertised = member.advertised(&tool.name);
                if routes.contains_key(&advertised) {
                    if !collisions.contains(&advertised) {
                        collisions.push(advertised.clone());
                    }
                    continue;
                }
                tool.name = advertised.clone();
                routes.insert(advertised, index);
                tools.push(tool);
            }
        }

        Self { members, routes, tools, collisions }
    }

    /// Tool names claimed by more than one member. The first claimant serves
    /// them; give one of the groups a prefix to expose both.
    pub fn collisions(&self) -> &[String] {
        &self.collisions
    }
}

#[async_trait]
impl ToolGroup for Composite {
    fn tools(&self) -> Vec<ToolDef> {
        self.tools.clone()
    }

    async fn call(&self, name: &str, args: Value) -> ToolResult<ToolOutput> {
        let index =
            *self.routes.get(name).ok_or_else(|| ToolFailure::NotFound(name.to_string()))?;
        let member = &self.members[index];
        member.group.call(&member.local(name), args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Stub(&'static [&'static str]);

    #[async_trait]
    impl ToolGroup for Stub {
        fn tools(&self) -> Vec<ToolDef> {
            self.0
                .iter()
                .map(|n| ToolDef::new(*n, "stub tool", json!({ "type": "object" })))
                .collect()
        }
        async fn call(&self, name: &str, _args: Value) -> ToolResult<ToolOutput> {
            Ok(ToolOutput::text(format!("{}::{name}", self.0[0])))
        }
    }

    fn composite(members: Vec<Member>) -> Composite {
        Composite::new(members)
    }

    #[tokio::test]
    async fn every_member_contributes_its_tools() {
        let c = composite(vec![
            Member::new(Arc::new(Stub(&["alpha", "beta"]))),
            Member::new(Arc::new(Stub(&["gamma"]))),
        ]);
        let names: Vec<String> = c.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn calls_reach_the_member_that_owns_the_tool() {
        let c = composite(vec![
            Member::new(Arc::new(Stub(&["alpha"]))),
            Member::new(Arc::new(Stub(&["gamma"]))),
        ]);
        assert_eq!(c.call("alpha", json!({})).await.unwrap().text, "alpha::alpha");
        assert_eq!(c.call("gamma", json!({})).await.unwrap().text, "gamma::gamma");
    }

    #[tokio::test]
    async fn a_prefix_namespaces_a_member_and_strips_on_dispatch() {
        let c = composite(vec![Member::prefixed(Arc::new(Stub(&["view"])), "hex_")]);
        assert_eq!(c.tools()[0].name, "hex_view");
        // The member must receive its own unprefixed name.
        assert_eq!(c.call("hex_view", json!({})).await.unwrap().text, "view::view");
    }

    #[tokio::test]
    async fn prefixes_let_two_members_expose_the_same_tool_name() {
        let c = composite(vec![
            Member::prefixed(Arc::new(Stub(&["search"])), "a_"),
            Member::prefixed(Arc::new(Stub(&["search"])), "b_"),
        ]);
        let names: Vec<String> = c.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["a_search", "b_search"]);
        assert!(c.collisions().is_empty());
    }

    #[tokio::test]
    async fn an_unprefixed_collision_is_reported_rather_than_hidden() {
        // Silently dropping the second makes its tool unreachable with no clue why.
        let c = composite(vec![
            Member::new(Arc::new(Stub(&["search"]))),
            Member::new(Arc::new(Stub(&["search"]))),
        ]);
        assert_eq!(c.tools().len(), 1, "the duplicate must not be advertised twice");
        assert_eq!(c.collisions(), ["search"]);
        // The first claimant serves it.
        assert_eq!(c.call("search", json!({})).await.unwrap().text, "search::search");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_not_found() {
        let c = composite(vec![Member::new(Arc::new(Stub(&["alpha"])))]);
        assert!(matches!(c.call("nope", json!({})).await, Err(ToolFailure::NotFound(_))));
    }

    #[tokio::test]
    async fn an_empty_composite_is_valid() {
        let c = composite(Vec::new());
        assert!(c.tools().is_empty());
        assert!(matches!(c.call("x", json!({})).await, Err(ToolFailure::NotFound(_))));
    }

    #[test]
    fn an_already_prefixed_name_is_not_prefixed_twice() {
        let member = Member::prefixed(Arc::new(Stub(&["hex_view"])), "hex_");
        assert_eq!(member.advertised("hex_view"), "hex_view");
    }
}
