use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    process::{Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::oneshot,
    time::timeout,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agent-chat-loop-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[derive(Debug)]
struct Request {
    start_line: String,
    headers: Vec<(String, String)>,
    body: Value,
}

enum Reply {
    Http(u16, String),
    Disconnect,
}

fn answer(content: &str) -> Reply {
    Reply::Http(
        200,
        json!({"choices": [{"message": {"role": "assistant", "content": content},
                            "finish_reason": "stop"}]})
        .to_string(),
    )
}

async fn read_request(stream: &mut BufReader<TcpStream>) -> Request {
    let mut start_line = String::new();
    stream.read_line(&mut start_line).await.unwrap();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').unwrap();
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .unwrap()
        .1
        .parse::<usize>()
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    Request {
        start_line,
        headers,
        body: serde_json::from_slice(&body).unwrap(),
    }
}

async fn run(
    input: &[u8],
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
) -> (Output, Vec<Request>, PathBuf) {
    let cwd = TestDir::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1///", listener.local_addr().unwrap());
    let (stop, mut stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut replies: VecDeque<_> = replies.into();
        let mut requests = Vec::new();
        loop {
            let (stream, _) = tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let mut stream = BufReader::new(stream);
            requests.push(
                timeout(Duration::from_secs(5), read_request(&mut stream))
                    .await
                    .expect("client did not finish sending its request"),
            );
            match replies
                .pop_front()
                .unwrap_or(Reply::Http(500, "unexpected extra request".to_owned()))
            {
                Reply::Http(status, body) => {
                    let response = format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                }
                Reply::Disconnect => {}
            }
        }
        requests
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .current_dir(&cwd.0)
        .env("OPENAI_API_KEY", "dummy-key")
        .env("OPENAI_BASE_URL", base_url)
        .env_remove("OPENAI_MODEL")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure(&mut command);
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).await.unwrap();
    drop(stdin);
    let output = timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("agent did not exit")
        .unwrap();
    stop.send(()).unwrap();
    let requests = server.await.unwrap();
    (output, requests, cwd.0.clone())
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).unwrap()
}

#[tokio::test]
async fn two_turns_replay_exact_history_and_cwd_with_configured_wire_fields() {
    let (output, requests, cwd) = run(
        b"  hello \t\nremember me?\n/exit\nignored\n",
        vec![answer("Hello!\nHow can I help?"), answer("Yes.")],
        |command| {
            command.env("OPENAI_MODEL", "mock-model");
        },
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello!\nHow can I help?\nYes.\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    let system = json!({"role": "system", "content": format!(
        "You are a coding assistant.\nBe concise and direct.\nHelp the user with programming tasks.\nWork in the current working directory: {}\nBe clear about what you know and what you have actually done.",
        cwd.display()
    )});
    let first_user = json!({"role": "user", "content": "  hello \t"});
    assert_eq!(requests[0].body["messages"], json!([system, first_user]));
    assert_eq!(
        requests[1].body["messages"],
        json!([
            system,
            first_user,
            {"role": "assistant", "content": "Hello!\nHow can I help?"},
            {"role": "user", "content": "remember me?"}
        ])
    );
    for request in requests {
        assert_eq!(request.start_line, "POST /v1/chat/completions HTTP/1.1\r\n");
        assert!(
            request
                .headers
                .contains(&("authorization".to_owned(), "Bearer dummy-key".to_owned()))
        );
        assert_eq!(request.body["model"], "mock-model");
        assert!(request.body.get("stream").is_none());
    }
}

#[tokio::test]
async fn blank_lines_crlf_slash_text_and_final_unterminated_line() {
    let (output, requests, _) = run(
        b"\n \t\r\n/help\r\n final \t",
        vec![answer("help"), answer("final")],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "help\nfinal\n");
    assert_eq!(stderr(&output), "> > > > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["model"], "gpt-4.1");
    assert_eq!(requests[0].body["messages"][1]["content"], "/help");
    assert_eq!(requests[1].body["messages"][3]["content"], " final \t");
}

#[tokio::test]
async fn exit_and_eof_are_successful_without_requests() {
    for input in ["", "/exit\nignored\n", " \t/exit \t\r\nignored\n", "/exit"] {
        let (output, requests, _) = run(input.as_bytes(), vec![], |_| {}).await;
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), "> ");
        assert!(requests.is_empty());
    }
}

