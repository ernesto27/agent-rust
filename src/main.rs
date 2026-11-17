use std::env;

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
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
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
    let api_key = env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY is required; set it in your environment before running agent")?;
    ensure!(
        !api_key.trim().is_empty(),
        "OPENAI_API_KEY must not be empty; set it before running agent"
    );

    let base_url = env_or_default("OPENAI_BASE_URL", DEFAULT_BASE_URL)?;
    let model = env_or_default("OPENAI_MODEL", DEFAULT_MODEL)?;

    let prompt = env::args().skip(1).collect::<Vec<_>>().join(" ");
    ensure!(
        !prompt.trim().is_empty(),
        "Provide a question: cargo run -- \"say hi\""
    );

    let request = ChatRequest {
        model,
        messages: vec![ChatMessage {
            role: "user".to_owned(),
            content: Some(prompt),
        }],
    };

    let response = reqwest::Client::new()
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
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
        .first()
        .context("API response contained no choices")?;
    let content = choice.message.content.as_deref().with_context(|| {
        format!(
            "API response contained no message content (finish_reason: {})",
            choice.finish_reason.as_deref().unwrap_or("unknown")
        )
    })?;

    println!("{content}");
    Ok(())
}

fn env_or_default(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error).with_context(|| format!("{name} must contain valid Unicode")),
    }
}
