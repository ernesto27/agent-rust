# PLAN: minimal agent in Rust, pi style, on the OpenAI API

The goal is the smallest coding agent that works, built one step at a time, following pi's approach (see [AGENT_DESIGN.md](AGENT_DESIGN.md) §4):
- a short system prompt
- four tools: `read`, `write`, `edit`, `bash`
- no permission prompts, no MCP, no subagents, no plan mode

[ARCHITECTURE.md](ARCHITECTURE.md) is the long-term picture. This plan deliberately starts much smaller.

**Rules for this plan**
- Each step ends with something you can **run and see working**. Don't start the next step until the current one works.
- One crate (`cargo new`), no workspace. Split into modules only when a file grows past about 300 lines.
- **One API: OpenAI Chat Completions** (`POST /v1/chat/completions`). It is the simplest OpenAI API for tool calling. The base URL is configurable, so the same code also works with OpenRouter, Groq, Ollama and LM Studio. The newer Responses API is an optional upgrade in Phase 4.
- YOLO like pi: the agent runs commands with your permissions. Test it in a throwaway folder or a container.
- Add a dependency only when a step needs it.

**Environment variables**

| Variable | Default | Purpose |
|---|---|---|
| `OPENAI_API_KEY` | (required) | sent as `Authorization: Bearer <key>` |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | point at OpenRouter, Ollama (`http://localhost:11434/v1`) and so on |
| `OPENAI_MODEL` | `gpt-4.1` | can be overridden with `--model` later |

---

## Phase 1: Talk to the model

### Step 1: Project setup
- [x] `cargo init --name agent` in this folder (edition 2024).
- [x] Add dependencies: `tokio` (features `full`), `reqwest` (features `json`, `rustls-tls`), `serde` (features `derive`), `serde_json`, `anyhow`.
- [x] Read `OPENAI_API_KEY`, `OPENAI_BASE_URL` and `OPENAI_MODEL` from the environment, with the defaults above. Exit with a clear error if the key is missing.
- [x] Add `.gitignore` with `/target`.

**Done when:** `cargo run` prints the base URL and model it will use (or the missing-key error).

### Step 2: One question, one answer (no streaming)
- [x] `POST {base_url}/chat/completions` with the header `Authorization: Bearer {key}`.
- [x] Body: `{ "model": "...", "messages": [{ "role": "user", "content": "<arg>" }] }`.
- [x] Define just enough serde types:
  - `ChatMessage { role, content: Option<String> }`
  - the response: `choices[0].message`, `choices[0].finish_reason`, `usage`
- [x] Print `choices[0].message.content`.
- [x] Print the API error body if the status is not 2xx.

**Done when:** `cargo run -- "say hi"` prints the model's reply.

### Step 3: Chat loop (REPL)
- [x] Read lines from stdin in a loop. Keep `history: Vec<ChatMessage>`, append the user message, call the API, append the assistant message.
- [x] The **first message** is the system prompt, `{ "role": "system", "content": "..." }`. Make it tiny, pi style, about 5 lines: who you are, be concise, work in the cwd.
- [x] `/exit` or Ctrl-D quits.

**Done when:** you can have a multi-turn conversation and the model remembers earlier turns.

Implemented per [SPEC-chat-loop.md](SPEC-chat-loop.md). Verified with subprocess
integration tests and local HTTP mocks (no live provider calls), including exact
two-turn history, cwd, input handling, configuration, and fail-fast errors.

### Step 4: Streaming
- [x] Add `"stream": true` and `"stream_options": { "include_usage": true }`, plus the `futures` and `eventsource-stream` dependencies.
- [x] Decode SSE event data without requiring event names; each event carries a JSON chunk or the `[DONE]` end marker.
- [x] Consume optional `choices[0].delta.content` fragments and record the first non-null `finish_reason`. Continue through optional usage-only chunks with empty `choices` to `[DONE]`.
- [x] Print text deltas as they arrive (`print!` + `flush`).
- [x] Rebuild the full assistant message from the deltas and push it to the history.

**Done when:** text appears word by word, and multi-turn still works.

Implemented per the approved Step 4 streaming delta and incorporated into
[SPEC-chat-loop.md](SPEC-chat-loop.md). Verified with 16 subprocess integration
tests and a `cargo run` smoke check against local HTTP mocks (no live provider
calls), including bounded stdout handshakes, split SSE/JSON/Unicode, exact
history, optional usage, partial failures, and stdout write/flush errors.

---

## Phase 2: Become an agent (tools)

### Step 5: Tool plumbing + `bash`
- [x] Extend the types:
  - An assistant message can have `tool_calls: [{ id, type: "function", function: { name, arguments } }]`. Note that `arguments` is a **JSON string**, not an object.
  - Add a `tool` role message: `{ "role": "tool", "tool_call_id": "...", "content": "..." }`.
- [x] Send a `tools` array with one tool, `bash`:
  ```json
  { "type": "function",
    "function": { "name": "bash",
                  "description": "Run a bash command in the current directory.",
                  "parameters": { "type": "object",
                                  "properties": { "command": { "type": "string" } },
                                  "required": ["command"] } } }
  ```
