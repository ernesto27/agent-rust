use std::{
    env,
    io::{self, BufRead, Write},
};

use anyhow::{Context, Result, ensure};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4.1";

#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: Option<String>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
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
}

#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        env::args_os().nth(1).is_none(),
        "Positional arguments are not supported; run cargo run and enter messages on stdin"
    );

    let api_key = env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY is required; set it in your environment before running agent")?;
    ensure!(
        !api_key.trim().is_empty(),
        "OPENAI_API_KEY must not be empty; set it before running agent"
    );

    let base_url = env_or_default("OPENAI_BASE_URL", DEFAULT_BASE_URL)?;
    let model = env_or_default("OPENAI_MODEL", DEFAULT_MODEL)?;

    let cwd = env::current_dir().context("Failed to get current working directory")?;
    let url = reqwest::Url::parse(&format!(
        "{}/chat/completions",
        base_url.trim_end_matches('/')
    ))
    .context("Invalid OPENAI_BASE_URL")?;
    let client = reqwest::Client::builder()
        .build()
        .context("Failed to create HTTP client")?;
    let mut history: Vec<ChatMessage> = vec![ChatMessage {
        role: "system".to_owned(),
        content: Some(format!(
            "You are a coding assistant.\n\
             Be concise and direct.\n\
             Help the user with programming tasks.\n\
             Work in the current working directory: {}\n\
             Be clear about what you know and what you have actually done.",
            cwd.display()
        )),
    }];
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let mut line = String::new();

    loop {
        write!(stderr, "> ").context("Failed to write input prompt")?;
        stderr.flush().context("Failed to flush input prompt")?;
        line.clear();
        if stdin.read_line(&mut line).context("Failed to read stdin")? == 0 {
            break;
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if line.trim() == "/exit" {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }

        history.push(ChatMessage {
            role: "user".to_owned(),
            content: Some(line.clone()),
        });
        let message = chat(&client, &url, &api_key, &model, &history, &mut stdout).await?;
        // Only completed streams with successfully flushed output enter history.
        history.push(message);
    }

    Ok(())
}

async fn chat(
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
        if finish_reason.is_none() {
            finish_reason = choice.finish_reason;
        }
    }
    ensure!(done, "Chat Completions stream ended before [DONE]");
    ensure!(
        saw_content,
        "API response contained no message content (finish_reason: {})",
        finish_reason.as_deref().unwrap_or("unknown")
    );
    ensure!(
        finish_reason.is_some(),
        "Chat Completions stream contained no finish_reason"
    );
    writeln!(stdout).context("Failed to write assistant reply")?;
    stdout.flush().context("Failed to flush assistant reply")?;
    Ok(ChatMessage {
        role: "assistant".to_owned(),
        content: Some(content),
    })
}

fn env_or_default(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error).with_context(|| format!("{name} must contain valid Unicode")),
    }
}
