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
pub(super) struct Request {
    pub(super) start_line: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Value,
}

pub(super) enum Reply {
    Http(u16, String),
    Disconnect,
    Stream {
        parts: Vec<Vec<u8>>,
        after_first: Option<oneshot::Receiver<()>>,
        truncated: bool,
    },
}

pub(super) fn event(chunk: Value) -> String {
    format!("data: {chunk}\n\n")
}

pub(super) fn text_event(content: &str) -> String {
    event(json!({"choices": [{"delta": {"content": content}}]}))
}

pub(super) fn finish_event() -> String {
    event(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}))
}

pub(super) fn completion(fragments: &[&str]) -> Reply {
    let mut body: String = fragments
        .iter()
        .map(|fragment| text_event(fragment))
        .collect();
    body.push_str(&finish_event());
    body.push_str("data: [DONE]\n\n");
    Reply::Http(200, body)
}

pub(super) fn answer(content: &str) -> Reply {
    completion(&[content])
}

pub(super) fn tool_delta(
    index: usize,
    id: Option<&str>,
    name: Option<&str>,
    arguments: &str,
) -> String {
    let mut delta = json!({"index": index, "function": {"arguments": arguments}});
    if let Some(id) = id {
        delta["id"] = json!(id);
        delta["type"] = json!("function");
    }
    if let Some(name) = name {
        delta["function"]["name"] = json!(name);
    }
    event(json!({"choices": [{"delta": {"tool_calls": [delta]}}]}))
}

pub(super) fn tool_finish() -> String {
    event(json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}))
}

pub(super) fn tool_completion(events: &[String]) -> Reply {
    Reply::Http(
        200,
        format!("{}{}data: [DONE]\n\n", events.concat(), tool_finish()),
    )
}

pub(super) fn bash_call(index: usize, id: &str, command: &str) -> String {
    tool_delta(
        index,
        Some(id),
        Some("bash"),
        &json!({"command": command}).to_string(),
    )
}

pub(super) fn tool_result(request: &Request, message: usize) -> Value {
    serde_json::from_str(
        request.body["messages"][message]["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
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

pub(super) async fn run(
    input: &[u8],
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
) -> (Output, Vec<Request>, PathBuf) {
    run_with_handshake(input, replies, configure, None).await
}

pub(super) async fn run_with_handshake(
    input: &[u8],
    replies: Vec<Reply>,
    configure: impl FnOnce(&mut Command),
    handshake: Option<(&[u8], oneshot::Sender<()>)>,
) -> (Output, Vec<Request>, PathBuf) {
    run_inspected(input, replies, configure, handshake, |_| {}).await
}

pub(super) async fn run_inspected(
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

pub(super) fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

pub(super) fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).unwrap()
}
