use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
};

use crate::types::{BashArgs, ChatMessage, CompletedReply, FunctionCall, ToolCall};
use anyhow::{Context, Result, ensure};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    tools: [ToolDefinition; 1],
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct ChatChunk {
    choices: Vec<ChatChoice>,
    usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatDelta {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDefinition,
}

#[derive(Serialize)]
struct FunctionDefinition {
    name: &'static str,
    description: &'static str,
    parameters: Parameters,
}

#[derive(Serialize)]
struct Parameters {
    #[serde(rename = "type")]
    kind: &'static str,
    properties: Properties,
    required: [&'static str; 1],
}

#[derive(Serialize)]
struct Properties {
    command: StringParameter,
}

#[derive(Serialize)]
struct StringParameter {
    #[serde(rename = "type")]
    kind: &'static str,
}

fn bash_definition() -> ToolDefinition {
    ToolDefinition {
        kind: "function",
        function: FunctionDefinition {
            name: "bash",
            description: "Run a bash command in the current directory.",
            parameters: Parameters {
                kind: "object",
                properties: Properties {
                    command: StringParameter { kind: "string" },
                },
                required: ["command"],
            },
        },
    }
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Default)]
struct ToolAccumulator {
    calls: BTreeMap<usize, PartialToolCall>,
}

fn merge_metadata(slot: &mut Option<String>, value: Option<String>, field: &str) -> Result<()> {
    if let Some(value) = value {
        if let Some(previous) = slot {
            ensure!(*previous == value, "Conflicting tool call {field}");
        } else {
            *slot = Some(value);
        }
    }
    Ok(())
}

impl ToolAccumulator {
    fn push(&mut self, delta: ToolCallDelta) -> Result<()> {
        let index = delta
            .index
            .context("Tool call is missing a numeric index")?;
        let call = self.calls.entry(index).or_default();
        merge_metadata(&mut call.id, delta.id, "id")?;
        merge_metadata(&mut call.kind, delta.kind, "type")?;
        if let Some(function) = delta.function {
            merge_metadata(&mut call.name, function.name, "function name")?;
            if let Some(arguments) = function.arguments {
                call.arguments.push_str(&arguments);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<(Vec<ToolCall>, Vec<BashArgs>)> {
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        let mut commands = Vec::new();
        for (index, partial) in self.calls {
            let id = partial
                .id
                .with_context(|| format!("Tool call {index} is missing id"))?;
            ensure!(!id.trim().is_empty(), "Tool call {index} has blank id");
            ensure!(ids.insert(id.clone()), "Duplicate tool call id: {id}");
            let kind = partial.kind.context("Tool call is missing type")?;
            ensure!(kind == "function", "Unsupported tool call type: {kind}");
            let name = partial.name.context("Tool call is missing function name")?;
            ensure!(name == "bash", "Unknown tool function: {name}");
            // Serde struct decoding also accepts positional arrays; the tool requires an object.
            ensure!(
                partial.arguments.trim_start().starts_with('{'),
                "Invalid bash arguments for tool call {id}: expected a JSON object"
            );
            let args: BashArgs = serde_json::from_str(&partial.arguments)
                .with_context(|| format!("Invalid bash arguments for tool call {id}"))?;
            calls.push(ToolCall {
                id,
                kind,
                function: FunctionCall {
                    name,
                    arguments: partial.arguments,
                },
            });
            commands.push(args);
        }
        Ok((calls, commands))
    }
}

pub async fn chat(
    client: &reqwest::Client,
    url: &reqwest::Url,
    api_key: &str,
    model: &str,
    history: &[ChatMessage],
    stdout: &mut impl Write,
) -> Result<CompletedReply> {
    let request = ChatRequest {
        model,
        messages: history,
        tools: [bash_definition()],
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
    };

    let response = client
        .post(url.clone())
        .bearer_auth(api_key)
        .json(&request)
        .send()
        .await
        .context("Failed to send Chat Completions request")?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .with_context(|| format!("Failed to read API error response ({status})"))?;
        anyhow::bail!("API error ({status}):\n{body}");
    }

    let mut events = response.bytes_stream().eventsource();
    let mut content = String::new();
    let mut saw_content = false;
    let mut finish_reason = None;
    let mut done = false;
    let mut tools = ToolAccumulator::default();
    while let Some(event) = events.next().await {
        let event = event.context("Failed to decode Chat Completions SSE stream")?;
        if event.data == "[DONE]" {
            done = true;
            break;
        }
        // Include the raw data so in-stream provider errors reach stderr.
        let chunk: ChatChunk = serde_json::from_str(&event.data).with_context(|| {
            format!(
                "Failed to parse Chat Completions stream chunk: {}",
                event.data
            )
        })?;
        let Some(choice) = chunk.choices.into_iter().next() else {
            ensure!(
                chunk.usage.is_some(),
                "API stream chunk contained no choices or usage"
            );
            continue;
        };
        if let Some(fragment) = choice.delta.content {
            stdout
                .write_all(fragment.as_bytes())
                .context("Failed to write assistant reply")?;
            stdout.flush().context("Failed to flush assistant reply")?;
            content.push_str(&fragment);
            saw_content = true;
        }
        for delta in choice.delta.tool_calls.unwrap_or_default() {
            tools.push(delta)?;
        }
        if finish_reason.is_none() {
            finish_reason = choice.finish_reason;
        }
    }
    ensure!(done, "Chat Completions stream ended before [DONE]");
    ensure!(
        saw_content || !tools.calls.is_empty() || finish_reason.as_deref() == Some("tool_calls"),
        "API response contained no message content (finish_reason: {})",
        finish_reason.as_deref().unwrap_or("unknown")
    );
    ensure!(
        finish_reason.is_some(),
        "Chat Completions stream contained no finish_reason"
    );
    ensure!(
        (finish_reason.as_deref() == Some("tool_calls")) == !tools.calls.is_empty(),
        "Tool calls do not match finish_reason: {}",
        finish_reason.as_deref().unwrap_or("unknown")
    );
    let (tool_calls, commands) = tools.finish()?;
    if saw_content {
        writeln!(stdout).context("Failed to write assistant reply")?;
        stdout.flush().context("Failed to flush assistant reply")?;
    }
    Ok(CompletedReply {
        message: ChatMessage {
            role: "assistant".to_owned(),
            content: saw_content.then_some(content),
            tool_calls,
            tool_call_id: None,
        },
        commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;
    use serde_json::{Value, json};

    fn assemble(fragments: Vec<Value>) -> Result<(Vec<ToolCall>, Vec<BashArgs>)> {
        let mut accumulator = ToolAccumulator::default();
        for fragment in fragments {
            accumulator.push(serde_json::from_value(fragment)?)?;
        }
        accumulator.finish()
    }

    fn call(index: usize, id: &str, args: &str) -> Value {
        json!({"index": index, "id": id, "type": "function",
            "function": {"name": "bash", "arguments": args}})
    }

    #[test]
    fn indexed_fragments_preserve_json_strings_and_numeric_order() {
        let (calls, commands) = assemble(vec![
            call(7, "b", ""),
            call(0, "a", "{\"command\":\"printf "),
            json!({"index": 7, "function": {"arguments": "{\"command\":\"echo \\\"雪\\\"\"}"}}),
            call(0, "a", "hi\"}"),
            json!({"index": 0, "function": {"arguments": ""}}),
        ])
        .unwrap();
        assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(calls[0].function.arguments, "{\"command\":\"printf hi\"}");
        assert_eq!(commands[0].command, "printf hi");
        assert_eq!(commands[1].command, "echo \"雪\"");
        assert_eq!(
            serde_json::to_value(&calls[0]).unwrap(),
            json!({
                "id": "a", "type": "function", "function": {
                    "name": "bash", "arguments": "{\"command\":\"printf hi\"}"
                }
            })
        );
    }

    #[test]
    fn ordinary_messages_omit_tool_fields_and_tool_only_content_is_null() {
        for value in [
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "tool_call_id": "a", "content": "{}"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": "a", "type": "function", "function": {
                    "name": "bash", "arguments": "{\"command\":\"ls\"}"
                }
            }]}),
        ] {
            let message: ChatMessage = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(message).unwrap(), value);
        }
    }

    #[test]
    fn invalid_metadata_or_arguments_reject_the_whole_batch() {
        let valid = call(0, "a", "{\"command\":\"true\"}");
        let second = call(1, "b", "{\"command\":\"true\"}");
        let mut cases = Vec::new();
        for field in ["index", "id", "type"] {
            let mut bad = second.clone();
            bad.as_object_mut().unwrap().remove(field);
            cases.push(bad);
        }
        for (field, value) in [
            ("index", json!(-1)),
            ("id", json!(" ")),
            ("type", json!("")),
            ("type", json!("other")),
        ] {
            let mut bad = second.clone();
            bad[field] = value;
            cases.push(bad);
        }
        for value in [Value::Null, json!(""), json!(" "), json!("unknown")] {
            let mut bad = second.clone();
            bad["function"]["name"] = value;
            cases.push(bad);
        }
        for args in [
            "",
            "{",
            "{}",
            "{\"command\":null}",
            "{\"command\":1}",
            "[]",
            "[\"true\"]",
        ] {
            cases.push(call(1, "b", args));
        }
        cases.push(call(1, "a", "{\"command\":\"true\"}"));
        for bad in cases {
            assert!(assemble(vec![valid.clone(), bad.clone()]).is_err(), "{bad}");
        }
        for conflict in [
            json!({"index": 0, "id": "different"}),
            json!({"index": 0, "type": "other"}),
            json!({"index": 0, "function": {"name": "other"}}),
        ] {
            assert!(assemble(vec![valid.clone(), conflict]).is_err());
        }
    }
}
