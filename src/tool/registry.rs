//! One registry, one lookup path. The core never matches on a tool name.

use super::{Purpose, Round, Tool, ToolError, ToolOutput, ToolSchema};

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

    /// The checkers of this run, which `review` needs so it can say that one
    /// was registered and never called. By purpose, not by where the tool was
    /// written: what matters is that its answer means something.
    pub fn names_with_purpose(&self, purpose: Purpose) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|tool| tool.purpose() == purpose)
            .map(|tool| tool.name())
            .collect()
    }

    /// What one round advertises, which is also exactly what it accepts.
    pub fn schemas_for(&self, round: Round) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .filter(|tool| tool.offered_on(round))
            .map(|tool| tool.schema())
            .collect()
    }

    pub fn offered_on(&self, round: Round) -> Vec<&dyn Tool> {
        self.tools
            .iter()
            .filter(|tool| tool.offered_on(round))
            .map(|tool| tool.as_ref())
            .collect()
    }

    /// Look the tool up and let it check its own arguments against the
    /// signature it published. Nothing here knows any tool's shape.
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
    use super::super::{Purpose, Round, Signature, Tool, ToolError, ToolOutput};
    use super::*;

    struct Echo {
        signature: Signature,
    }

    impl Echo {
        fn new() -> Self {
            Self {
                signature: Signature::new(Vec::new()),
            }
        }
    }

    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "returns its argument"
        }

        fn signature(&self) -> &Signature {
            &self.signature
        }

        fn purpose(&self) -> Purpose {
            Purpose::Content
        }

        fn rounds(&self) -> &'static [Round] {
            &[Round::Investigation]
        }

        fn execute(&self, arguments: &serde_json::Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::new(arguments.to_string()))
        }
    }

    #[test]
    fn execution_goes_through_the_registry_not_a_name_match() {
        let mut registry = Registry::new();
        registry.register(Box::new(Echo::new()));
        assert_eq!(registry.names(), vec!["echo"]);
        let output = registry
            .execute("echo", &serde_json::json!({}))
            .expect("executed");
        assert!(output.text.contains("{}"), "{}", output.text);
    }

    #[test]
    fn an_unregistered_tool_is_reported_rather_than_guessed_at() {
        let registry = Registry::new();
        assert!(matches!(
            registry.execute("cppcheck", &serde_json::json!({})),
            Err(ToolError::Unavailable { .. })
        ));
    }

    /// A round shows what it accepts and nothing else: a tool the model can
    /// see but cannot use this round is an action guaranteed to fail.
    #[test]
    fn a_round_advertises_only_the_tools_it_will_accept() {
        let mut registry = Registry::new();
        registry.register(Box::new(Echo::new()));
        assert_eq!(registry.schemas_for(Round::Investigation).len(), 1);
        assert!(registry.schemas_for(Round::Conclusion).is_empty());
        assert!(registry.schemas_for(Round::Scoring).is_empty());
    }
}
