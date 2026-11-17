# Design delta: Step 5 tool plumbing and bash — V1

Status: implemented and verified on 2026-11-17 (approved by the user on 2026-11-17)

## Intent and scope

Implement only [PLAN.md Step 5](../../../PLAN.md#step-5-tool-plumbing--bash):
turn the streaming REPL into an agent that can run bash commands and answer from
their results. For example, asking what files are in the current folder should
produce a bash `ls` call, a matching tool result, and an assistant answer.

This extends [SPEC-chat-loop.md](../../../SPEC-chat-loop.md). Its configuration,
input handling, short system prompt, SSE decoding, stdout/stderr separation,
and fail-fast transport/output behavior remain in force except as changed here.
Use the existing Rust 2024 binary crate and dependencies; no provider abstraction,
workspace, permissions layer, or additional tools.

## Approaches

The selected approach is a small extension of the existing streaming client plus
a sequential tool loop. Keep production code in `src/main.rs` until it exceeds
roughly 300 lines, then extract focused modules within the same crate.

A provider-neutral event system or generic tool framework would anticipate the
long-term architecture but add machinery this step does not need. Concurrent
execution would make dependent bash commands unpredictable and belongs to later
work. These alternatives were ruled out by the current plan; the user approved
the minimal sequential approach.

## Messages and requests

- Add optional assistant `tool_calls` with typed entries containing `id`,
  `type: "function"`, and `function: { name, arguments }`. Arguments remain the
  exact accumulated JSON **string** in conversation history.
- Add optional `tool_call_id` to messages for the `tool` role. Omit irrelevant
  optional tool fields from ordinary messages. A tool-only assistant message has
  `content: null`; preserve any accompanying assistant text.
- Every API request includes the single bash tool definition exactly as given
  in Step 5: description "Run a bash command in the current directory.", object
  parameters with a required string `command`. Keep streaming and usage options.

## Stream completion and execution

- Accumulate `delta.tool_calls` by numeric index, independently of text deltas.
  The initial fragment provides the id and function name; concatenate argument
  fragments without interpreting partial JSON. Preserve numeric call order.
- Continue to require a recorded finish reason and `[DONE]`. A valid nonempty
  tool batch with finish reason `tool_calls` can complete without text. Text-only
  responses retain their existing rules, including explicit empty-string text.
- Execute nothing until the entire stream is complete and the batch has been
  validated: unique nonblank ids, supported function type, name `bash`, and JSON
  arguments deserializable to a typed structure with string `command`. Reject
  conflicting/incomplete metadata or a mismatch between calls and finish reason.
  Validate all calls before running any, so an invalid later call does not cause
  an earlier command to execute.
- Append the completed assistant message, execute each call in order, append
  exactly one `tool` message per call with its original id, and request another
  assistant response. Repeat until a response has no tool calls; only then read
  the next user line. Do not add a tool-round limit in this step.

## Bash and output

- Run `tokio::process::Command::new("bash").arg("-c")` with the supplied command
  as a single subsequent argument. Each call is a fresh shell in the agent's
  startup working directory; `cd` and shell variables do not persist across calls.
- Give the child null stdin, reserving REPL input for the agent.
- Merge stdout and stderr into a shared capture destination at process launch,
  rather than concatenating two separately captured streams after completion.
  Capture the termination status and decode output as UTF-8 lossily.
- Return full merged output plus status in tool-message content as a JSON string:
  `{"output":"...","exit_code":0}`. A process terminated by signal has
  `exit_code: null` and an additional `termination` description. A nonzero exit
  code is a completed tool result and the model continues; the user confirmed
  this behavior. Spawn/capture failures remain fatal contextual application errors.
- Print and flush `$ <command>` to stderr before execution. After completion,
  print status and a preview capped at 10 lines and 2,000 Unicode characters,
  with an explicit omission note if shortened. This is only a human preview;
  the model receives full output. Assistant text alone remains on stdout.
- Print a terminating stdout newline only for assistant responses that contain
  text, including explicit empty strings. Tool-only responses add no blank line.

The result representation, null stdin, preview limits, lossy decoding, signal
description, and whole-batch validation above are design defaults proposed by
the assistant, rather than separately requested user requirements.

## Validation and boundaries

Extend subprocess/local HTTP mock tests in `tests/chat_loop.rs`, using throwaway
working directories and harmless commands. Verify the exact advertised schema,
fragmented/interleaved calls, numeric execution order, tool-only and mixed text
responses, exact call/result history over several tool rounds and user turns,
merged output, exit codes, null stdin, previews, and failure before execution for
incomplete streams or invalid batches. Preserve existing chat/streaming tests.

Run `cargo fmt --check`, `cargo build --locked`, and `cargo test --locked`, plus a
mock-backed smoke check of the folder-listing scenario. No live provider or paid
calls are necessary. Mark Step 5 items complete only after behavior is verified.

Always preserve user changes, contextual stderr errors, and nonzero application
exit status on fatal failures. Never print/commit API keys or remove failing
tests to make validation pass. Ask before expanding scope or adding dependencies.

Step 6 retains timeouts, process-group cleanup, model-output truncation, the
`Error (exit code N):` prefix, and returning malformed-argument errors to the
model for recovery. Invalid arguments and unknown tools are fatal for Step 5.
Interrupt recovery, retries, saved sessions, and live tool-output streaming are
also out of scope. No implementation begins until this design is approved.

## Open questions

None blocking; the proposed defaults above are included in design approval.
