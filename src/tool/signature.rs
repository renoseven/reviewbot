//! What a tool takes, declared once.
//!
//! The declaration is the source; the JSON Schema the vendor is shown and the
//! check a call is put through are both derived from it. Written the other way
//! round — a schema literal for the model and a hand-rolled check for the
//! arguments — "what the model was told" and "what reviewbot accepts" are two
//! facts about the same tool, kept in two places, and they drift: the schema
//! promises a field the check ignores, or the check refuses a shape the schema
//! allows. This file is the only place in the crate that writes JSON Schema.
//!
//! Validation lives here rather than in each tool for the same reason, and
//! because a refusal is an answer to the model: it has to name the tool, the
//! argument and what was wrong with it, so the next call can be right.

use serde_json::{Map, Value, json};

use crate::config::{ParamKind, ToolEntry};

use super::ToolError;

/// One declared argument.
pub struct Parameter {
    pub name: String,
    pub shape: Shape,
    pub required: bool,
    /// What the model is told this argument is for. Empty is allowed for a
    /// `[[tool]]` entry that gave no description; nothing builtin leaves it
    /// blank.
    pub description: String,
}

impl Parameter {
    pub fn required(name: &str, shape: Shape, description: &str) -> Self {
        Self {
            name: name.to_string(),
            shape,
            required: true,
            description: description.to_string(),
        }
    }

    pub fn optional(name: &str, shape: Shape, description: &str) -> Self {
        Self {
            name: name.to_string(),
            shape,
            required: false,
            description: description.to_string(),
        }
    }
}

/// What a value may be. `Path` is a string the tool will put through
/// `PathPolicy` afterwards: whether a path may be read is a permission
/// question and this file answers about shapes.
pub enum Shape {
    Text {
        pattern: Option<String>,
        choices: Option<Vec<String>>,
    },
    Path,
    Integer {
        minimum: Option<u64>,
        maximum: Option<u64>,
    },
    Number,
    Boolean,
    Array(Box<Shape>),
    Object(Vec<Parameter>),
}

impl Shape {
    pub fn text() -> Self {
        Shape::Text {
            pattern: None,
            choices: None,
        }
    }

    pub fn counting() -> Self {
        Shape::Integer {
            minimum: Some(1),
            maximum: None,
        }
    }

    pub fn line() -> Self {
        Shape::Integer {
            minimum: Some(0),
            maximum: None,
        }
    }

    pub fn percentage() -> Self {
        Shape::Integer {
            minimum: Some(0),
            maximum: Some(100),
        }
    }

    fn json_type(&self) -> &'static str {
        match self {
            Shape::Text { .. } | Shape::Path => "string",
            Shape::Integer { .. } => "integer",
            Shape::Number => "number",
            Shape::Boolean => "boolean",
            Shape::Array(_) => "array",
            Shape::Object(_) => "object",
        }
    }

    /// This shape as the vendor's schema language spells it.
    fn schema(&self) -> Value {
        let mut object = Map::new();
        object.insert(
            "type".to_string(),
            Value::String(self.json_type().to_string()),
        );
        match self {
            Shape::Text { pattern, choices } => {
                if let Some(pattern) = pattern {
                    object.insert("pattern".to_string(), Value::String(pattern.clone()));
                }
                if let Some(choices) = choices {
                    object.insert(
                        "enum".to_string(),
                        Value::Array(choices.iter().cloned().map(Value::String).collect()),
                    );
                }
            }
            Shape::Integer { minimum, maximum } => {
                if let Some(minimum) = minimum {
                    object.insert("minimum".to_string(), json!(minimum));
                }
                if let Some(maximum) = maximum {
                    object.insert("maximum".to_string(), json!(maximum));
                }
            }
            Shape::Array(element) => {
                object.insert("items".to_string(), element.schema());
            }
            Shape::Object(parameters) => {
                let inner = Signature::new(
                    parameters
                        .iter()
                        .map(|parameter| Parameter {
                            name: parameter.name.clone(),
                            shape: parameter.shape.clone_shape(),
                            required: parameter.required,
                            description: parameter.description.clone(),
                        })
                        .collect(),
                );
                return inner.schema();
            }
            Shape::Path | Shape::Number | Shape::Boolean => {}
        }
        Value::Object(object)
    }

    /// Shapes are declarations and are copied when a nested object renders its
    /// own schema. Written out rather than derived, because `Clone` on a
    /// declaration invites cloning it at call time, which is not what it is
    /// for.
    fn clone_shape(&self) -> Shape {
        match self {
            Shape::Text { pattern, choices } => Shape::Text {
                pattern: pattern.clone(),
                choices: choices.clone(),
            },
            Shape::Path => Shape::Path,
            Shape::Integer { minimum, maximum } => Shape::Integer {
                minimum: *minimum,
                maximum: *maximum,
            },
            Shape::Number => Shape::Number,
            Shape::Boolean => Shape::Boolean,
            Shape::Array(element) => Shape::Array(Box::new(element.clone_shape())),
            Shape::Object(parameters) => Shape::Object(
                parameters
                    .iter()
                    .map(|parameter| Parameter {
                        name: parameter.name.clone(),
                        shape: parameter.shape.clone_shape(),
                        required: parameter.required,
                        description: parameter.description.clone(),
                    })
                    .collect(),
            ),
        }
    }
}

