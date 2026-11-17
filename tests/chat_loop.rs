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
    Stream {
        parts: Vec<Vec<u8>>,
        after_first: Option<oneshot::Receiver<()>>,
        truncated: bool,
    },
}

fn event(chunk: Value) -> String {
    format!("data: {chunk}\n\n")
}

fn text_event(content: &str) -> String {
    event(json!({"choices": [{"delta": {"content": content}}]}))
}

fn finish_event() -> String {
    event(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}))
}

fn completion(fragments: &[&str]) -> Reply {
    let mut body: String = fragments
        .iter()
        .map(|fragment| text_event(fragment))
        .collect();
    body.push_str(&finish_event());
    body.push_str("data: [DONE]\n\n");
    Reply::Http(200, body)
}

fn answer(content: &str) -> Reply {
    completion(&[content])
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
    run_with_handshake(input, replies, configure, None).await
}

async fn run_with_handshake(
    input: &[u8],
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
    handshake: Option<(&[u8], oneshot::Sender<()>)>,
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
                        "HTTP/1.1 {status} Mock\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                }
                Reply::Disconnect => {}
                Reply::Stream {
                    parts,
                    mut after_first,
                    truncated,
                } => {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    for (index, part) in parts.iter().enumerate() {
                        stream
                            .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                            .await
                            .unwrap();
                        stream.write_all(part).await.unwrap();
                        stream.write_all(b"\r\n").await.unwrap();
                        stream.flush().await.unwrap();
                        if index == 0
                            && let Some(proceed) = after_first.take()
                        {
                            timeout(Duration::from_secs(5), proceed)
                                .await
                                .expect("stdout fragment was not flushed before remainder")
                                .expect("stdout observer dropped the handshake");
                        }
                    }
                    if !truncated {
                        stream.write_all(b"0\r\n\r\n").await.unwrap();
                    }
                }
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
    let mut observed = Vec::new();
    if let Some((fragment, proceed)) = handshake {
        let mut stdout = child.stdout.take().unwrap();
        observed.resize(fragment.len(), 0);
        timeout(Duration::from_secs(5), stdout.read_exact(&mut observed))
            .await
            .expect("agent buffered the first fragment while mock withheld the remainder")
            .unwrap();
        assert_eq!(observed, fragment);
        proceed.send(()).unwrap();
        child.stdout = Some(stdout);
    }
    let mut output = timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("agent did not exit")
        .unwrap();
    observed.extend_from_slice(&output.stdout);
    output.stdout = observed;
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
        vec![
            completion(&["Hello!", "\nHow can ", "I help?"]),
            answer("Yes."),
        ],
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
        assert_eq!(request.body["stream"], true);
        assert_eq!(
            request.body["stream_options"],
            json!({"include_usage": true})
        );
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
        format!(
            "{}data: [DONE]\n\n",
            event(json!({"choices": [
                {"delta": {"role": "assistant", "content": ""}, "finish_reason": "stop"},
                {"delta": {"content": "ignore this"}, "finish_reason": "length"}
            ]}))
        ),
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
            Reply::Http(200, "data: not json\n\n".to_owned()),
            vec!["Failed to parse Chat Completions stream chunk"],
        ),
        (
            Reply::Http(200, event(json!({"choices": []}))),
            vec!["API stream chunk contained no choices or usage"],
        ),
        (
            Reply::Http(200, event(json!({"choices": [], "usage": null}))),
            vec!["API stream chunk contained no choices or usage"],
        ),
        (
            Reply::Http(
                200,
                format!(
                    "{}data: [DONE]\n\n",
                    event(json!({"choices": [
                        {"delta": {"role": "assistant"}, "finish_reason": "length"}
                    ]}))
                ),
            ),
            vec!["no message content", "finish_reason: length"],
        ),
        (
            Reply::Http(
                200,
                format!(
                    "{}data: [DONE]\n\n",
                    event(json!({"choices": [
                        {"delta": {"content": null}}
                    ]}))
                ),
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
    for (reply, error) in [
        ("reply".to_owned(), "Failed to flush assistant reply"),
        ("x".repeat(65536), "Failed to write assistant reply"),
        (String::new(), "Failed to write assistant reply"),
    ] {
        let (output, requests, _) = run(b"first\nsecond\n", vec![answer(&reply)], |command| {
            command.stdout(fs::File::options().write(true).open("/dev/full").unwrap());
        })
        .await;
        assert!(!output.status.success());
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn fragments_are_flushed_before_remainder_and_sse_boundaries_preserve_exact_history() {
    let prefix = " \tHello";
    let (proceed, after_first) = oneshot::channel();
    let first = format!(
        ": comment\n\n{}{}{}{}{}",
        event(json!({"choices": [{"delta": {"role": "assistant"}}]})),
        event(json!({"choices": [{"delta": {"content": null}}]})),
        event(json!({"choices": [{"delta": {}}]})),
        // No event name is required; unrelated fields are ignored.
        event(json!({"id": "ignored", "choices": [{"delta": {"refusal": null}}]})),
        text_event(prefix)
    )
    .replace('\n', "\r\n");
    let unicode_event = text_event(" 🌍").replacen("\"choices\":", "\"choices\":\ndata: ", 1);
    let tail = format!(
        ": keepalive\r\n\r\n{}{}{}{}data: [DONE]\r\n\r\n",
        unicode_event,
        text_event("\nend "),
        finish_event(),
        event(json!({"choices": [], "usage": {"total_tokens": 42}}))
    );
    let bytes = tail.into_bytes();
    let unicode = bytes
        .windows(4)
        .position(|window| window == "🌍".as_bytes())
        .unwrap();
    let cr = bytes.iter().position(|byte| *byte == b'\r').unwrap();
    // Separate HTTP chunks and network writes inside JSON, a UTF-8 character,
    // and CRLF. Later chunks also contain multiple complete events.
    let parts = vec![
        first.into_bytes(),
        bytes[..cr + 1].to_vec(),
        bytes[cr + 1..unicode - 1].to_vec(),
        bytes[unicode - 1..unicode + 1].to_vec(),
        bytes[unicode + 1..unicode + 3].to_vec(),
        bytes[unicode + 3..].to_vec(),
    ];
    let (output, requests, _) = run_with_handshake(
        b"first\nsecond\n/exit\n",
        vec![
            Reply::Stream {
                parts,
                after_first: Some(after_first),
                truncated: false,
            },
            answer("ok"),
        ],
        |_| {},
        Some((prefix.as_bytes(), proceed)),
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), " \tHello 🌍\nend \nok\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({"role": "assistant", "content": " \tHello 🌍\nend "})
    );
}

#[tokio::test]
async fn malformed_chunk_shapes_are_rejected_before_next_turn() {
    for chunk in [
        json!({}),
        json!({"choices": null}),
        json!({"choices": {}}),
        json!({"choices": [{}]}),
        json!({"choices": [{"delta": null}]}),
        json!({"choices": [{"delta": {"content": 7}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": false}]}),
    ] {
        let (output, requests, _) = run(
            b"first\nsecond\n",
            vec![Reply::Http(200, event(chunk))],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).contains("Failed to parse Chat Completions stream chunk"),
            "{}",
            stderr(&output)
        );
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn partial_second_turn_failures_keep_flushed_text_without_newline_or_third_request() {
    let cases = [
        (
            Reply::Http(200, format!("{}data: bad json\n\n", text_event("partial"))),
            "Failed to parse Chat Completions stream chunk",
        ),
        (
            Reply::Http(200, format!("{}data: [DONE]\n\n", text_event("partial"))),
            "contained no finish_reason",
        ),
        (
            Reply::Http(200, format!("{}{}", text_event("partial"), finish_event())),
            "ended before [DONE]",
        ),
        (
            Reply::Http(200, text_event("partial")),
            "ended before [DONE]",
        ),
        // An unterminated event is never a completion marker.
        (
            Reply::Http(
                200,
                format!("{}{}data: [DONE]\n", text_event("partial"), finish_event()),
            ),
            "ended before [DONE]",
        ),
        (
            Reply::Stream {
                parts: vec![text_event("partial").into_bytes()],
                after_first: None,
                truncated: true,
            },
            "Failed to decode Chat Completions SSE stream",
        ),
        (
            Reply::Stream {
                parts: vec![
                    text_event("partial").into_bytes(),
                    b"data: \xff\n\n".to_vec(),
                ],
                after_first: None,
                truncated: false,
            },
            "Failed to decode Chat Completions SSE stream",
        ),
    ];
    for (reply, error) in cases {
        let (output, requests, _) = run(
            b"first\nsecond\nthird\n",
            vec![answer("complete"), reply],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), "complete\npartial");
        assert!(
            stderr(&output).starts_with("> > Error: "),
            "{}",
            stderr(&output)
        );
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
        assert_eq!(requests.len(), 2);
    }
}

#[tokio::test]
async fn second_choice_cannot_supply_missing_first_choice_content_or_finish_reason() {
    for (choices, expected_output, error) in [
        (
            json!([
                {"delta": {"content": "first"}},
                {"delta": {"content": "ignored"}, "finish_reason": "stop"}
            ]),
            "first",
            "contained no finish_reason",
        ),
        (
            json!([
                {"delta": {}, "finish_reason": "length"},
                {"delta": {"content": "ignored"}, "finish_reason": "stop"}
            ]),
            "",
            "no message content (finish_reason: length)",
        ),
    ] {
        let body = format!("{}data: [DONE]\n\n", event(json!({"choices": choices})));
        let (output, requests, _) =
            run(b"first\nnext\n", vec![Reply::Http(200, body)], |_| {}).await;
        assert!(!output.status.success());
        assert_eq!(stdout(&output), expected_output);
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn first_non_null_finish_reason_is_retained_until_done() {
    let body = format!(
        "{}{}{}data: [DONE]\n\n",
        event(json!({"choices": [{"delta": {}, "finish_reason": "length"}]})),
        finish_event(),
        event(json!({"choices": [], "usage": {}}))
    );
    let (output, requests, _) = run(b"first\nnext\n", vec![Reply::Http(200, body)], |_| {}).await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("no message content (finish_reason: length)"),
        "{}",
        stderr(&output)
    );
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn failure_after_observed_second_turn_fragment_never_submits_queued_third_turn() {
    let (proceed, after_first) = oneshot::channel();
    let (output, requests, _) = run_with_handshake(
        b"first\nsecond\nthird\n",
        vec![
            answer("complete"),
            Reply::Stream {
                parts: vec![
                    text_event("partial").into_bytes(),
                    b"data: malformed\n\n".to_vec(),
                ],
                after_first: Some(after_first),
                truncated: false,
            },
        ],
        |_| {},
        Some((b"complete\npartial", proceed)),
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(stdout(&output), "complete\npartial");
    assert!(stderr(&output).starts_with("> > Error: "));
    assert!(stderr(&output).contains("Failed to parse Chat Completions stream chunk"));
    assert_eq!(requests.len(), 2);
}
