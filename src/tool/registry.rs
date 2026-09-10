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

    /// Every tool this config defines, whatever this run's worktree can do
    /// with it. What varies between runs is not the set of names but what the
    /// worktree behind them can answer, and the tool says that itself.
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

    /// Every tool, in registration order. `tool list` reads its rows off
    /// these, so the catalog is the registry rather than a second description
    /// of it.
    pub fn all(&self) -> Vec<&dyn Tool> {
        self.tools.iter().map(|tool| tool.as_ref()).collect()
    }

    /// The tools of one purpose that this run's worktree can actually answer,
    /// which is what `review` needs so it can say a checker went unused. By
    /// purpose, not by where the tool was written: what matters is that its
    /// answer means something. And usable rather than merely registered,
    /// because a checker this run cannot answer is not a checker nobody
    /// bothered to call.
    pub fn usable_with_purpose(&self, purpose: Purpose) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|tool| tool.purpose() == purpose && tool.unavailable().is_none())
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

    /// The same list, in the shape a `Request` carries. Purpose stays here;
    /// the wire type has no slot for it.
    pub fn request_schemas(&self, round: Round) -> Vec<crate::protocol::ToolSchema> {
        self.schemas_for(round)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    pub fn offered_on(&self, round: Round) -> Vec<&dyn Tool> {
        self.tools
            .iter()
            .filter(|tool| tool.offered_on(round))
            .map(|tool| tool.as_ref())
            .collect()
    }

    /// Look the tool up, ask the worktree whether it can be answered at all,
    /// and let it check its own arguments against the signature it published.
    /// Nothing here knows any tool's shape.
    ///
    /// The availability question is asked here rather than inside each
    /// `execute`, because every tool would otherwise have to remember to ask
    /// it, and the one that forgot would spawn a checker against a worktree
    /// holding four files and hand the model its missing-include screen.
    pub fn execute(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.get(name).ok_or_else(|| ToolError::Unavailable {
            tool: name.to_string(),
            reason: "no tool with that name is registered".to_string(),
        })?;
        if let Some(reason) = tool.unavailable() {
            return Err(ToolError::Unavailable {
                tool: name.to_string(),
                reason: reason.to_string(),
            });
        }
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

    /// A tool this run's worktree cannot answer is registered like any other
    /// and refused here, before it can reach out and do half the job. The
    /// reason travels verbatim: it is the only thing standing between "this
    /// run could not look" and a finding that says the code is fine.
    #[test]
    fn a_tool_the_worktree_cannot_answer_is_refused_before_it_runs() {
        struct Grounded;

        impl Tool for Grounded {
            fn name(&self) -> &str {
                "grounded"
            }

            fn description(&self) -> &str {
                "never gets that far"
            }

            fn signature(&self) -> &Signature {
                static EMPTY: std::sync::OnceLock<Signature> = std::sync::OnceLock::new();
                EMPTY.get_or_init(|| Signature::new(Vec::new()))
            }

            fn purpose(&self) -> Purpose {
                Purpose::Check
            }

            fn rounds(&self) -> &'static [Round] {
                &[Round::Investigation]
            }

            fn unavailable(&self) -> Option<&str> {
                Some("this run's worktree is not a checkout, which says nothing about the code")
            }

            fn execute(&self, _arguments: &serde_json::Value) -> Result<ToolOutput, ToolError> {
                panic!("a tool that cannot answer must not be reached");
            }
        }

        let mut registry = Registry::new();
        registry.register(Box::new(Grounded));
        registry.register(Box::new(Echo::new()));

        assert_eq!(
            registry.names(),
            vec!["grounded", "echo"],
            "offered all the same: the model decides what to call"
        );
        assert!(
            registry.usable_with_purpose(Purpose::Check).is_empty(),
            "and a checker that would refuse is not one nobody bothered to call"
        );
        let refused = registry
            .execute("grounded", &serde_json::json!({}))
            .expect_err("cannot be answered");
        assert!(
            matches!(&refused, ToolError::Unavailable { reason, .. }
                     if reason.contains("says nothing about the code")),
            "{refused}"
        );
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
