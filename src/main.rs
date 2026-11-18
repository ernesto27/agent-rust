mod bash;
mod openai;
mod types;

use std::{
    env,
    io::{self, BufRead, Write},
};

use anyhow::{Context, Result, ensure};
use types::ChatMessage;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4.1";

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
        tool_calls: Vec::new(),
        tool_call_id: None,
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
            tool_calls: Vec::new(),
            tool_call_id: None,
            role: "user".to_owned(),
            content: Some(line.clone()),
        });
        loop {
            let reply =
                openai::chat(&client, &url, &api_key, &model, &history, &mut stdout).await?;
            let call_ids: Vec<_> = reply
                .message
                .tool_calls
                .iter()
                .map(|call| call.id.clone())
                .collect();
            // chat validates the entire batch before returning any executable commands.
            history.push(reply.message);
            if call_ids.is_empty() {
                break;
            }
            for (id, args) in call_ids.into_iter().zip(reply.commands) {
                let content = bash::run(&args.command, &cwd, &mut stderr).await?;
                history.push(ChatMessage {
                    role: "tool".to_owned(),
                    content: Some(content),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(id),
                });
            }
        }
    }

    Ok(())
}

fn env_or_default(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error).with_context(|| format!("{name} must contain valid Unicode")),
    }
}
