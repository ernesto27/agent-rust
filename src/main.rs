use std::{
    env,
    io::{self, BufRead, Write},
};

use anyhow::{Context, Result, ensure};
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
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(rename = "usage")]
    _usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    finish_reason: Option<String>,
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
        let message = chat(&client, &url, &api_key, &model, &history).await?;
        // chat validates content before the message can enter history.
        writeln!(stdout, "{}", message.content.as_deref().unwrap())
            .context("Failed to write assistant reply")?;
        stdout.flush().context("Failed to flush assistant reply")?;
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
) -> Result<ChatMessage> {
    let request = ChatRequest {
        model,
        messages: history,
    };

    let response = client
        .post(url.clone())
        .bearer_auth(api_key)
        .json(&request)
        .send()
        .await
        .context("Failed to send Chat Completions request")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read API response")?;
    ensure!(status.is_success(), "API error ({status}):\n{body}");

    let response: ChatResponse =
        serde_json::from_str(&body).context("Failed to parse Chat Completions response")?;
    let choice = response
        .choices
        .into_iter()
        .next()
        .context("API response contained no choices")?;
    choice.message.content.as_deref().with_context(|| {
        format!(
            "API response contained no message content (finish_reason: {})",
            choice.finish_reason.as_deref().unwrap_or("unknown")
        )
    })?;

    Ok(choice.message)
}

fn env_or_default(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error).with_context(|| format!("{name} must contain valid Unicode")),
    }
}
