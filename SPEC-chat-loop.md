# Spec: Streaming Chat Loop with Bash Tools — V1

Status: implemented (Steps 3–5)

## Objective

Provide a minimal streaming, line-based REPL with conversation history and a
sequential bash tool loop. This implements [PLAN.md Step 3](PLAN.md#step-3-chat-loop-repl),
[Step 4](PLAN.md#step-4-streaming), and [Step 5](PLAN.md#step-5-tool-plumbing--bash).
Success means exchanging multiple turns, running bash commands when requested by
the model, answering from their results, and quitting with `/exit` or end-of-input.
The [approved Step 5 delta](docs/superpowers/specs/2026-11-17-step-5-bash-design.md)
records the design choices; Step 6 guard rails remain deferred.

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
  completion, and append exactly one assistant message containing the accumulated
  text and tool calls. Print and flush one terminating newline only when text was
  present, including an explicit empty string. Tool-only replies add no blank line.
- Assistant tool calls contain typed `id`, `type: "function"`, and
  `function: {name, arguments}` fields. Preserve the exact accumulated arguments
  JSON **string** in history. A tool-only assistant message has `content: null`.
  Omit empty `tool_calls` and absent `tool_call_id` fields from ordinary messages.
- Every request advertises exactly one tool, with no extra schema options:

  ```json
  {"type":"function","function":{"name":"bash","description":"Run a bash command in the current directory.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}
  ```
- Send the configured model and Bearer authorization. Reuse one HTTP client.
  Include `"stream": true` and `"stream_options": {"include_usage": true}` in
  every request. Keep typed serde request, chunk, choice, and delta structures.
- Send requests sequentially. After a completed tool batch, append the assistant
  message, execute each call in numeric index order, append exactly one `tool`
  message per call with its original `tool_call_id` and result content, then
  request another assistant response. Repeat until there are no calls. Do not
  prompt or read another user line between rounds. There is no tool-round limit.
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
- Accumulate `delta.tool_calls` by numeric index independently from text. Preserve
  sparse numeric order and concatenate argument fragments without parsing partial
  JSON. Initial metadata supplies id, type and function name. Repeated identical
  metadata is accepted; conflicts or incomplete metadata are fatal.
- Success requires `[DONE]` and a recorded finish reason. A nonempty valid tool
  batch requires `finish_reason: "tool_calls"`; that reason requires a nonempty
  batch. Otherwise at least one string content delta is required. An explicit
  empty string counts. Missing-content errors include the finish reason or `unknown`.
- Validate the entire batch before running anything: ids are nonblank and unique,
  type is `function`, name is `bash`, and arguments deserialize to a typed object
  with a string `command`. Invalid later calls prevent earlier calls from running.
- Only after successful validation and any terminating newline's write/flush may
  the complete assistant message enter history. Later requests include it once.

### Bash execution and diagnostics

- Run `tokio::process::Command::new("bash").arg("-c").arg(command)` with the command
  as one argument, null stdin, and explicit startup cwd. Each call is a fresh shell;
  `cd` and shell variables do not persist. REPL input stays reserved for the agent.
  Remove `OPENAI_API_KEY` from the child environment; the parent retains it for HTTP
  authorization so command output cannot expose it through environment inspection.
- Merge stdout and stderr into one pipe at launch. Drain it concurrently with the
  child to avoid pipe-buffer deadlocks, capture termination status, and decode the
  full output as UTF-8 lossily.
- Tool content is a JSON string encoding `{"output":"...","exit_code":0}`.
  Signal termination uses `exit_code: null` and an additional nonempty
  `termination` description. Nonzero exit codes are completed tool results and
  the model continues. Spawn, wait and capture failures are fatal contextual errors.
- Before spawning, write and flush `$ <command>` to stderr. After completion,
  write status and a preview capped at 10 lines and 2,000 Unicode scalar values.
  Add `[output preview truncated]` if shortened. The model receives full output;
  the preview limits affect stderr only. All diagnostic write/flush failures are
  fatal. Only assistant text goes to stdout.

### Failure handling

- Any configuration, cwd, input/output, transport, HTTP, or response-parsing
  failure ends the process with context on stderr and nonzero status.
- Preserve HTTP status and response body in errors for unsuccessful responses.
- Transport/SSE decoding errors, malformed JSON, invalid chunk shapes, empty
  choices without usage, premature EOF without `[DONE]`, missing finish reason,
  missing required string content, invalid tool batches, and calls/finish-reason
  mismatches are fatal. A finish reason without `[DONE]` is
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

- `src/main.rs`: configuration, startup prompt, REPL and sequential tool loop.
- `src/types.rs`: typed messages, tool calls, bash arguments and completed replies.
- `src/openai.rs`: typed wire/schema structures, HTTP/SSE handling, indexed
  accumulation and complete-batch validation.
- `src/bash.rs`: shell execution, merged capture, status serialization and previews.
- `tests/chat_loop.rs`: subprocess integration tests using stdin/stdout/stderr
  and a local HTTP mock, including chunked SSE fixtures and a timeout-bounded
  stdout handshake proving incremental flushing. Shared fixtures and the
  throwaway-directory/mock-server harness live in `tests/support/mod.rs`.
- `SPEC-chat-loop.md`: this feature's requirements.
- `PLAN.md`: implementation progress; check each step only after verification.
- `Cargo.lock`: stays tracked; `/target` stays ignored.

Use rustfmt, snake_case functions and variables, PascalCase types, typed serde
messages, and `anyhow` context for fallible operations. Preserve the wire shape:

```rust
#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
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
2. Two successful text-only turns make exactly two requests with role sequences
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
10. Verify the exact single bash schema; fragmented/interleaved calls, escaped
    arguments and numeric order; null/tool-only and mixed text messages; exact
    history across multiple tool rounds and user turns; and whole-batch rejection
    before execution for invalid calls or incomplete streams.
11. Verify merged output, nonzero/signal status, lossy UTF-8, fresh cwd/shell state,
    null stdin with queued input, output larger than a pipe buffer, diagnostic
    failures, and both preview caps without shortening model-visible output.
12. Formatting, locked build, and locked tests pass. Smoke-test a folder-listing
    tool round via `cargo run` against a local mock in a throwaway directory.
    Report whether verification used mocks or a live provider.

Verified on 2026-11-18 with 9 unit and 25 subprocess integration tests, the three
required Cargo checks, and the folder-listing smoke scenario. All HTTP checks
used local mocks and dummy credentials; no live provider calls.

## Boundaries

- Always stay within Steps 3–5, use contextual errors, preserve stdout/stderr
  separation, run required checks, and update progress only after verification.
- Ask first before expanding beyond this spec, adding dependencies, or changing CI.
- Never print or commit API keys, make paid calls for automated validation,
  remove failing tests to pass checks, or alter unrelated user changes.

Step 6 owns timeouts, process-group cleanup, truncation of model-visible output,
`Error (exit code N):` prefixes, and malformed-argument recovery. Invalid arguments
and unknown tools remain fatal in Step 5. Live tool-output streaming, saved
sessions, resume, compaction, context limits, multiline editing, input history,
TUI, special Ctrl-C handling, retries, recovery, extra commands, and print mode
are out of scope. Retain normal process signal behavior. Loading project
instructions or a separate environment block belongs to future context assembly;
this step includes only the cwd in the tiny system prompt.

## Open questions

None blocking. Approval includes the inherited input/system-prompt behavior,
finish reason plus `[DONE]`, optional usage, text/tool completion policy, bash
result representation and previews, and fail-fast partial-output behavior above.
