//! One registry, one lookup path. The core never matches on a tool name.

use super::{Origin, Tool, ToolError, ToolOutput, ToolSchema};

#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A tool whose preconditions are unmet is not registered: what the model
    /// can see always equals what it can really call.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// The external checkers of this run, which `review` needs so it can say
    /// that one was registered and never called.
    pub fn names_with_origin(&self, origin: Origin) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|tool| tool.origin() == origin)
            .map(|tool| tool.name())
            .collect()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|tool| tool.schema()).collect()
    }

    /// What remains after investigation tools have been withdrawn.
    pub fn concluding_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .filter(|tool| tool.available_when_concluding())
            .map(|tool| tool.schema())
            .collect()
    }

    /// Validate nothing here beyond "the tool exists"; argument checking is
    /// the tool's own job, because only it knows its schema.
    pub fn execute(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.get(name).ok_or_else(|| ToolError::Unavailable {
            tool: name.to_string(),
            reason: "no tool with that name is registered".to_string(),
        })?;
        tool.execute(arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Origin, Tool, ToolError, ToolOutput};
    use super::*;

    struct Echo;

    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "returns its argument"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn origin(&self) -> Origin {
            Origin::Builtin
        }

        fn execute(&self, arguments: &serde_json::Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::new(arguments.to_string()))
        }
    }

    #[test]
    fn execution_goes_through_the_registry_not_a_name_match() {
        let mut registry = Registry::new();
        registry.register(Box::new(Echo));
        assert_eq!(registry.names(), vec!["echo"]);
        let output = registry
            .execute("echo", &serde_json::json!({"path": "src/parse.c"}))
            .unwrap();
        assert!(output.text.contains("src/parse.c"));
    }

    #[test]
    fn an_unregistered_tool_is_reported_rather_than_guessed_at() {
        let registry = Registry::new();
        assert!(matches!(
            registry.execute("cppcheck", &serde_json::json!({})),
            Err(ToolError::Unavailable { .. })
        ));
    }
}
