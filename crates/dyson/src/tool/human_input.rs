//! Human input is a tool result supplied by a later authenticated signal.
use super::*;
use dyson_harness::continuation::HumanInputRequest;

pub struct HumanInputTool;

fn validate_form_schema(schema: &serde_json::Value) -> std::result::Result<(), &'static str> {
    if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err("answer schema must have type object");
    }
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .ok_or("answer schema needs properties")?;
    if properties.is_empty() || properties.len() > 32 {
        return Err("answer schema needs 1 to 32 fields");
    }
    let top_keys = [
        "type",
        "properties",
        "required",
        "additionalProperties",
        "title",
        "description",
    ];
    if schema
        .as_object()
        .is_some_and(|o| o.keys().any(|k| !top_keys.contains(&k.as_str())))
    {
        return Err("unsupported answer schema keyword");
    }
    let field_keys = [
        "type",
        "title",
        "description",
        "default",
        "enum",
        "enumNames",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
    ];
    for spec in properties.values() {
        let object = spec.as_object().ok_or("field schema must be an object")?;
        if object.keys().any(|k| !field_keys.contains(&k.as_str())) {
            return Err("unsupported answer field constraint");
        }
        if !matches!(
            spec.get("type").and_then(serde_json::Value::as_str),
            Some("string" | "number" | "integer" | "boolean")
        ) {
            return Err("answer fields must have primitive types");
        }
        for key in ["minLength", "maxLength"] {
            if spec.get(key).is_some_and(|v| v.as_u64().is_none()) {
                return Err("length constraints must be nonnegative integers");
            }
        }
        for key in ["minimum", "maximum"] {
            if spec.get(key).is_some_and(|v| !v.is_number()) {
                return Err("numeric constraints must be numbers");
            }
        }
        if let Some(choices) = spec.get("enum") {
            let choices = choices
                .as_array()
                .filter(|v| !v.is_empty())
                .ok_or("enum must contain choices")?;
            for choice in choices {
                super::validate_tool_input(spec, choice)
                    .map_err(|_| "enum choice does not match its field schema")?;
            }
        }
    }
    if let Some(required) = schema.get("required") {
        let required = required.as_array().ok_or("required must be an array")?;
        if required
            .iter()
            .any(|k| k.as_str().is_none_or(|k| !properties.contains_key(k)))
        {
            return Err("required must name defined fields");
        }
    }
    if schema
        .get("additionalProperties")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("additionalProperties must be a boolean");
    }
    Ok(())
}

#[async_trait]
impl Tool for HumanInputTool {
    fn name(&self) -> &str {
        "request_human_input"
    }
    fn agent_only(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Ask the user a question and durably pause this run until they answer. Use an object schema with primitive fields. The user can accept, decline, or cancel. Child agents should return their question to their parent instead."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "question":{"type":"string","minLength":1,"maxLength":8192},
            "schema":{"type":"object","description":"Object JSON Schema for the answer; default is a required text answer."}
        },"required":["question"],"additionalProperties":false})
    }
    fn execution_plan(&self, _: &serde_json::Value, _: &ToolContext) -> ToolExecutionPlan {
        // A question is a barrier: no later calls in the batch execute before an answer.
        ToolExecutionPlan::read("global:tool-execution")
    }
    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "Return this question as a blocker to the parent agent.",
            ));
        }
        let question = required_str(input, "question", self.name())?.trim();
        if question.is_empty() {
            return Ok(ToolOutput::error("question must not be empty"));
        }
        let schema = input.get("schema").cloned().unwrap_or_else(|| serde_json::json!({
            "type":"object","properties":{"answer":{"type":"string","minLength":1}},"required":["answer"]
        }));
        if let Err(error) = validate_form_schema(&schema) {
            return Ok(ToolOutput::error(error));
        }
        let mut result = ToolOutput::success("");
        result.human_input = Some(HumanInputRequest {
            id: ctx
                .tool_use_id
                .clone()
                .ok_or_else(|| DysonError::tool(self.name(), "a run tool call is required"))?,
            question: question.into(),
            schema,
        });
        Ok(result)
    }
}
