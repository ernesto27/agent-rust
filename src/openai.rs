use crate::types::{BashArgs, ChatMessage, CompletedReply, FunctionCall, ToolCall};
use anyhow::{Context, Result, ensure};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
};

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    stream_options: StreamOptions,
    tools: [ToolDefinition; 1],
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
    // Some compatible servers send an explicit `null` instead of omitting it.
    tool_calls: Option<Vec<ToolCallDelta>>,
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
        stream: true,
        tools: [ToolDefinition::bash()],
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
    let mut tools = ToolAccumulator::default();
    let mut content = String::new();
    let mut saw_content = false;
    let mut finish_reason = None;
    let mut done = false;
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
        for delta in choice.delta.tool_calls.into_iter().flatten() {
            tools.push(delta)?;
        }
        if finish_reason.is_none() {
            finish_reason = choice.finish_reason;
        }
    }
    ensure!(done, "Chat Completions stream ended before [DONE]");
    let (tool_calls, commands) = tools.finish()?;
    ensure!(
        saw_content || !tool_calls.is_empty(),
        "API response contained no message content (finish_reason: {})",
        finish_reason.as_deref().unwrap_or("unknown")
    );
    ensure!(
        finish_reason.is_some(),
        "Chat Completions stream contained no finish_reason"
    );
    ensure!(
        (finish_reason.as_deref() == Some("tool_calls")) == !tool_calls.is_empty(),
        "Tool calls and finish_reason do not match"
    );
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

#[derive(Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: BashDefinition,
}

#[derive(Serialize)]
struct BashDefinition {
    name: &'static str,
    description: &'static str,
    parameters: BashParameters,
}