- [x] Streaming: tool calls arrive in `delta.tool_calls[]`, keyed by `index`.
  - The first fragment for an index carries `id` and `function.name`. Later fragments append to `function.arguments`.
  - Accumulate per index, and parse the arguments with `serde_json::from_str` once the stream ends (`finish_reason: "tool_calls"`).
- [x] Run the command with `tokio::process::Command::new("bash").arg("-c")`, merging stdout and stderr and capturing the exit code.
- [x] **The agent loop:** while the last assistant message has `tool_calls`, run each one and push **one `tool` message per call** (matching `tool_call_id`), then call the API again. Stop when there are no tool calls.
- [x] Print each tool call (`$ <command>`) and a short preview of the output, so you can see what happens.

**Done when:** "what files are in this folder?" makes the model run `ls` and answer from the output.

Implemented per [the Step 5 plan](plans/2026-11-22-step-5-bash-agent-loop.md).
Verified with unit tests and subprocess integration tests using local HTTP mocks
and throwaway working directories; no live provider calls. Coverage includes
streamed arguments, tool-only/text-plus-tool replies, sequential calls and rounds,
exact result history, merged output and nonzero exits, fresh shell state, null
stdin, bounded terminal previews, and fatal protocol/launch failures. Step 6
guardrails and recoverable argument errors remain deferred.

### Step 6: Guard rails for `bash`
- [ ] Timeout, 120 s by default, optionally set with `timeout_ms` in the arguments. Kill the process on timeout.
- [ ] Spawn with `process_group(0)` and kill the whole group, so child processes die too.
- [ ] Truncate the output to the **last** ~2000 lines or 50 KB, with a note like `[output truncated: showing last N lines]`.
- [ ] Chat Completions has no `is_error` flag. On a non-zero exit code or a timeout, start the content with `Error (exit code N):` so the model notices.
- [ ] If the `arguments` string is invalid JSON, don't crash. Return a tool message saying so, and the model will retry.

**Done when:** `sleep 999` times out cleanly and `cat` on a huge file doesn't flood the context.

### Step 7: `read` tool
- [x] Parameters: `{ path, offset?, limit? }`. Relative paths are resolved against the cwd.
- [x] Output is line-numbered (`{n:>6}\t{line}`), 2000 lines by default, and each line is cut at 2000 chars.
- [x] Tell the model when there is more: `[showing lines 1-2000 of 5400, use offset to read more]`.
- [x] Clear errors for a missing file, a directory or a binary file.
- [x] Move the tool definitions into a list, `Vec<ToolDef>`, and dispatch on `function.name`.

**Done when:** the model reads a file with `read` instead of `cat`.

Implemented per [the Step 7 plan](plans/2026-11-22-step-7-read-tool.md).
Verified with reader unit tests and subprocess integration tests using local
HTTP mocks and throwaway directories. Step 6 Bash guardrails remain deferred.

### Step 8: `write` and `edit` tools
- [x] `write { path, content }` creates parent directories and overwrites the file. It writes the supplied UTF-8 bytes verbatim, refuses a symlink or other non-regular file as the final destination, and returns a short confirmation. A bounded diff preview goes to stderr, not to the model.
- [ ] `edit { path, old_text, new_text }`, exact replacement only (like pi):
  - [ ] Error if `old_text` is not found: "not found, re-read the file".
  - [ ] Error if it is found more than once: "found N times, add more surrounding context".
- [ ] Return a short confirmation; print a small diff with `similar` for the human.
- [ ] For `edit`, keep the original line endings (`\r\n` vs `\n`).

**Done when:** "create hello.py that prints hi, then change it to print bye" works using only `write` + `edit`.

> 🎯 **Milestone: this is a working coding agent.** Try a real task, such as "add a test to a small project and make it pass".

---

## Phase 3: Make it pleasant

### Step 9: Context: system prompt, environment, AGENTS.md
- [ ] Move the system prompt to `src/prompt.md` (`include_str!`). Keep it under about 1000 tokens, tool descriptions included.
- [ ] Append an environment block: cwd, OS, date and whether the cwd is a git repo.
- [ ] Load `AGENTS.md` from `~/.config/agent/` and from every directory from the git root down to the cwd. Concatenate them root-first.
- [ ] **Prompt caching is automatic on OpenAI** for prompts of 1024+ tokens with an identical prefix. Keep the start of the request stable: system prompt first, tools in a fixed order, and nothing that changes every turn (such as seconds in a timestamp) in the system prompt.
  - [ ] Print `usage.prompt_tokens_details.cached_tokens` to confirm cache hits.

**Done when:** a rule written in AGENTS.md ("always answer in Spanish") is followed, and the usage shows cached tokens from the second turn on.

### Step 10: CLI flags + print mode
- [ ] Add `clap`:
  - `agent` (interactive)
  - `agent -p "task"` (one task, final text to stdout, exits)
  - `--model <id>`
  - `--base-url <url>`
