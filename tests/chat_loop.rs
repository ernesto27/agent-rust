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
    run_full(input, None, replies, configure, handshake).await
}

/// Like `run`, but keeps stdin open and writes `deferred` only after the mock
/// has received the first request, so the REPL cannot have buffered it.
async fn run_with_deferred_input(
    input: &[u8],
    deferred: &[u8],
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
) -> (Output, Vec<Request>, PathBuf) {
    run_full(input, Some(deferred), replies, configure, None).await
}

async fn run_full(
    input: &[u8],
    deferred: Option<&[u8]>,
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
    handshake: Option<(&[u8], oneshot::Sender<()>)>,
) -> (Output, Vec<Request>, PathBuf) {
    let cwd = TestDir::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1///", listener.local_addr().unwrap());
    let (stop, mut stopped) = oneshot::channel();
    let (first_request, first_received) = oneshot::channel();
    let mut first_request = Some(first_request);
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
            if let Some(notify) = first_request.take() {
                let _ = notify.send(());
            }
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
                    let last = parts.len().saturating_sub(1);
                    for (index, part) in parts.iter().enumerate() {
                        stream
                            .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                            .await
                            .unwrap();
                        stream.write_all(part).await.unwrap();
                        stream.flush().await.unwrap();
                        // The client may legitimately close once the final
                        // payload ends the stream ([DONE] or a fatal chunk),
                        // so framing after it is best-effort.
                        let framing = stream.write_all(b"\r\n").await;
                        if index < last {
                            framing.unwrap();
                            stream.flush().await.unwrap();
                        }
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
                        let _ = stream.write_all(b"0\r\n\r\n").await;
                        let _ = stream.flush().await;
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
    if let Some(deferred) = deferred {
        stdin.flush().await.unwrap();
        timeout(Duration::from_secs(5), first_received)
            .await
            .expect("agent did not send its first request")
            .unwrap();
        // The REPL is now inside the agent loop, so these bytes stay in the
        // pipe where only a child inheriting stdin could consume them.
        stdin.write_all(deferred).await.unwrap();
    }
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
async fn null_tool_calls_in_text_deltas_are_accepted() {
    let body = format!(
        "{}{}data: [DONE]\n\n",
        event(json!({"choices": [{"delta": {"content": "plain", "tool_calls": null}}]})),
        event(json!({"choices": [{"delta": {"tool_calls": null}, "finish_reason": "stop"}]}))
    );
    let (output, requests, _) = run(
        b"first\nsecond\n",
        vec![Reply::Http(200, body), answer("ok")],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "plain\nok\n");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({"role": "assistant", "content": "plain"})
    );
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

fn bash_call(index: usize, id: &str, command: &str) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{
        "name":"bash","arguments":json!({"command":command}).to_string()
    }})
}

fn read_call(index: usize, id: &str, arguments: &str) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{
        "name":"read","arguments":arguments
    }})
}

fn write_call(index: usize, id: &str, arguments: &str) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{
        "name":"write","arguments":arguments
    }})
}