#[tokio::test]
async fn positional_arguments_are_rejected_before_prompt_or_request() {
    let (output, requests, _) = run(b"", vec![], |command| {
        command.arg("say hi");
    })
    .await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("Positional arguments are not supported"));
    assert!(!stderr(&output).contains("> "));
    assert!(requests.is_empty());
}

#[tokio::test]
async fn missing_and_blank_keys_fail_before_prompt_or_request() {
    for key in [None, Some(""), Some(" \t\n")] {
        let (output, requests, _) = run(b"", vec![], |command| {
            if let Some(key) = key {
                command.env("OPENAI_API_KEY", key);
            } else {
                command.env_remove("OPENAI_API_KEY");
            }
        })
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "");
        assert!(stderr(&output).contains(if key.is_none() {
            "OPENAI_API_KEY is required"
        } else {
            "OPENAI_API_KEY must not be empty"
        }));
        assert!(!stderr(&output).contains("> "));
        assert!(requests.is_empty());
    }
}

#[tokio::test]
async fn default_configuration_starts_without_network_and_bad_url_fails_before_prompt() {
    let (output, requests, _) = run(b"", vec![], |command| {
        command.env_remove("OPENAI_BASE_URL");
    })
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "> ");
    assert!(requests.is_empty());

    let (output, requests, _) = run(b"", vec![], |command| {
        command.env("OPENAI_BASE_URL", "invalid-url");
    })
    .await;
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Invalid OPENAI_BASE_URL"));
    assert!(!stderr(&output).contains("> "));
    assert!(requests.is_empty());
}

#[tokio::test]
async fn empty_reply_is_printed_and_retained_and_only_first_choice_is_used() {
    let reply = Reply::Http(
        200,
        json!({"choices": [
            {"message": {"role": "assistant", "content": ""}},
            {"message": {"role": "assistant", "content": "ignore this"}}
        ]})
        .to_string(),
    );
    let (output, requests, _) = run(b"first\nsecond\n", vec![reply, answer("ok")], |_| {}).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "\nok\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({"role": "assistant", "content": ""})
    );
}

#[tokio::test]
async fn second_turn_failures_preserve_first_reply_and_do_not_submit_third_turn() {
    let cases = [
        (
            Reply::Http(429, "mock rate limit body".to_owned()),
            vec!["429", "mock rate limit body"],
        ),
        (
            Reply::Http(200, "not json".to_owned()),
            vec!["Failed to parse Chat Completions response"],
        ),
        (
            Reply::Http(200, json!({"choices": []}).to_string()),
            vec!["API response contained no choices"],
        ),
        (
            Reply::Http(
                200,
                json!({"choices": [{"message": {"role": "assistant"}, "finish_reason": "length"}]})
                    .to_string(),
            ),
            vec!["no message content", "finish_reason: length"],
        ),
        (
            Reply::Http(
                200,
                json!({"choices": [{"message": {"role": "assistant", "content": null}}]})
                    .to_string(),
            ),
            vec!["no message content", "finish_reason: unknown"],
        ),
        (
            Reply::Disconnect,
            vec!["Failed to send Chat Completions request"],
        ),
    ];
    for (reply, expected_errors) in cases {
        let (output, requests, _) = run(
            b"first\nsecond\nthird\n",
            vec![answer("first reply"), reply],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "first reply\n");
        assert!(stderr(&output).starts_with("> > Error: "));
        for expected in expected_errors {
            assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        }
        assert!(!stderr(&output).contains("dummy-key"));
        assert_eq!(requests.len(), 2);
    }
}

#[tokio::test]
async fn invalid_utf8_input_exits_with_context_without_request() {
    let (output, requests, _) = run(b"\xff\n", vec![], |_| {}).await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("Failed to read stdin"));
    assert!(requests.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn stdout_failure_exits_before_submitting_next_turn() {
    let (output, requests, _) = run(b"first\nsecond\n", vec![answer("reply")], |command| {
        command.stdout(fs::File::options().write(true).open("/dev/full").unwrap());
    })
    .await;
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Failed to write assistant reply"));
    assert_eq!(requests.len(), 1);
}
