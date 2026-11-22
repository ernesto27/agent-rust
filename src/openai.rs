use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
};

use anyhow::{Context, Result, ensure};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::types::{ChatMessage, FunctionCall, ToolCall, parse_bash_args};

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    stream_options: StreamOptions,
    tools: Vec<ToolDef>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ToolDef {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDef,
}

#[derive(Serialize)]
struct FunctionDef {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

fn bash_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        kind: "function",
        function: FunctionDef {
            name: "bash",
            description: "Run a bash command in the current directory.",
            parameters: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        },
    }]
}

#[derive(Deserialize)]
struct ChatChunk {
    choices: Vec<ChatChoice>,
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolDelta>,
}

#[derive(Deserialize)]
struct ToolDelta {
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
struct PendingCall {
    id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn merge_field(target: &mut Option<String>, value: Option<String>) -> Result<()> {
    if let Some(value) = value {
        if let Some(existing) = target.as_ref() {
            ensure!(existing == &value, "Conflicting tool-call metadata");
        } else {
            *target = Some(value);
        }
    }
    Ok(())
}

fn accumulate(calls: &mut BTreeMap<usize, PendingCall>, delta: ToolDelta) -> Result<()> {
    let call = calls.entry(delta.index).or_default();
    merge_field(&mut call.id, delta.id)?;
    merge_field(&mut call.kind, delta.kind)?;
    if let Some(function) = delta.function {
        merge_field(&mut call.name, function.name)?;
        if let Some(fragment) = function.arguments {
            call.arguments.push_str(&fragment);
        }
    }
    Ok(())
}

fn finish_calls(calls: BTreeMap<usize, PendingCall>) -> Result<Vec<ToolCall>> {
    let mut ids = HashSet::new();
    let mut complete = Vec::new();
    for (expected, (index, call)) in calls.into_iter().enumerate() {
        ensure!(index == expected, "Noncontiguous tool-call indices");
        let id = call.id.context("Tool call missing id")?;
        ensure!(!id.trim().is_empty(), "Tool call has blank id");
        ensure!(ids.insert(id.clone()), "Duplicate tool-call id");
        let kind = call.kind.context("Tool call missing type")?;
        ensure!(kind == "function", "Unsupported tool-call type: {kind}");
        let name = call.name.context("Tool call missing function name")?;
        ensure!(name == "bash", "Unknown tool: {name}");
        parse_bash_args(&call.arguments).with_context(|| format!("Invalid tool call {id}"))?;
        complete.push(ToolCall {
            id,
            kind,
            function: FunctionCall {
                name,
                arguments: call.arguments,
            },
        });
    }
    Ok(complete)
}

pub async fn chat(
    client: &reqwest::Client,
    url: &reqwest::Url,
    api_key: &str,
    model: &str,
    history: &[ChatMessage],
    stdout: &mut impl Write,
) -> Result<ChatMessage> {
    let request = ChatRequest {
        model,
        messages: history,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        tools: bash_tools(),
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
    let mut calls = BTreeMap::new();
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
        for delta in choice.delta.tool_calls {
            accumulate(&mut calls, delta)?;
        }
        if finish_reason.is_none() {
            finish_reason = choice.finish_reason;
        }
    }
    ensure!(done, "Chat Completions stream ended before [DONE]");
    let tool_calls = finish_calls(calls)?;
    if tool_calls.is_empty() && !saw_content {
        anyhow::bail!(
            "API response contained no message content (finish_reason: {})",
            finish_reason.as_deref().unwrap_or("unknown")
        );
    }
    let finish_reason =
        finish_reason.context("Chat Completions stream contained no finish_reason")?;
    if tool_calls.is_empty() {
        ensure!(
            finish_reason != "tool_calls",
            "Tool finish reason without tool calls"
        );
        ensure!(
            saw_content,
            "API response contained no message content (finish_reason: {finish_reason})"
        );
    } else {
        ensure!(
            finish_reason == "tool_calls",
            "Tool calls without tool_calls finish reason"
        );
    }
    if saw_content {
        writeln!(stdout).context("Failed to write assistant reply")?;
        stdout.flush().context("Failed to flush assistant reply")?;
    }
    Ok(ChatMessage {
        role: "assistant".to_owned(),
        content: saw_content.then_some(content),
        tool_calls,
        tool_call_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(calls: &mut BTreeMap<usize, PendingCall>, value: Value) {
        accumulate(calls, serde_json::from_value(value).unwrap()).unwrap();
    }

    fn complete(index: usize, id: &str) -> Value {
        json!({"index":index,"id":id,"type":"function",
            "function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}})
    }

    #[test]
    fn interleaved_fragments_are_sorted_and_arguments_preserved() {
        let mut calls = BTreeMap::new();
        add(&mut calls, complete(1, "b"));
        add(
            &mut calls,
            json!({"index":0,"id":"a","type":"function",
            "function":{"name":"bash","arguments":"{\"command\":"}}),
        );
        add(
            &mut calls,
            json!({"index":0,"function":{"arguments":"\"pwd\"}"}}),
        );
        let calls = finish_calls(calls).unwrap();
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].function.arguments, r#"{"command":"pwd"}"#);
        assert_eq!(calls[1].id, "b");
    }

    #[test]
    fn invalid_batches_and_conflicting_metadata_fail() {
        for invalid in [
            json!({"index":0}),
            complete(1, "gap"),
            complete(0, " "),
            json!({"index":0,"id":"a","function":{
                "name":"bash","arguments":"{\"command\":\"ls\"}"}}),
            json!({"index":0,"id":"a","type":"function","function":{
                "arguments":"{\"command\":\"ls\"}"}}),
            json!({"index":0,"id":"a","type":"other",
                "function":{"name":"bash","arguments":"{}"}}),
            json!({"index":0,"id":"a","type":"function",
                "function":{"name":"read","arguments":"{}"}}),
            json!({"index":0,"id":"a","type":"function",
                "function":{"name":"bash","arguments":"{"}}),
        ] {
            let mut calls = BTreeMap::new();
            add(&mut calls, invalid);
            assert!(finish_calls(calls).is_err());
        }
        let mut calls = BTreeMap::new();
        add(&mut calls, complete(0, "a"));
        add(&mut calls, complete(1, "a"));
        assert!(finish_calls(calls).is_err());
        for delta in [
            json!({"index":0,"id":"changed"}),
            json!({"index":0,"type":"changed"}),
            json!({"index":0,"function":{"name":"changed"}}),
        ] {
            let mut calls = BTreeMap::new();
            add(&mut calls, complete(0, "a"));
            assert!(accumulate(&mut calls, serde_json::from_value(delta).unwrap()).is_err());
        }
        let mut calls = BTreeMap::new();
        add(&mut calls, complete(0, "a"));
        add(
            &mut calls,
            json!({"index":0,"id":"a","type":"function",
            "function":{"name":"bash"}}),
        );
        assert_eq!(finish_calls(calls).unwrap().len(), 1);
    }
}