#[tokio::test]
async fn write_creates_parents_and_overwrites_with_exact_content() {
    let fixture = TestDir::new();
    let path = "nested/hello.txt";
    let first = json!({"path":path,"content":"hi\r\n"}).to_string();
    let second = json!({"path":path,"content":"bye 🦀\n"}).to_string();
    let (output, requests, _) = run(
        b"write a file\n/exit\n",
        vec![
            tool_reply(vec![write_call(0, "create", &first)]),
            tool_reply(vec![write_call(0, "overwrite", &second)]),
            answer("done"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert_eq!(
        fs::read(fixture.0.join(path)).unwrap(),
        "bye 🦀\n".as_bytes()
    );
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].body["messages"][3],
        json!({"role":"tool", "tool_call_id":"create", "content":"Wrote 4 bytes to \"nested/hello.txt\""})
    );
    assert_eq!(
        requests[2].body["messages"][5],
        json!({"role":"tool", "tool_call_id":"overwrite", "content":"Wrote 9 bytes to \"nested/hello.txt\""})
    );
    assert_eq!(
        stderr(&output),
        "> write \"nested/hello.txt\"\n@@ -0,0 +1 @@\n+hi\\r\nwrite \"nested/hello.txt\"\n@@ -1 +1 @@\n-hi\\r\n+bye 🦀\n> "
    );
}

#[tokio::test]
async fn write_bounds_the_preview_without_truncating_the_file() {
    let fixture = TestDir::new();
    let boundary = "\0".repeat(64 * 1024);
    let over = format!("{boundary}x");
    let first = json!({"path":"boundary","content":boundary}).to_string();
    let second = json!({"path":"over","content":over}).to_string();
    let (output, requests, _) = run(
        b"write two files\n/exit\n",
        vec![
            tool_reply(vec![
                write_call(0, "boundary", &first),
                write_call(1, "over", &second),
            ]),
            answer("done"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert_eq!(
        fs::read(fixture.0.join("boundary")).unwrap(),
        boundary.as_bytes()
    );
    assert_eq!(fs::read(fixture.0.join("over")).unwrap(), over.as_bytes());
    assert_eq!(
        stderr(&output),
        format!(
            "> write \"boundary\"\n@@ -0,0 +1 @@\n+{}\n[diff preview truncated]\nwrite \"over\"\n[diff omitted: file exceeds 64 KiB]\n> ",
            "\\u{0}".repeat(4096 - "@@ -0,0 +1 @@\n+".len())
        )
    );
    assert_eq!(requests.len(), 2);
    for (index, (id, content)) in [
        ("boundary", "Wrote 65536 bytes to \"boundary\""),
        ("over", "Wrote 65537 bytes to \"over\""),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            requests[1].body["messages"][3 + index],
            json!({"role":"tool","tool_call_id":id,"content":content})
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn write_rejections_leave_existing_files_and_the_batch_intact() {
    let fixture = TestDir::new();
    fs::write(fixture.0.join("target"), b"keep\n").unwrap();
    std::os::unix::fs::symlink("target", fixture.0.join("link")).unwrap();
    let (output, requests, _) = run(
        b"try writing\n/exit\n",
        vec![
            tool_reply(vec![
                write_call(0, "bad", r#"{"path":"new/file"}"#),
                write_call(1, "link", r#"{"path":"link","content":"no"}"#),
                write_call(2, "dir", r#"{"path":".","content":"no"}"#),
                write_call(3, "empty", r#"{"path":"empty","content":""}"#),
            ]),
            answer("handled"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "handled\n");
    assert_eq!(
        stderr(&output),
        "> write: invalid arguments\nwrite \"link\"\nwrite \".\"\nwrite \"empty\"\n> "
    );
    assert_eq!(requests.len(), 2);
    for (index, (id, content)) in [
        (
            "bad",
            "Error: write arguments: missing field `content` at line 1 column 19",
        ),
        (
            "link",
            "Error: write \"link\": destination must be a regular file",
        ),
        (
            "dir",
            "Error: write \".\": destination must be a regular file",
        ),
        ("empty", "Wrote 0 bytes to \"empty\""),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            requests[1].body["messages"][3 + index],
            json!({"role":"tool","tool_call_id":id,"content":content})
        );
    }
    assert_eq!(fs::read(fixture.0.join("target")).unwrap(), b"keep\n");
    assert_eq!(
        fs::read_link(fixture.0.join("link")).unwrap(),
        std::path::Path::new("target")
    );
    assert!(!fixture.0.join("new").exists());
    assert_eq!(fs::read(fixture.0.join("empty")).unwrap(), b"");
}

fn edit_call(index: usize, id: &str, path: &str, old_text: &str, new_text: &str) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{
        "name":"edit","arguments":json!({"path":path,"old_text":old_text,"new_text":new_text}).to_string()
    }})
}

#[tokio::test]
async fn edit_replaces_one_match_and_keeps_diff_out_of_tool_result() {
    let fixture = TestDir::new();
    let (output, requests, _) = run(
        b"create hello.py that prints hi, then change it to print bye\n/exit\n",
        vec![
            tool_reply(vec![write_call(
                0,
                "create",
                r#"{"path":"hello.py","content":"print('hi')\n"}"#,
            )]),
            tool_reply(vec![edit_call(
                0,
                "edit",
                "hello.py",
                "print('hi')",
                "print('bye')",
            )]),
            answer("done"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert_eq!(
        stderr(&output),
        "> write \"hello.py\"\n@@ -0,0 +1 @@\n+print('hi')\nedit \"hello.py\"\n@@ -1 +1 @@\n-print('hi')\n+print('bye')\n> "
    );
    assert_eq!(
        fs::read(fixture.0.join("hello.py")).unwrap(),
        b"print('bye')\n"
    );
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].body["messages"][5],
        json!({
            "role":"tool", "tool_call_id":"edit", "content":"Edited \"hello.py\""
        })
    );
}

#[tokio::test]
async fn edit_rejects_missing_and_ambiguous_text_without_changing_files() {
    let fixture = TestDir::new();
    fs::write(fixture.0.join("sample"), "aaa\n").unwrap();
    let (output, requests, _) = run(
        b"try edits\n/exit\n",
        vec![
            tool_reply(vec![
                edit_call(0, "absent", "sample", "A", "x"),
                edit_call(1, "multiple", "sample", "aa", "x"),
                edit_call(2, "empty", "sample", "", "x"),
            ]),
            answer("handled"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "handled\n");
    assert_eq!(
        stderr(&output),
        "> edit \"sample\"\nedit \"sample\"\nedit: invalid arguments\n> "
    );
    assert_eq!(fs::read(fixture.0.join("sample")).unwrap(), b"aaa\n");
    assert_eq!(requests.len(), 2);
    for (index, (id, content)) in [
        (
            "absent",
            "Error: edit \"sample\": not found, re-read the file",
        ),
        (
            "multiple",
            "Error: edit \"sample\": found 2 times, add more surrounding context",
        ),
        ("empty", "Error: edit arguments: old_text must not be empty"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            requests[1].body["messages"][3 + index],
            json!({"role":"tool","tool_call_id":id,"content":content})
        );
    }
}

#[tokio::test]
async fn edit_preserves_crlf_bom_and_missing_final_newline() {
    let fixture = TestDir::new();
    fs::write(fixture.0.join("sample"), b"\xef\xbb\xbfhi\r\nold\r\ntail").unwrap();
    fs::write(fixture.0.join("explicit"), "\u{feff}hi\r\ntail\r\n").unwrap();
    let (output, requests, _) = run(
        b"edit the first two lines\n/exit\n",
        vec![
            tool_reply(vec![
                edit_call(0, "edit", "sample", "hi\nold", "bye\nnew"),
                edit_call(1, "explicit", "explicit", "\u{feff}hi\n", ""),
            ]),
            answer("done"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert_eq!(
        stderr(&output),
        concat!(
            "> edit \"sample\"\n@@ -1,3 +1,3 @@\n-\u{feff}hi\\r\n-old\\r\n+\u{feff}bye\\r\n+new\\r\n tail\n\\ No newline at end of file\n",
            "edit \"explicit\"\n@@ -1,2 +1 @@\n-\u{feff}hi\\r\n tail\\r\n> "
        )
    );
    assert_eq!(
        fs::read(fixture.0.join("sample")).unwrap(),
        b"\xef\xbb\xbfbye\r\nnew\r\ntail"
    );
    assert_eq!(fs::read(fixture.0.join("explicit")).unwrap(), b"tail\r\n");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][4],
        json!({
            "role":"tool", "tool_call_id":"explicit", "content":"Edited \"explicit\""
        })
    );
    assert_eq!(
        requests[1].body["messages"][3],
        json!({
            "role":"tool", "tool_call_id":"edit", "content":"Edited \"sample\""
        })
    );
}

fn tool_reply(calls: Vec<Value>) -> Reply {
    let body = format!(
        "{}{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"tool_calls":calls}}]})),
        event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}))
    );
    Reply::Http(200, body)
}

#[tokio::test]
async fn bash_results_nonzero_exits_and_full_history_reach_model() {
    let command = "printf out; printf err >&2; exit 7";
    let (output, requests, _) = run(
        b"do it\nnext\n/exit\n",
        vec![
            tool_reply(vec![bash_call(0, "c1", command)]),
            answer("handled"),
            answer("remembered"),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "handled\nremembered\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].body["messages"][2],
        json!({
            "role":"assistant","content":null,"tool_calls":[{
                "id":"c1","type":"function","function":{
                    "name":"bash","arguments":json!({"command":command}).to_string()
                }
            }]
        })
    );
    let result = json!({"role":"tool","tool_call_id":"c1",
        "content":"Exit code: 7\nouterr"});
    assert_eq!(requests[1].body["messages"][3], result);
    assert_eq!(requests[2].body["messages"][3], result);
    assert!(stderr(&output).contains(&format!("$ {command}\n")));
    for request in &requests {
        assert_eq!(
            request.body["tools"],
            json!([{
                "type":"function", "function":{
                    "name":"bash",
                    "description":"Run a bash command in the current directory.",
                    "parameters":{
                        "type":"object", "properties":{"command":{"type":"string"}},
                        "required":["command"], "additionalProperties":false
                    }
                }
            }, {
                "type":"function", "function":{
                    "name":"read",
                    "description":"Read a UTF-8 text file by line number. Prefer this over cat for reading files.",
                    "parameters":{
                        "type":"object",
                        "properties":{
                            "path":{"type":"string","minLength":1},
                            "offset":{"type":"integer","minimum":1,"default":1},
                            "limit":{"type":"integer","minimum":1,"maximum":2000,"default":2000}
                        },
                        "required":["path"], "additionalProperties":false
                    }
                }
            }, {
                "type":"function", "function":{
                    "name":"write",
                    "description":"Create or overwrite a file with the supplied text. Creates missing parent directories.",
                    "parameters":{
                        "type":"object",
                        "properties":{"path":{"type":"string","minLength":1},"content":{"type":"string"}},
                        "required":["path","content"], "additionalProperties":false
                    }
                }
            }, {
                "type":"function", "function":{
                    "name":"edit",
                    "description":"Replace exactly one occurrence of old_text in an existing UTF-8 file. Use surrounding context for a unique match.",
                    "parameters":{
                        "type":"object",
                        "properties":{"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}},
                        "required":["path","old_text","new_text"], "additionalProperties":false
                    }
                }
            }])
        );
        assert_eq!(request.body["stream"], true);
    }
}

#[tokio::test]
async fn sequential_calls_and_rounds_share_files_but_not_shell_state() {
    let (output, requests, cwd) = run(b"work\n/exit\n", vec![
        tool_reply(vec![
            bash_call(0, "a", "mkdir child; printf first > marker; cd child; export STEP5_LOCAL=changed"),
            bash_call(1, "b", "printf '%s\\n' \"$PWD\"; printf '%s\\n' \"${STEP5_LOCAL-unset}\"; cat marker; printf '%s' \"$STEP5_INHERITED\""),
        ]),
        tool_reply(vec![bash_call(0, "c", "cat marker")]),
        answer("done"),
    ], |command| {
        command.env("STEP5_INHERITED", "inherited").env_remove("STEP5_LOCAL");
    }).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].body["messages"][4]["content"],
        format!("Exit code: 0\n{}\nunset\nfirstinherited", cwd.display())
    );
    assert_eq!(
        requests[2].body["messages"][6]["content"],
        "Exit code: 0\nfirst"
    );
    assert_eq!(requests[1].body["messages"][3]["tool_call_id"], "a");
    assert_eq!(requests[1].body["messages"][4]["tool_call_id"], "b");
    assert_eq!(requests[2].body["messages"][6]["tool_call_id"], "c");
}

#[tokio::test]
async fn invalid_batch_stops_before_any_command_or_next_input() {
    for invalid in [
        json!({"index":1,"id":"bad","type":"function",
            "function":{"name":"bash","arguments":"{"}}),
        json!({"index":1,"id":"bad","type":"function",
            "function":{"name":"unknown","arguments":"{\"command\":\"ls\"}"}}),
        json!({"index":1,"id":"bad","type":"other",
            "function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}),
    ] {
        let (output, requests, _) = run(
            b"work\nqueued\n",
            vec![tool_reply(vec![
                bash_call(0, "good", "printf MUST_NOT_RUN"),
                invalid,
            ])],
            |_| {},
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(requests.len(), 1);
        assert!(!stderr(&output).contains("$ printf MUST_NOT_RUN"));
        assert_eq!(stdout(&output), "");
    }
}

#[tokio::test]
async fn read_pages_and_bash_results_reach_the_model_in_order() {
    let fixture = TestDir::new();
    let work = fixture.0.join("work");
    fs::create_dir(&work).unwrap();
    fs::write(fixture.0.join("sample.txt"), "one\r\ntwo\nthree").unwrap();
    let absolute = json!({"path":fixture.0.join("sample.txt"),"offset":2}).to_string();
    let (output, requests, _) = run(
        b"read the sample\n/exit\n",
        vec![
            tool_reply(vec![
                bash_call(0, "bash", "printf ready"),
                read_call(1, "first", r#"{"path":"../sample.txt","limit":1}"#),
            ]),
            tool_reply(vec![read_call(0, "second", &absolute)]),
            answer("The file contains one, two, three."),
        ],
        |command| {
            command.current_dir(&work);
        },
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "The file contains one, two, three.\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].body["messages"][2]["tool_calls"][1]["function"]["name"],
        "read"
    );
    assert_eq!(
        requests[1].body["messages"][3],
        json!({
            "role":"tool","tool_call_id":"bash","content":"Exit code: 0\nready"
        })
    );
    assert_eq!(
        requests[1].body["messages"][4],
        json!({
            "role":"tool","tool_call_id":"first",
            "content":"     1\tone\n[showing lines 1-1 of 3, use offset=2 to read more]\n"
        })
    );
    assert_eq!(
        requests[2].body["messages"][6],
        json!({
            "role":"tool","tool_call_id":"second","content":"     2\ttwo\n     3\tthree\n"
        })
    );
    assert!(stderr(&output).contains("read \"../sample.txt\""));
}

#[cfg(unix)]
#[tokio::test]
async fn read_failures_return_tool_results_and_retry_can_follow_a_symlink() {
    let fixture = TestDir::new();
    fs::write(fixture.0.join("data.bin"), b"text\0bytes").unwrap();
    fs::write(fixture.0.join("ok.txt"), "recovered\n").unwrap();
    std::os::unix::fs::symlink("ok.txt", fixture.0.join("link.txt")).unwrap();
    let (output, requests, _) = run(
        b"read a file\n/exit\n",
        vec![
            tool_reply(vec![
                read_call(0, "args", "{"),
                read_call(1, "missing", r#"{"path":"missing"}"#),
                read_call(2, "dir", r#"{"path":"."}"#),
                read_call(3, "binary", r#"{"path":"data.bin"}"#),
            ]),
            tool_reply(vec![read_call(0, "retry", r#"{"path":"link.txt"}"#)]),
            answer("recovered"),
        ],
        |command| {
            command.current_dir(&fixture.0);
        },
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "recovered\n");
    assert_eq!(requests.len(), 3);
    for (index, (id, reason)) in [
        ("args", "read arguments"),
        ("missing", "file not found"),
        ("dir", "path is a directory"),
        ("binary", "binary file"),
    ]
    .into_iter()
    .enumerate()
    {
        let result = &requests[1].body["messages"][index + 3];
        assert_eq!(result["role"], "tool");
        assert_eq!(result["tool_call_id"], id);
        let content = result["content"].as_str().unwrap();
        assert!(content.starts_with("Error: read"), "{content}");
        assert!(content.contains(reason), "{content}");
    }
    assert_eq!(
        requests[2].body["messages"][8],
        json!({
            "role":"tool","tool_call_id":"retry","content":"     1\trecovered\n"
        })
    );
}

#[tokio::test]
async fn missing_bash_is_a_fatal_launch_error() {
    let (output, requests, _) = run(
        b"work\nqueued\n",
        vec![tool_reply(vec![bash_call(0, "a", "printf hello")])],
        |command| {
            command.env("PATH", "/nonexistent-step5-path");
        },
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(requests.len(), 1);
    assert!(stderr(&output).contains("Failed to start bash"));
}

#[tokio::test]
async fn command_stdin_is_eof_and_large_output_is_only_preview_truncated() {
    let command = "if read -r line; then printf stolen; else printf eof; fi; printf '%0100000d' 0";
    let (output, requests, _) = run_with_deferred_input(
        b"work\n",
        b"queued\n/exit\n",
        vec![
            tool_reply(vec![bash_call(0, "a", command)]),
            answer("done"),
            answer("next"),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\nnext\n");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].body["messages"][5]["content"], "queued");
    let result = requests[1].body["messages"][3]["content"].as_str().unwrap();
    assert_eq!(result.len(), "Exit code: 0\neof".len() + 100_000);
    assert!(result.starts_with("Exit code: 0\neof"));
    assert!(stderr(&output).contains("[preview truncated; full output sent to model]"));
    assert!(stderr(&output).len() < 5000);
}

#[tokio::test]
async fn terminal_controls_reach_model_raw_but_are_escaped_on_stderr() {
    let command = r"printf 'a\033]52;c;aGk=\007b\r\033[2Kc\n'";
    let (output, requests, _) = run(
        b"work\n/exit\n",
        vec![tool_reply(vec![bash_call(0, "a", command)]), answer("done")],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        requests[1].body["messages"][3]["content"],
        "Exit code: 0\na\x1b]52;c;aGk=\x07b\r\x1b[2Kc\n"
    );
    let stderr = stderr(&output);
    assert!(!stderr.contains(['\x1b', '\x07', '\r']));
    assert!(stderr.contains("a\\u{1b}]52;c;aGk=\\u{7}b\\r\\u{1b}[2Kc\n"));
    assert!(stderr.contains(&format!("$ {command}\n")));
}

#[tokio::test]
async fn fragmented_calls_allow_text_and_execute_only_after_complete_stream() {
    let fragments = [
        text_event("Working"),
        event(json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"a","type":"function","function":{
                "name":"bash","arguments":"{\"command\":"
            }
        }]}}]})),
        event(json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"function":{"arguments":"\"printf hello\"}"}
        }]}}]})),
        event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})),
        event(json!({"choices":[],"usage":{"prompt_tokens":1}})),
        "data: [DONE]\n\n".to_owned(),
    ];
    let (proceed, receiver) = oneshot::channel();
    let body = fragments[1..].concat().into_bytes();
    let (output, requests, _) = run_with_handshake(
        b"work\n/exit\n",
        vec![
            Reply::Stream {
                parts: vec![fragments[0].clone().into_bytes(), body],
                after_first: Some(receiver),
                truncated: false,
            },
            answer("done"),
        ],
        |_| {},
        Some((b"Working", proceed)),
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Working\ndone\n");
    assert_eq!(
        requests[1].body["messages"][2],
        json!({
            "role":"assistant", "content":"Working", "tool_calls":[{
                "id":"a", "type":"function", "function":{
                    "name":"bash", "arguments":"{\"command\":\"printf hello\"}"
                }
            }]
        })
    );
    assert_eq!(
        requests[1].body["messages"][3]["content"],
        "Exit code: 0\nhello"
    );

    // A syntactically complete call without [DONE] must never execute.
    let body = event(json!({"choices":[{"delta":{"tool_calls":[
        bash_call(0, "a", "printf MUST_NOT_RUN")
    ]}}]}));
    let (output, requests, _) = run(b"work\nqueued\n", vec![Reply::Http(200, body)], |_| {}).await;
    assert!(!output.status.success());
    assert_eq!(requests.len(), 1);
    assert!(!stderr(&output).contains("$ printf MUST_NOT_RUN"));
}

#[tokio::test]
async fn tools_require_matching_finish_reason_and_done() {
    for (calls, reason) in [
        (vec![], Some("tool_calls")),
        (vec![bash_call(0, "a", "printf MUST_NOT_RUN")], Some("stop")),
        (vec![bash_call(0, "a", "printf MUST_NOT_RUN")], None),
    ] {
        let mut body = event(json!({"choices":[{"delta":{"tool_calls":calls}}]}));
        if let Some(reason) = reason {
            body.push_str(&event(
                json!({"choices":[{"delta":{},"finish_reason":reason}]}),
            ));
        }
        body.push_str("data: [DONE]\n\n");
        let (output, requests, _) =
            run(b"work\nqueued\n", vec![Reply::Http(200, body)], |_| {}).await;
        assert!(!output.status.success());
        assert_eq!(requests.len(), 1);
        assert!(!stderr(&output).contains("$ printf MUST_NOT_RUN"));
    }
}

#[tokio::test]
async fn empty_commands_signal_exits_and_lossy_output_are_tool_results() {
    let (output, requests, _) = run(
        b"work\n/exit\n",
        vec![
            tool_reply(vec![
                bash_call(0, "empty", ""),
                bash_call(1, "bytes", "printf '\\377out'; printf err >&2; printf tail"),
                bash_call(2, "signal", "printf interrupted; kill -TERM $$"),
            ]),
            answer("done"),
        ],
        |_| {},
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].body["messages"];
    assert_eq!(messages[3]["content"], "Exit code: 0\n");
    assert_eq!(messages[4]["content"], "Exit code: 0\n�outerrtail");
    assert_eq!(messages[5]["content"], "Termination: signal\ninterrupted");
    assert!(stderr(&output).contains("Termination: signal\ninterrupted\n"));
}
