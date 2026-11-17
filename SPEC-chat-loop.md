# Spec: Chat Loop with Step 4 Streaming — V1

Status: approved

## Objective

Replace the one-question CLI with a minimal line-based REPL so later turns include
earlier messages. This implements [PLAN.md Step 3](PLAN.md#step-3-chat-loop-repl) and
[Step 4](PLAN.md#step-4-streaming); later steps remain deferred. Success means
launching `cargo run`, seeing text as it arrives, exchanging multiple turns, and
quitting with `/exit` or end-of-input.

## Previous behavior

`src/main.rs` joined command-line arguments into one question, sent one
nonstreaming Chat Completions request, printed the answer, and exited. There was
no system message or conversation history. Configuration and API error handling
already existed.

## Required behavior

### Startup and input

- `cargo run` starts the REPL without a positional question. Reject supplied
  arguments with a clear stderr error and nonzero status, without an API call.
- Preserve environment configuration: required nonblank `OPENAI_API_KEY`,
  `OPENAI_BASE_URL` defaulting to `https://api.openai.com/v1`, and `OPENAI_MODEL`
  defaulting to `gpt-4.1`. Validate configuration before prompting.
- Before each read, write and flush `> ` on stderr, for terminal and piped stdin.
  Terminal detection is unnecessary.
- Each input line is one user turn. Remove its line ending, including CRLF;
  preserve other whitespace in submitted text.
- Skip whitespace-only lines without changing history or calling the API.
- Recognize `/exit` after trimming surrounding whitespace. Exit successfully
  without adding a message or making a request. Other slash-prefixed input is text.
- EOF exits successfully. Process a final nonempty line even without a newline.

### Conversation and requests

- Keep `history: Vec<ChatMessage>` in memory for the current process only.
- Initialize it with exactly one system message, retained as the first message
  throughout the session. Capture the cwd once at startup.
- Use this five-line system prompt, replacing `<cwd>` with the actual path:

  ```text
  You are a coding assistant.
  Be concise and direct.
  Help the user with programming tasks.
  Work in the current working directory: <cwd>
  Be clear about what you know and what you have actually done.
  ```

- For each accepted line, append a user message, send the entire history to
  `{base_url}/chat/completions`, stream the first choice's content, validate
  completion, print and flush one terminating newline, then append exactly one
  assistant message containing the concatenated deltas.
- Send the configured model and Bearer authorization. Reuse one HTTP client.
  Include `"stream": true` and `"stream_options": {"include_usage": true}` in
  every request. Keep typed serde request, chunk, choice, and delta structures.
- Send requests sequentially. Do not read the next turn while awaiting a reply.
- Write each string content delta to the locked stdout and flush immediately,
  preserving exact whitespace and Unicode. Accumulate the same fragments without
  reprinting them. Prompts and errors belong on stderr; no role labels, banners,
  raw SSE, usage reports, or echoed input appear on stdout.

### Streaming and completion

- Check HTTP status before decoding a successful body incrementally with
  `eventsource-stream` and `futures` over reqwest's byte stream. Parse SSE event
  data without requiring event names; `[DONE]` is the end marker. The decoder
  handles blank-line event boundaries, LF/CRLF, comments, multiline data, and
  network boundaries within JSON or UTF-8 characters.
- Decode other event data as typed JSON chunks, ignoring unrelated fields. Use
  the first choice when choices are nonempty. Optional `delta.content` may be
  absent or null; role-only and empty deltas emit no text.
- Record the first non-null finish reason from the first choice. Continue
  consuming events afterward to accept a usage chunk and `[DONE]`.
- Empty choices with non-null usage are valid. Empty choices without usage are
  errors. Usage itself is optional; do not display, persist, or calculate costs
  from it.
- Success requires `[DONE]`, a recorded finish reason, and at least one string
  content delta. An explicit empty string counts, prints a terminating newline,
  and is retained as an empty assistant reply. Missing-content errors include
  the finish reason or `unknown`.
- Only after successful validation and the terminating newline's write/flush
  may the complete assistant message enter history. Later requests include it
  exactly once.

### Failure handling

- Any configuration, cwd, input/output, transport, HTTP, or response-parsing
  failure ends the process with context on stderr and nonzero status.
- Preserve HTTP status and response body in errors for unsuccessful responses.
- Transport/SSE decoding errors, malformed JSON, invalid chunk shapes, empty
  choices without usage, premature EOF without `[DONE]`, missing finish reason,
  and missing string content are fatal. A finish reason without `[DONE]` is
  insufficient, as is `[DONE]` without a finish reason.
- A failed stream leaves already flushed text visible without a success newline.
  Do not erase it, replay the request, fabricate completion, append partial
  history, or submit a queued next turn. Output write/flush failures are fatal.
- Do not retry or return to the prompt after failure. Earlier successful replies
  remain on stdout; no fabricated assistant message is added.

## Tech stack and structure

Rust edition 2024, one binary crate. Use `anyhow`, `reqwest` (JSON, rustls,
and stream), `serde` (derive), `serde_json`, `tokio` (full), `futures`, and
`eventsource-stream`. Resolved versions remain governed by tracked `Cargo.lock`.

- `src/main.rs`: configuration, wire types, HTTP handling, and the small REPL;
  keep production code here until it exceeds roughly 300 lines.
- `tests/chat_loop.rs`: subprocess integration tests using stdin/stdout/stderr
  and a local HTTP mock, including chunked SSE fixtures and a timeout-bounded
  stdout handshake proving incremental flushing.
- `SPEC-chat-loop.md`: this feature's requirements.
- `PLAN.md`: implementation progress; check each step only after verification.
- `AGENTS.md`: update its running example to show `cargo run`.
- `Cargo.lock`: stays tracked; `/target` stays ignored.

Use rustfmt, snake_case functions and variables, PascalCase types, typed serde
messages, and `anyhow` context for fallible operations. Preserve the wire shape:

```rust
#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: Option<String>,
}
```

For example, input `hello` produces the prompt `> ` on stderr and the complete
mock reply `Hello!\n` on stdout.

## Commands and verification

```bash
cargo run
cargo fmt --check
cargo build --locked
cargo test --locked
```

Run the REPL with the key already exported. Automated HTTP checks use a local
mock URL and a dummy key; paid provider calls are unnecessary. Use Rust's test
framework and subprocess integration tests. No model inference or coverage
percentage target is required. Cover these observable contracts:

1. No-argument startup accepts stdin; supplied arguments fail without HTTP calls.
2. Two successful turns make exactly two requests with role sequences
   `[system, user]` and `[system, user, assistant, user]`. Assert exact prior
   contents, configured model, path, and dummy authorization header.
3. The system message occurs once, contains the subprocess cwd, and stays
   unchanged between requests. Run the subprocess in a throwaway directory.
4. Prompts appear on stderr; exact deltas followed by one added success newline
   appear only on stdout. A bounded handshake proves output before stream completion.
5. Blank lines make no request; CRLF input and a final line without newline work.
6. `/exit`, whitespace-padded `/exit`, and EOF exit with status zero. Input after
   `/exit` is not processed. Other slash-prefixed text is submitted normally.
7. A failure on the second request exits nonzero, preserves the first reply,
   and does not submit a queued third turn. Cover HTTP errors with body, malformed
   JSON, invalid chunks, empty choices without usage, missing content or finish
   reason, EOF before `[DONE]`, and transport/SSE failures after partial output.
8. Missing/blank API keys fail before input or HTTP. Base URL and model defaults
   remain intact, with overrides checked using the mock.
9. Cover role-only, null/missing content, explicit empty strings, first-choice
   behavior, optional usage, multiline data, comments, LF/CRLF, and JSON/Unicode
   split across network writes; include multiple events in one write. Check
   stdout write/flush failures stop the next turn.
10. Formatting, locked build, and locked tests pass. Report whether verification
    used mocks or a live provider.

## Boundaries

- Always stay within Steps 3–4, use contextual errors, preserve stdout/stderr
  separation, run required checks, and update progress only after verification.
- Ask first before expanding beyond this spec, adding dependencies, or changing CI.
- Never print or commit API keys, make paid calls for automated validation,
  remove failing tests to pass checks, or alter unrelated user changes.

Tool execution and streaming tool calls are deferred to Phase 2. Saved
sessions, resume, compaction, context limits, multiline editing, input history,
TUI, special Ctrl-C handling, retries, recovery, extra commands, and print mode
are out of scope. Retain normal process signal behavior. Loading project
instructions or a separate environment block belongs to future context assembly;
this step includes only the cwd in the tiny system prompt.

## Open questions

None blocking. Approval includes the inherited input/system-prompt behavior,
finish reason plus `[DONE]`, optional usage, missing-content policy, and fail-fast
partial-output behavior above.