#[derive(Serialize)]
struct BashParameters {
    #[serde(rename = "type")]
    kind: &'static str,
    properties: BashProperties,
    required: [&'static str; 1],
}

#[derive(Serialize)]
struct BashProperties {
    command: StringParameter,
}

#[derive(Serialize)]
struct StringParameter {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl ToolDefinition {
    fn bash() -> Self {
        Self {
            kind: "function",
            function: BashDefinition {
                name: "bash",
                description: "Run a bash command in the current directory.",
                parameters: BashParameters {
                    kind: "object",
                    properties: BashProperties {
                        command: StringParameter { kind: "string" },
                    },
                    required: ["command"],
                },
            },
        }
    }
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: usize,
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
struct ToolAccumulator(BTreeMap<usize, PartialToolCall>);

impl ToolAccumulator {
    fn push(&mut self, delta: ToolCallDelta) -> Result<()> {
        let call = self.0.entry(delta.index).or_default();
        merge_metadata(&mut call.id, delta.id, "id")?;
        merge_metadata(&mut call.kind, delta.kind, "type")?;
        if let Some(function) = delta.function {
            merge_metadata(&mut call.name, function.name, "function name")?;
            if let Some(fragment) = function.arguments {
                call.arguments.push_str(&fragment);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<(Vec<ToolCall>, Vec<BashArgs>)> {
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        let mut commands = Vec::new();
        for (index, call) in self.0 {
            let id = call
                .id
                .with_context(|| format!("Tool call {index} missing id"))?;
            ensure!(ids.insert(id.clone()), "Duplicate tool call id: {id}");
            let kind = call
                .kind
                .with_context(|| format!("Tool call {index} missing type"))?;
            ensure!(kind == "function", "Unsupported tool call type: {kind}");
            let name = call
                .name
                .with_context(|| format!("Tool call {index} missing function name"))?;
            ensure!(name == "bash", "Unsupported tool name: {name}");
            // Serde structs also accept positional arrays; the tool requires an object.
            ensure!(
                call.arguments.trim_start().starts_with('{'),
                "Invalid bash arguments for tool call {id}: expected a JSON object"
            );
            let args: BashArgs = serde_json::from_str(&call.arguments)
                .with_context(|| format!("Invalid bash arguments for tool call {id}"))?;
            calls.push(ToolCall {
                id,
                kind,
                function: FunctionCall {
                    name,
                    arguments: call.arguments,
                },
            });
            commands.push(args);
        }
        Ok((calls, commands))
    }
}

fn merge_metadata(target: &mut Option<String>, value: Option<String>, field: &str) -> Result<()> {
    if let Some(value) = value {
        ensure!(!value.trim().is_empty(), "Tool call has blank {field}");
        if let Some(previous) = target {
            ensure!(*previous == value, "Conflicting tool call {field}");
        } else {
            *target = Some(value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn assemble(
        fragments: Vec<Value>,
    ) -> Result<(Vec<crate::types::ToolCall>, Vec<crate::types::BashArgs>)> {
        let mut acc = ToolAccumulator::default();
        for fragment in fragments {
            acc.push(serde_json::from_value(fragment)?)?;
        }
        acc.finish()
    }

    fn call(index: usize, id: &str, arguments: &str) -> Value {
        json!({"index":index,"id":id,"type":"function","function":{"name":"bash","arguments":arguments}})
    }

    #[test]
    fn indexed_fragments_preserve_json_and_numeric_order() {
        let (calls, commands) = assemble(vec![
            call(9, "b", "{\"command\":\"printf "),
            call(2, "a", "{\"command\":"),
            json!({"index":9,"function":{"arguments":"\\\"🌍\\\"\"}"}}),
            json!({"index":2,"id":"a","type":"function","function":{"name":"bash","arguments":"\"printf hi\"}"}}),
            json!({"index":2,"function":{"arguments":""}}),
        ]).unwrap();
        assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(calls[0].function.arguments, "{\"command\":\"printf hi\"}");
        assert_eq!(commands[0].command, "printf hi");
        assert_eq!(commands[1].command, "printf \"🌍\"");
        assert_eq!(serde_json::to_value(&calls[0]).unwrap()["type"], "function");
    }

    #[test]
    fn null_continuation_metadata_is_treated_as_absent() {
        let (calls, commands) = assemble(vec![
            call(0, "a", "{\"command\":"),
            json!({"index":0,"id":null,"type":null,"function":{"name":null,"arguments":"\"true\"}"}}),
            json!({"index":0,"id":null,"type":null,"function":null}),
            json!({"index":0,"function":{"arguments":null}}),
        ])
        .unwrap();
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].function.arguments, "{\"command\":\"true\"}");
        assert_eq!(commands[0].command, "true");
        for field in ["id", "type"] {
            let mut null = call(0, "a", "{\"command\":\"true\"}");
            null[field] = json!(null);
            assert!(assemble(vec![null]).is_err(), "accepted null {field}");
        }
        let mut null_name = call(0, "a", "{\"command\":\"true\"}");
        null_name["function"]["name"] = json!(null);
        assert!(assemble(vec![null_name]).is_err());
    }

    #[test]
    fn incomplete_invalid_and_conflicting_batches_are_rejected() {
        let valid = call(0, "a", "{\"command\":\"true\"}");
        let mut invalid = Vec::new();
        for field in ["id", "type"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            invalid.push(vec![missing]);
            let mut blank = valid.clone();
            blank[field] = json!(" \t");
            invalid.push(vec![blank]);
        }
        for function in [
            json!({"arguments":"{}"}),
            json!({"name":"","arguments":"{}"}),
            json!({"name":"other","arguments":"{}"}),
        ] {
            let mut bad = valid.clone();
            bad["function"] = function;
            invalid.push(vec![bad]);
        }
        for args in [
            "",
            "{",
            "{}",
            "{\"command\":null}",
            "{\"command\":1}",
            "[\"true\"]",
        ] {
            invalid.push(vec![call(0, "a", args)]);
        }
        let mut unsupported = valid.clone();
        unsupported["type"] = json!("other");
        invalid.push(vec![unsupported]);
        invalid.push(vec![valid.clone(), call(1, "a", "{\"command\":\"true\"}")]);
        for fragment in [
            json!({"index":0,"id":"different"}),
            json!({"index":0,"type":"other"}),
            json!({"index":0,"function":{"name":"other"}}),
        ] {
            invalid.push(vec![valid.clone(), fragment]);
        }
        for index in [json!(-1), json!(null), json!("0")] {
            let mut bad = valid.clone();
            bad["index"] = index;
            invalid.push(vec![bad]);
        }
        let mut missing_index = valid.clone();
        missing_index.as_object_mut().unwrap().remove("index");
        invalid.push(vec![missing_index]);
        for batch in invalid {
            assert!(assemble(batch.clone()).is_err(), "accepted {batch:?}");
        }
    }
}