/// Everything one tool takes. Held by the tool, so its schema and its checks
/// cannot come from two different lists.
pub struct Signature {
    parameters: Vec<Parameter>,
}

impl Signature {
    pub fn new(parameters: Vec<Parameter>) -> Self {
        Self { parameters }
    }

    /// Straight from a `[[tool]]` entry: a configured checker declares its
    /// arguments in the same language a builtin does, so both get the same
    /// schema and the same checking.
    pub fn from_entry(entry: &ToolEntry) -> Self {
        Self::new(
            entry
                .params
                .iter()
                .map(|(name, spec)| Parameter {
                    name: name.clone(),
                    shape: match spec.kind {
                        ParamKind::Path => Shape::Path,
                        ParamKind::String => Shape::Text {
                            pattern: spec.pattern.clone(),
                            choices: spec.choices.clone(),
                        },
                        ParamKind::Integer => Shape::Integer {
                            minimum: None,
                            maximum: None,
                        },
                        ParamKind::Number => Shape::Number,
                        ParamKind::Boolean => Shape::Boolean,
                    },
                    // Every declared parameter is required: an argv template
                    // has a hole for each of them, and there is nothing to
                    // put in a hole an optional one left empty.
                    required: true,
                    description: spec.description.clone().unwrap_or_default(),
                })
                .collect(),
        )
    }

    pub fn parameters(&self) -> &[Parameter] {
        &self.parameters
    }

    /// The JSON Schema the vendor is shown, derived rather than written.
    pub fn schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for parameter in &self.parameters {
            let mut property = parameter.shape.schema();
            if !parameter.description.is_empty()
                && let Some(object) = property.as_object_mut()
            {
                object.insert(
                    "description".to_string(),
                    Value::String(parameter.description.clone()),
                );
            }
            properties.insert(parameter.name.clone(), property);
            if parameter.required {
                required.push(Value::String(parameter.name.clone()));
            }
        }
        json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": Value::Array(required),
            "additionalProperties": false,
        })
    }

    /// The model's arguments, checked against the declaration and handed back
    /// with the shapes it wrote loosely read as the shapes it declared. A
    /// refusal names the argument, because the model's next move is to write
    /// that argument again.
    pub fn validate(&self, tool: &str, arguments: &Value) -> Result<Arguments, ToolError> {
        let checked = check_object(tool, "", &self.parameters, arguments)?;
        Ok(Arguments { values: checked })
    }
}

/// Validated arguments. Every value here is the shape its declaration asked
/// for, so reading one cannot fail.
#[derive(Debug)]
pub struct Arguments {
    values: Map<String, Value>,
}

impl Arguments {
    pub fn text(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(Value::as_str)
    }