- [ ] Read the prompt from stdin if one is piped in: `cat err.log | agent -p "explain"`.
- [ ] Print token usage (`prompt_tokens`, `completion_tokens`, cached tokens) and an approximate cost at the end of each turn (hard-code the prices of one model for now).
- [ ] Optional: `--reasoning <low|medium|high>` sends `reasoning_effort` to reasoning models.

**Done when:** `agent -p "count the .rs files"` works from a script, and `--base-url http://localhost:11434/v1` works with a local Ollama model.

### Step 11: Interrupt with Ctrl-C
- [ ] The first Ctrl-C while the agent is working cancels the current request or tool (`tokio::select!` + `CancellationToken`) and returns to the prompt. Ctrl-C at an idle prompt exits.
- [ ] **Repair the history:** every `tool_call` id in an assistant message needs a matching `tool` message. Add `"Error: interrupted by user"` for any that are missing, otherwise the next API call fails with a 400.
- [ ] Retry 429, 5xx and network errors with backoff (1 s, 2 s, 4 s; max 3 attempts). Respect the `retry-after` header if present.

**Done when:** you can stop a long `bash` in the middle and keep chatting.

### Step 12: Save and resume sessions (JSONL)
- [ ] Each session is one file at `~/.local/share/agent/sessions/<cwd-slug>/<timestamp>.jsonl`.
- [ ] The first line is a header `{type:"session", cwd, model, base_url, created}`. Then one line per message, `{type:"message", id, parent_id, message}`, appended as soon as it exists.
- [ ] `--continue` loads the latest session for this cwd; `--resume <file>` loads a specific one. Rebuild the history from the lines and repair any dangling tool calls.
- [ ] Keep `id`/`parent_id` even though the history is a straight line for now. It makes tree-shaped sessions (pi's `/tree`, `/fork`) possible later.

**Done when:** you quit, run `agent --continue`, and the model remembers what you were doing.

---

## Phase 4: Next steps (pick by need)

Do these only when you actually miss them, in roughly this order:

- [ ] **Compaction.** When `prompt_tokens` gets close to the model's context window (window minus 16k), ask the model for a summary with fixed sections: Goal, Progress, Key decisions, Next steps, Files read/modified. Keep the last ~20k tokens verbatim and never split a tool call from its `tool` messages. Also add a `/compact` command.
- [ ] **Slash commands:** `/model`, `/clear`, `/compact`, `/cost`, and prompt templates from `.agent/prompts/*.md` with `$ARGUMENTS`.
- [ ] **Steering:** typing while the agent works queues a message, delivered after the current tool batch.
- [ ] **Nicer terminal UI:**
  - [ ] Colors with `crossterm` first: dim tool output, bold tool names, markdown via `termimad`.
  - [ ] Later, a line-based differential renderer like `pi-tui`, or `ratatui` with an inline viewport.
- [ ] **Run read-only tools in parallel** (several `read`s at once). Set `parallel_tool_calls: true`, which is already the default.
- [ ] **OpenAI Responses API** (`POST /v1/responses`), which is what Codex uses. It is better for reasoning models because it passes their reasoning back between turns (`include: ["reasoning.encrypted_content"]`, `store: false`). Introduce a `Provider` trait with provider-neutral message types at this point, and keep Chat Completions as the second implementation for OpenRouter and Ollama.
- [ ] **Anthropic Messages API** as a third provider, if wanted.
- [ ] **Session tree:** `/tree` to jump back to an earlier message and branch, `/fork` to copy the session into a new file.
- [ ] **Extra tools, off by default:** `grep` and `find` (pi keeps them optional).
- [ ] **Optional safety, opt-in:** a `--confirm` flag that asks before `bash`, `write` and `edit`.
- [ ] **Skills:** `SKILL.md` files where only the description goes into the prompt and the model `read`s the body when needed.

---

## Suggested file layout as it grows

```
src/
  main.rs        # CLI args, REPL, print mode
  openai.rs      # Chat Completions request + SSE streaming → events
  types.rs       # ChatMessage, ToolCall, Usage
  agent.rs       # the loop: call model → run tools → repeat
  tools/
    mod.rs       # tool list, JSON schemas, dispatch by name
    bash.rs
    read.rs
    write.rs
    edit.rs
  context.rs     # system prompt + env + AGENTS.md
  session.rs     # JSONL save/load
  prompt.md      # the system prompt
```

## Quick reference: Chat Completions shapes

```jsonc
// request
{ "model": "…", "stream": true, "stream_options": {"include_usage": true},
  "messages": [
    {"role": "system", "content": "You are a coding agent…"},
    {"role": "user", "content": "list the files"},
    {"role": "assistant", "content": null,
     "tool_calls": [{"id": "call_1", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "Cargo.toml\nsrc\n"}
  ],
  "tools": [ { "type": "function", "function": { "name": "bash", "description": "…", "parameters": { … } } } ] }

// streamed chunks (one per `data:` line)
{"choices":[{"index":0,"delta":{"content":"Here"}}]}
{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}
{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}}]}
{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}
{"choices":[],"usage":{"prompt_tokens":812,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":0}}}
// then: data: [DONE]
```
