use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: String) -> Self {
        Self {
            role: role.to_owned(),
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn tool(id: String, content: String) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: Some(id),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    pub command: String,
}

pub fn parse_bash_args(arguments: &str) -> Result<BashArgs> {
    // Serde's derived struct visitor also accepts arrays; the tool requires an object.
    ensure!(
        arguments.trim_start().starts_with('{'),
        "Invalid bash arguments; expected {{command: string}}"
    );
    serde_json::from_str(arguments).context("Invalid bash arguments; expected {command: string}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_and_tool_wire_shapes() {
        assert_eq!(
            serde_json::to_value(ChatMessage::text("user", "hi".into())).unwrap(),
            json!({"role":"user", "content":"hi"})
        );
        assert_eq!(
            serde_json::to_value(ChatMessage::tool("c".into(), "ok".into())).unwrap(),
            json!({"role":"tool", "content":"ok", "tool_call_id":"c"})
        );
    }

    #[test]
    fn arguments_require_exact_object_shape() {
        assert_eq!(parse_bash_args(r#"{"command":""}"#).unwrap().command, "");
        for bad in [
            "{",
            "null",
            "[]",
            r#"["ls"]"#,
            "{}",
            r#"{"command":1}"#,
            r#"{"command":"a","command":"b"}"#,
            r#"{"command":"ls","timeout_ms":1}"#,
        ] {
            assert!(parse_bash_args(bad).is_err(), "{bad}");
        }
    }
}
