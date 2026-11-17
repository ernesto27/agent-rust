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
    run_inspected(input, replies, configure, handshake, |_| {}).await
}

async fn run_inspected(
    input: &[u8],
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
    handshake: Option<(&[u8], oneshot::Sender<()>)>,
    inspect: impl FnOnce(&std::path::Path),
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
                        // A fail-fast client may close as soon as it sees an invalid event.
                        if let Err(error) = stream.write_all(b"0\r\n\r\n").await {
                            assert!(
                                matches!(
                                    error.kind(),
                                    std::io::ErrorKind::BrokenPipe
                                        | std::io::ErrorKind::ConnectionReset
                                ),
                                "{error}"
                            );
                        }
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
    inspect(&cwd.0);
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
        assert_eq!(request.body["tools"], bash_schema());
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
async fn explicit_null_tool_calls_are_treated_as_absent() {
    let reply = Reply::Http(
        200,
        format!(
            "{}{}{}data: [DONE]\n\n",
            event(
                json!({"choices": [{"delta": {"role": "assistant", "content": "Hi", "tool_calls": null}}]})
            ),
            event(json!({"choices": [{"delta": {"content": " there", "tool_calls": null}}]})),
            event(json!({"choices": [{"delta": {"tool_calls": null}, "finish_reason": "stop"}]})),
        ),
    );
    let (output, requests, _) = run(b"first\nsecond\n", vec![reply, answer("ok")], |_| {}).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hi there\nok\n");
    assert_eq!(stderr(&output), "> > > ");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({"role": "assistant", "content": "Hi there"})
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
        // An in-stream provider error after HTTP 200 surfaces its message.
        (
            Reply::Http(
                200,
                format!(
                    "{}{}",
                    text_event("partial"),
                    event(json!({"error": {
                        "message": "The server had an error while processing your request.",
                        "type": "server_error"
                    }}))
                ),
            ),
            "The server had an error while processing your request.",
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

fn bash_schema() -> Value {
    json!([{"type":"function","function":{"name":"bash",
        "description":"Run a bash command in the current directory.",
        "parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}])
}

fn tool_delta(index: usize, id: Option<&str>, name: Option<&str>, arguments: &str) -> String {
    let mut delta = json!({"index": index, "function": {"arguments": arguments}});
    if let Some(id) = id {
        delta["id"] = json!(id);
        delta["type"] = json!("function");
    }
    if let Some(name) = name {
        delta["function"]["name"] = json!(name);
    }
    tool_event(delta)
}

fn tool_event(delta: Value) -> String {
    event(json!({"choices":[{"delta":{"tool_calls":[delta]}}]}))
}

fn tool_finish() -> String {
    event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}))
}

fn tool_completion(events: &[String]) -> Reply {
    Reply::Http(
        200,
        format!("{}{}data: [DONE]\n\n", events.concat(), tool_finish()),
    )
}

fn bash_call(index: usize, id: &str, command: &str) -> String {
    tool_delta(
        index,
        Some(id),
        Some("bash"),
        &json!({"command":command}).to_string(),
    )
}

fn tool_result(request: &Request, index: usize, id: &str) -> Value {
    let message = &request.body["messages"][index];
    assert_eq!(message["role"], "tool");
    assert_eq!(message["tool_call_id"], id);
    assert_eq!(message.as_object().unwrap().len(), 3);
    serde_json::from_str(message["content"].as_str().unwrap()).unwrap()
}

fn assert_tools(requests: &[Request]) {
    for request in requests {
        assert_eq!(request.body["tools"], bash_schema());
        assert_eq!(request.body["stream"], true);
        assert_eq!(
            request.body["stream_options"],
            json!({"include_usage":true})
        );
    }
}

#[tokio::test]
async fn bash_round_replays_exact_history() {
    let (output, requests, _) = run(
        b"run it\nnext turn\n/exit\n",
        vec![
            tool_completion(&[tool_delta(
                0,
                Some("a"),
                Some("bash"),
                "{ \"command\": \"printf hi\" }",
            )]),
            answer("It says hi."),
            answer("Next."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "It says hi.\nNext.\n");
    assert_eq!(stderr(&output), "> $ printf hi\nexit code: 0\nhi\n> > ");
    assert_eq!(requests.len(), 3);
    let mut expected = requests[0].body["messages"].as_array().unwrap().clone();
    assert_eq!(expected.len(), 2);
    expected.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"bash","arguments":"{ \"command\": \"printf hi\" }"}}]}));
    expected.push(
        json!({"role":"tool","tool_call_id":"a","content":"{\"output\":\"hi\",\"exit_code\":0}"}),
    );
    assert_eq!(requests[1].body["messages"], json!(expected));
    expected.push(json!({"role":"assistant","content":"It says hi."}));
    expected.push(json!({"role":"user","content":"next turn"}));
    assert_eq!(requests[2].body["messages"], json!(expected));
    assert_tools(&requests);
}

