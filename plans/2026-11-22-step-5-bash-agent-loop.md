# Plan: Step 5 — Bash tool and agent loop

- Date: 2026-11-22
- Domain: backend / terminal CLI
- Author: plan-from-spec (reviewed with the user in conversation)
- Status: Approved

This records the approved Step 5 scope and contracts. Implementation and
validation progress belongs in [PLAN.md](../PLAN.md).

## Scope and resolved decisions

Extend the existing streaming Chat Completions client into a minimal agent.
Advertise only `bash { command: string }`, reconstruct streamed tool calls,
validate the whole batch, execute sequentially without approval, return one result
per call ID, and request another completion until the model returns no calls.
Implement only PLAN.md Step 5, in one Rust 2024 binary crate with the existing
dependencies. Preserve all 16 baseline integration tests. Leave skills and
`skills-lock.json` untouched.

Each invocation starts fresh noninteractive `bash -c <command>` at the original
cwd with the inherited environment and user permissions. Null stdin prevents
commands consuming REPL input. Commands share filesystem changes but no shell
state. Nonzero and signal exits are results; invalid calls, unknown tools,
malformed arguments, and launch/wait/capture/output failures are fatal. Never
retry a batch; previously completed side effects are not rolled back.

## Structure and interfaces

- `main.rs`: configuration, short unchanged system prompt, REPL, sequential
  model/tool rounds, and history.
- `types.rs`: typed messages, calls, and strict Bash argument deserialization.
- `openai.rs`: existing HTTP status/body and SSE behavior, tool schema,
  indexed argument accumulation, and complete-batch validation.
- `bash.rs`: process launch, merged output capture, status and result formatting,
  and bounded stderr preview.

Every request retains configured model, messages, `stream: true`, and
`stream_options.include_usage: true`, and adds the same single Bash schema with
`additionalProperties: false`. Ordinary text messages omit absent tool fields.
Tool-only assistants have `content: null` and `tool_calls`; tool results have
`role: "tool"`, `tool_call_id`, and string content. Preserve the original argument
JSON string when replaying history.

Accumulate by zero-based index; require contiguous indices, unique nonblank IDs,
`type: "function"`, name `bash`, and a JSON object containing exactly a string
`command`. Empty commands are valid. Reject conflicting repeated call metadata
and malformed chunks. Validate every call before executing any command; execute
in ascending index order. Require `[DONE]` and a recorded finish reason. Calls
require reason `tool_calls`; that reason without calls is invalid. Responses
without calls require at least one string content delta, including an explicitly
empty string. Preserve baseline missing-content diagnostics and the first
non-null finish reason.

Stream text immediately and flush each delta. Only successfully validated
text-bearing responses add and flush one stdout newline. Tool-only responses
add none. Append each completed assistant once, then each matching tool result,
then request another completion without reading user input. On failure, retain
previously flushed text without a success newline and exit nonzero with context
on stderr.

## Command capture and diagnostics

Merge stdout and stderr with `std::io::pipe`, cloning the writer for both child
streams. Drop the command builder after spawning so parent writers cannot
prevent EOF. Drain the reader on a blocking worker concurrently with the async
child wait; never concatenate independently captured streams. The pipe preserves
observed writes, without promising an order for independent concurrent writers.
Convert bytes to text using UTF-8 replacement.

Send complete results to the model as `Exit code: N\n<output>` or
`Termination: signal\n<output>`. Log `$ <command>` to stderr and flush before
execution. Print a status header and the first 20 lines and at most 4096 UTF-8
bytes of output, with a truncation notice if necessary. Only the output preview
is capped; model results, command text, status, and notices stay complete.

## Ordered implementation tasks

1. Introduce typed messages, function calls, constructors, and Bash arguments;
   preserve exact ordinary message serialization.
2. Extract the existing HTTP/SSE client and add the Bash schema, indexed streamed
   calls, metadata validation, and tool-aware completion validation.
3. Implement Bash execution, merged capture, result formatting, and preview.
4. Wire sequential calls and model rounds into the unchanged input loop.
5. Add unit tests for serialization, exact argument shape, interleaved fragments,
   ordering, missing/conflicting metadata, duplicate IDs, gaps, preview line/byte/
   Unicode boundaries, and stderr write failures.
6. Extend the mock subprocess suite with nonzero exits and exact history,
   sequential calls/rounds, cwd/environment/shell isolation, invalid batches,
   missing Bash, null stdin, large capture/full results/bounded preview,
   fragmented calls and text flushing, and completion-contract failures.
7. Run `cargo fmt`, `cargo fmt --check`, `cargo build --locked`, and
   `cargo test --locked`. Only after success, mark all six Step 5 checklist items
   complete and record mock verification. Amend SPEC-chat-loop.md to describe
   the Steps 3–4 baseline, superseded only for tool-bearing responses.

Use dummy credentials, local loopback HTTP mocks, and throwaway command working
directories. Preserve existing request assertions and tests. If a malformed
stream fixture encounters an expected client disconnect, handle that disconnect
only in the relevant fixture. No live paid-provider calls or numerical coverage
target are required. No Cargo.toml, Cargo.lock, CI, AGENTS.md, or old acceptance
example changes are needed.

## Acceptance and explicit deferrals

Existing startup, configuration, exact text history, streaming flush, EOF and
fail-fast behavior must pass. Verify schema/path/model/auth, tool-only and
text-plus-tool responses, fragmented/interleaved calls, whole-batch validation,
sequential execution, exact result IDs/history, merged output, nonzero exits,
fresh shells, null stdin, and bounded diagnostics without truncating model
results or deadlocking large capture. Fatal errors must stop subsequent rounds
and queued user turns.

Step 6 timeouts, process groups, output/context truncation, recoverable argument
errors, and error-prefix conventions are deferred. Also exclude execution/round
limits, persistent shells, PTYs, interactive stdin, live tool-output streaming,
additional tools, sessions, retries, provider abstractions, flags, UI changes,
workspaces, and approval prompts. Unbounded output uses unbounded memory; long
commands or background descendants holding pipe writers may keep a call open
indefinitely. Signal results omit platform-specific signal numbers.

## References

- [Rust std::io::pipe](https://doc.rust-lang.org/std/io/fn.pipe.html), available
  since Rust 1.87 and compatible with the pinned 1.99.1 toolchain.
- [Tokio 1.54.1 Command](https://docs.rs/tokio/1.54.1/tokio/process/struct.Command.html).
- [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling).

## Implementation discovery

The argument-shape regression test showed that Serde's derived struct visitor
also accepts a positional array such as `["ls"]`. Add an explicit object check
before typed deserialization, retaining rejection of unknown and duplicate
fields. This enforces the approved object-only contract without expanding scope.
