# Spec: Step 3 Chat Loop — V1

Status: approved

## Objective

Replace the one-question CLI with a minimal line-based REPL so later turns include
earlier messages. This implements [PLAN.md Step 3](PLAN.md#step-3-chat-loop-repl);
later steps remain deferred. Success means launching `cargo run`, exchanging
multiple turns, and quitting with `/exit` or end-of-input.

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
  `{base_url}/chat/completions`, validate the response, append the first choice's
  assistant message, and print its content followed by a newline.
- Send the configured model and Bearer authorization. Reuse one HTTP client.
  Keep typed serde request/response structures and nonstreaming behavior.
- Send requests sequentially. Do not read the next turn while awaiting a reply.
- Print only complete assistant replies on stdout, without role labels, banners,
  usage reports, or echoed input. Prompts and errors belong on stderr.

### Failure handling

- Any configuration, cwd, input/output, transport, HTTP, or response-parsing
  failure ends the process with context on stderr and nonzero status.
- Preserve HTTP status and response body in errors for unsuccessful responses.
- Empty choices and missing/null message content remain errors, including the
  existing finish-reason context for missing content. Empty-string content prints
  a newline and retains the assistant message.
- Do not retry or return to the prompt after failure. Earlier successful replies
  remain on stdout; no fabricated assistant message is added.

## Tech stack and structure

Rust edition 2024, one binary crate. Existing dependency declarations are
`anyhow = 1.0.104`, `reqwest = 0.12` (JSON and rustls), `serde = 1.0.229`
(derive), `serde_json = 1.0.154`, and `tokio = 1.54.1` (full). Resolved versions
remain governed by tracked `Cargo.lock`. No new dependency is expected.

- `src/main.rs`: configuration, wire types, HTTP handling, and the small REPL;
  keep production code here until it exceeds roughly 300 lines.
- `tests/chat_loop.rs`: subprocess integration tests using stdin/stdout/stderr
  and a local HTTP mock built from existing facilities.
- `SPEC-chat-loop.md`: this feature's requirements.
- `PLAN.md`: implementation progress; check Step 3 only after verification.
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
4. Prompts appear on stderr and complete reply text appears only on stdout.
5. Blank lines make no request; CRLF input and a final line without newline work.
6. `/exit`, whitespace-padded `/exit`, and EOF exit with status zero. Input after
   `/exit` is not processed. Other slash-prefixed text is submitted normally.
7. A failure on the second request exits nonzero, preserves the first reply,
   and does not submit a queued third turn. Cover HTTP errors with body, malformed
   JSON, empty choices, missing content, and a transport failure.
8. Missing/blank API keys fail before input or HTTP. Base URL and model defaults
   remain intact, with overrides checked using the mock.
9. Formatting, locked build, and locked tests pass. Report whether verification
   used mocks or a live provider.

## Boundaries

- Always follow Step 3 scope, use contextual errors, preserve stdout/stderr
  separation, run required checks, and update Step 3 only after verification.
- Ask first before expanding beyond this spec, adding dependencies, or changing CI.
- Never print or commit API keys, make paid calls for automated validation,
  remove failing tests to pass checks, or alter unrelated user changes.

Streaming is deferred to Step 4. Tool execution is deferred to Phase 2. Saved
sessions, resume, compaction, context limits, multiline editing, input history,
TUI, special Ctrl-C handling, retries, recovery, extra commands, and print mode
are out of scope. Retain normal process signal behavior. Loading project
instructions or a separate environment block belongs to future context assembly;
this step includes only the cwd in the tiny system prompt.

## Open questions

None blocking. Approval includes rejecting arguments, preserving input whitespace
except line endings, trimmed `/exit` recognition, prompts for piped input, and
the five-line system prompt above.