#[tokio::test]
async fn multiple_calls_and_rounds_execute_in_numeric_order() {
    let (output, requests, _) = run(
        b"work\n/exit\n",
        vec![
            tool_completion(&[
                bash_call(9, "read", "cat marker; printf second >> marker"),
                bash_call(2, "write", "printf first > marker"),
            ]),
            tool_completion(&[bash_call(0, "again", "cat marker")]),
            answer("Done."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Done.\n");
    assert_eq!(requests.len(), 3);
    let history = &requests[1].body["messages"];
    assert_eq!(history.as_array().unwrap().len(), 5);
    assert_eq!(history[2]["tool_calls"][0]["id"], "write");
    assert_eq!(history[2]["tool_calls"][1]["id"], "read");
    assert_eq!(
        tool_result(&requests[1], 3, "write"),
        json!({"output":"","exit_code":0})
    );
    assert_eq!(
        tool_result(&requests[1], 4, "read"),
        json!({"output":"first","exit_code":0})
    );
    let history = requests[2].body["messages"].as_array().unwrap();
    assert_eq!(
        &history[..5],
        requests[1].body["messages"].as_array().unwrap()
    );
    assert_eq!(history.len(), 7);
    assert_eq!(history[5]["tool_calls"][0]["id"], "again");
    assert_eq!(
        tool_result(&requests[2], 6, "again"),
        json!({"output":"firstsecond","exit_code":0})
    );
    assert!(stderr(&output).starts_with("> $ printf first > marker\n"));
    assert!(!stderr(&output).contains("\n> $"));
    assert!(stderr(&output).ends_with("firstsecond\n> "));
    assert_tools(&requests);
}

#[tokio::test]
async fn mixed_text_and_tool_only_responses() {
    let first = format!(
        "{}{}",
        text_event("Checking 🌍"),
        tool_delta(0, Some("a"), Some("bash"), "{\"command\":\"printf ")
    );
    let tail = format!(
        "{}{}{}{}{}data: [DONE]\n\n",
        tool_delta(0, None, None, "'\\\"🌍\\\"'"),
        text_event(" now"),
        tool_delta(0, None, None, "\"}"),
        tool_finish(),
        event(json!({"choices":[],"usage":{"total_tokens":42}}))
    );
    let bytes = tail.into_bytes();
    let unicode = bytes.windows(4).position(|w| w == "🌍".as_bytes()).unwrap();
    let (proceed, after_first) = oneshot::channel();
    let (output, requests, _) = run_with_handshake(
        b"check\n/exit\n",
        vec![
            Reply::Stream {
                parts: vec![
                    first.into_bytes(),
                    bytes[..unicode + 1].to_vec(),
                    bytes[unicode + 1..].to_vec(),
                ],
                after_first: Some(after_first),
                truncated: false,
            },
            tool_completion(&[bash_call(0, "b", "true")]),
            answer("Found it."),
        ],
        |_| {},
        Some(("Checking 🌍".as_bytes(), proceed)),
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Checking 🌍 now\nFound it.\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].body["messages"][2]["content"],
        "Checking 🌍 now"
    );
    assert_eq!(
        requests[1].body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"command\":\"printf '\\\"🌍\\\"'\"}"
    );
    assert_eq!(
        tool_result(&requests[1], 3, "a"),
        json!({"output":"\"🌍\"","exit_code":0})
    );
    assert_eq!(requests[2].body["messages"][4]["content"], Value::Null);
    assert_tools(&requests);
}

#[tokio::test]
async fn invalid_batch_never_executes() {
    let first = bash_call(0, "first", "touch marker");
    let valid = json!({"index":1,"id":"second","type":"function","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}});
    let mut invalid = Vec::new();
    for args in ["{", "{}", "{\"command\":false}", "[\"true\"]"] {
        let mut bad = valid.clone();
        bad["function"]["arguments"] = json!(args);
        invalid.push(bad);
    }
    for field in ["id", "type", "index", "function"] {
        let mut bad = valid.clone();
        bad.as_object_mut().unwrap().remove(field);
        invalid.push(bad);
    }
    for (field, value) in [
        ("id", json!("first")),
        ("id", json!(" \t")),
        ("type", json!("other")),
        ("index", json!(-1)),
    ] {
        let mut bad = valid.clone();
        bad[field] = value;
        invalid.push(bad);
    }
    for name in [Value::Null, json!(""), json!("read")] {
        let mut bad = valid.clone();
        bad["function"]["name"] = name;
        invalid.push(bad);
    }
    let mut bodies: Vec<String> = invalid
        .into_iter()
        .map(|bad| {
            format!(
                "{first}{}{}data: [DONE]\n\n",
                tool_event(bad),
                tool_finish()
            )
        })
        .collect();
    for fragment in [
        json!({"index":0,"id":"conflict"}),
        json!({"index":0,"type":"conflict"}),
        json!({"index":0,"function":{"name":"conflict"}}),
    ] {
        bodies.push(format!(
            "{first}{}{}data: [DONE]\n\n",
            tool_event(fragment),
            tool_finish()
        ));
    }
    bodies.extend([
        format!("{first}{}data: [DONE]\n\n", finish_event()),
        format!("{first}data: [DONE]\n\n"),
        format!("{first}{}", tool_finish()),
        format!("{first}{}data: bad json\n\ndata: [DONE]\n\n", tool_finish()),
        format!("{first}{}data: [DONE]\n", tool_finish()),
        format!("{}{}data: [DONE]\n\n", text_event(""), tool_finish()),
    ]);
    for body in bodies {
        let (output, requests, _) = run_inspected(
            b"first\nqueued\n",
            vec![Reply::Http(200, body.clone())],
            |_| {},
            None,
            |cwd| {
                assert!(
                    !cwd.join("marker").exists(),
                    "executed an invalid batch: {body}"
                );
            },
        )
        .await;
        assert!(!output.status.success(), "accepted: {body}");
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).starts_with("> Error: "),
            "{}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("$ touch marker"));
        assert!(!stderr(&output).contains("dummy-key"));
        assert_eq!(requests.len(), 1);
    }
}

#[tokio::test]
async fn nonzero_exit_returns_to_model_with_merged_output() {
    let (output, requests, _) = run(
        b"work\n/exit\n",
        vec![
            tool_completion(&[bash_call(0, "a", "printf out; printf err >&2; exit 7")]),
            answer("Command exited 7."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Command exited 7.\n");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_result(&requests[1], 3, "a"),
        json!({"output":"outerr","exit_code":7})
    );
    assert!(stderr(&output).contains("exit code: 7\nouterr\n"));
}

#[tokio::test]
async fn tool_stdin_does_not_consume_next_turn() {
    let (output, requests, _) = run(
        b"first\nqueued second\n/exit\n",
        vec![
            tool_completion(&[bash_call(0, "a", "cat")]),
            answer("EOF."),
            answer("Second."),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "EOF.\nSecond.\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        tool_result(&requests[1], 3, "a"),
        json!({"output":"","exit_code":0})
    );
    assert_eq!(
        requests[2].body["messages"][5],
        json!({"role":"user","content":"queued second"})
    );
}

#[tokio::test]
async fn preview_is_bounded_but_result_is_complete() {
    for (command, full, preview) in [
        (
            "for ((i=0;i<12;i++)); do printf '🌍\\n'; done",
            "🌍\n".repeat(12),
            "🌍\n".repeat(10),
        ),
        (
            "for ((i=0;i<12;i++)); do printf '🌍%.0s' {1..300}; printf '\\n'; done",
            format!("{}\n", "🌍".repeat(300)).repeat(12),
            format!(
                "{}\n{}",
                "🌍".repeat(300),
                format!("{}\n", "🌍".repeat(300)).repeat(5)
            ) + &"🌍".repeat(194),
        ),
    ] {
        let (output, requests, _) = run(
            b"show\n/exit\n",
            vec![
                tool_completion(&[bash_call(0, "a", command)]),
                answer("Complete."),
            ],
            |_| {},
        )
        .await;
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), "Complete.\n");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            tool_result(&requests[1], 3, "a"),
            json!({"output":full,"exit_code":0})
        );
        let diagnostics = stderr(&output).split_once("exit code: 0\n").unwrap().1;
        let shown = diagnostics
            .split_once("[output omitted from preview]\n")
            .unwrap()
            .0;
        assert_eq!(
            shown,
            format!(
                "{preview}{}",
                if preview.ends_with('\n') { "" } else { "\n" }
            )
        );
        assert!(preview.chars().count() <= 2000);
        assert!(preview.lines().count() <= 10);
    }
}

#[tokio::test]
async fn failure_after_a_tool_round_stops_queued_input() {
    for failure in [
        Reply::Http(503, "mock unavailable".to_owned()),
        Reply::Http(200, format!("{}data: malformed\n\n", text_event("partial"))),
    ] {
        let (output, requests, _) = run(
            b"first\nqueued\n",
            vec![
                tool_completion(&[text_event("Checking"), bash_call(0, "a", "true")]),
                failure,
            ],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert!(stdout(&output) == "Checking\n" || stdout(&output) == "Checking\npartial");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            tool_result(&requests[1], 3, "a"),
            json!({"output":"","exit_code":0})
        );
        assert!(!stderr(&output).contains("\n> "));
    }
}