    pub fn integer(&self, name: &str) -> Option<u64> {
        self.values.get(name).and_then(Value::as_u64)
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    pub fn values(&self) -> &Map<String, Value> {
        &self.values
    }
}

fn check_object(
    tool: &str,
    path: &str,
    parameters: &[Parameter],
    given: &Value,
) -> Result<Map<String, Value>, ToolError> {
    let Some(object) = given.as_object() else {
        return Err(invalid(
            tool,
            match path.is_empty() {
                true => "the arguments have to be a JSON object".to_string(),
                false => format!("{path} has to be a JSON object"),
            },
        ));
    };
    for name in object.keys() {
        if !parameters.iter().any(|parameter| &parameter.name == name) {
            return Err(invalid(
                tool,
                format!(
                    "unknown argument {:?}; this call takes {}",
                    qualify(path, name),
                    names(parameters)
                ),
            ));
        }
    }
    let mut checked = Map::new();
    for parameter in parameters {
        let named = qualify(path, &parameter.name);
        match object.get(&parameter.name) {
            None | Some(Value::Null) if parameter.required => {
                return Err(invalid(tool, format!("missing argument {named:?}")));
            }
            None | Some(Value::Null) => {}
            Some(value) => {
                checked.insert(
                    parameter.name.clone(),
                    check_value(tool, &named, &parameter.shape, value)?,
                );
            }
        }
    }
    Ok(checked)
}

fn check_value(tool: &str, named: &str, shape: &Shape, given: &Value) -> Result<Value, ToolError> {
    match shape {
        Shape::Text { pattern, choices } => {
            let text = as_text(tool, named, given)?;
            if let Some(choices) = choices
                && !choices.iter().any(|choice| choice == text)
            {
                return Err(invalid(
                    tool,
                    format!("argument {named:?} has to be one of {}", choices.join(", ")),
                ));
            }
            if let Some(pattern) = pattern {
                let compiled =
                    regex::Regex::new(pattern).map_err(|error| ToolError::Unavailable {
                        tool: tool.to_string(),
                        reason: format!(
                            "the pattern configured for {named:?} will not compile: {error}"
                        ),
                    })?;
                if !compiled.is_match(text) {
                    return Err(invalid(
                        tool,
                        format!("argument {named:?} does not match the pattern {pattern}"),
                    ));
                }
            }
            Ok(Value::String(text.to_string()))
        }
        Shape::Path => Ok(Value::String(as_text(tool, named, given)?.to_string())),
        Shape::Integer { minimum, maximum } => {
            let number = whole(given).ok_or_else(|| {
                invalid(tool, format!("argument {named:?} has to be a whole number"))
            })?;
            if let Some(minimum) = minimum
                && number < *minimum
            {
                return Err(invalid(
                    tool,
                    format!("argument {named:?} has to be {minimum} or more"),
                ));
            }
            if let Some(maximum) = maximum
                && number > *maximum
            {
                return Err(invalid(
                    tool,
                    format!("argument {named:?} has to be {maximum} or less"),
                ));
            }
            Ok(json!(number))
        }
        Shape::Number if given.is_number() => Ok(given.clone()),
        Shape::Number => Err(invalid(
            tool,
            format!("argument {named:?} has to be a number"),
        )),
        Shape::Boolean if given.is_boolean() => Ok(given.clone()),
        Shape::Boolean => Err(invalid(
            tool,
            format!("argument {named:?} has to be true or false"),
        )),
        Shape::Array(element) => {
            let Some(items) = given.as_array() else {
                return Err(invalid(
                    tool,
                    format!("argument {named:?} has to be a list"),
                ));
            };
            let mut checked = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                checked.push(check_value(
                    tool,
                    &format!("{named}[{index}]"),
                    element,
                    item,
                )?);
            }
            Ok(Value::Array(checked))
        }
        Shape::Object(parameters) => {
            Ok(Value::Object(check_object(tool, named, parameters, given)?))
        }
    }
}

fn as_text<'a>(tool: &str, named: &str, given: &'a Value) -> Result<&'a str, ToolError> {
    let text = given
        .as_str()
        .ok_or_else(|| invalid(tool, format!("argument {named:?} has to be a string")))?;
    if text.is_empty() {
        return Err(invalid(tool, format!("argument {named:?} is empty")));
    }
    Ok(text)
}

/// A whole number, written as one or written as a string. Models that speak
/// function calls stringify integers often enough that refusing `"12"` buys a
/// whole round trip and the same 12 back — the schema says integer and the
/// vendor does not enforce it. Nothing else is read as a number: `"12.5"`,
/// `12.5` and `"twelve"` are all still refused, because rounding or truncating
/// would be inventing a value the model never wrote.
fn whole(given: &Value) -> Option<u64> {
    match given {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn qualify(path: &str, name: &str) -> String {
    match path.is_empty() {
        true => name.to_string(),
        false => format!("{path}.{name}"),
    }
}

fn names(parameters: &[Parameter]) -> String {
    parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn invalid(tool: &str, reason: String) -> ToolError {
    ToolError::InvalidArguments {
        tool: tool.to_string(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature() -> Signature {
        Signature::new(vec![
            Parameter::required("path", Shape::Path, "Repository-relative path"),
            Parameter::optional("first_line", Shape::counting(), "Start of the range"),
            Parameter::required("score", Shape::percentage(), "0 to 100"),
            Parameter::required(
                "evidence",
                Shape::Object(vec![
                    Parameter::required(
                        "diff_lines",
                        Shape::Array(Box::new(Shape::line())),
                        "Lines this rests on",
                    ),
                    Parameter::optional("note", Shape::text(), "One sentence"),
                ]),
                "What the finding rests on",
            ),
        ])
    }

    fn ok() -> Value {
        json!({
            "path": "src/parse.c",
            "score": 92,
            "evidence": {"diff_lines": [11, 12]},
        })
    }

    /// The schema is derived, so a declaration that changes cannot leave the
    /// model reading the old one.
    #[test]
    fn the_schema_is_written_from_the_declaration() {
        let schema = signature().schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(
            schema["properties"]["path"]["description"],
            "Repository-relative path"
        );
        assert_eq!(schema["properties"]["score"]["minimum"], 0);
        assert_eq!(schema["properties"]["score"]["maximum"], 100);
        assert_eq!(schema["properties"]["first_line"]["minimum"], 1);
        assert_eq!(
            schema["properties"]["evidence"]["properties"]["diff_lines"]["type"],
            "array"
        );
        assert_eq!(
            schema["properties"]["evidence"]["properties"]["diff_lines"]["items"]["type"],
            "integer"
        );
        assert_eq!(schema["additionalProperties"], false);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["path", "score", "evidence"]);
        assert_eq!(
            schema["properties"]["evidence"]["required"][0], "diff_lines",
            "a nested object declares its own required fields"
        );
    }

    /// The habit this exists for: a model that speaks function calls writes an
    /// integer as a string, and a re-ask gets the same number back one round
    /// trip later. Nested and inside lists as well, which is where the scores
    /// and the line numbers actually live.
    #[test]
    fn a_stringified_whole_number_is_that_number_at_every_depth() {
        let mut arguments = ok();
        arguments["score"] = json!("92");
        arguments["evidence"]["diff_lines"] = json!(["11", 12]);
        let checked = signature()
            .validate("submit_comment", &arguments)
            .expect("accepted");
        assert_eq!(checked.integer("score"), Some(92));
        assert_eq!(
            checked.get("evidence").expect("evidence")["diff_lines"],
            json!([11, 12])
        );
    }

    #[test]
    fn nothing_else_is_read_as_a_number() {
        for bad in [json!("92.5"), json!(92.5), json!("high"), json!(101)] {
            let mut arguments = ok();
            arguments["score"] = bad.clone();
            let error = signature()
                .validate("submit_comment", &arguments)
                .expect_err("refused");
            assert!(error.to_string().contains("score"), "{bad}: {error}");
        }
    }

    /// A refusal is an answer to the model, so it has to name the argument and
    /// the tool: the next thing it does is write that argument again.
    #[test]
    fn a_refusal_names_the_argument_and_the_tool() {
        let mut missing = ok();
        missing.as_object_mut().expect("object").remove("path");
        let error = signature()
            .validate("read_file", &missing)
            .expect_err("refused");
        assert!(error.to_string().starts_with("read_file:"), "{error}");
        assert!(error.to_string().contains("\"path\""), "{error}");

        let mut unknown = ok();
        unknown["depth"] = json!(3);
        let error = signature()
            .validate("read_file", &unknown)
            .expect_err("refused");
        assert!(error.to_string().contains("unknown argument"), "{error}");
        assert!(error.to_string().contains("path"), "{error}");

        let mut nested = ok();
        nested["evidence"]["extra"] = json!(1);
        let error = signature()
            .validate("read_file", &nested)
            .expect_err("refused");
        assert!(
            error.to_string().contains("evidence.extra"),
            "a nested name says where it is: {error}"
        );
    }

    #[test]
    fn an_omitted_optional_stays_omitted_rather_than_becoming_a_default() {
        let checked = signature().validate("read_file", &ok()).expect("accepted");
        assert_eq!(checked.integer("first_line"), None);
        assert!(checked.get("first_line").is_none());
    }

    /// A configured checker declares its arguments in the same language, so it
    /// is checked by the same code rather than by a second implementation.
    #[test]
    fn a_config_entry_declares_arguments_the_same_way() {
        use crate::config::ParamSpec;
        use std::collections::BTreeMap;

        let mut params = BTreeMap::new();
        params.insert(
            "path".to_string(),
            ParamSpec {
                kind: ParamKind::Path,
                description: Some("the file to check".to_string()),
                pattern: None,
                choices: None,
            },
        );
        params.insert(
            "level".to_string(),
            ParamSpec {
                kind: ParamKind::String,
                description: None,
                pattern: None,
                choices: Some(vec!["all".to_string(), "style".to_string()]),
            },
        );
        let entry = ToolEntry {
            name: "cppcheck".to_string(),
            description: "static analysis".to_string(),
            bin: std::path::PathBuf::from("/usr/bin/cppcheck"),
            args: vec!["{path}".to_string()],
            params,
            requires_checkout: true,
            requires_build: false,
            timeout_ms: 5_000,
        };
        let signature = Signature::from_entry(&entry);
        let schema = signature.schema();
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(
            schema["properties"]["path"]["description"],
            "the file to check"
        );
        assert_eq!(schema["properties"]["level"]["enum"][0], "all");

        let error = signature
            .validate("cppcheck", &json!({"path": "src/parse.c", "level": "deep"}))
            .expect_err("not one of the choices");
        assert!(error.to_string().contains("all, style"), "{error}");
    }
}
