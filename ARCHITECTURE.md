# Architecture: coding agent in Rust

The proposed architecture for our own terminal coding agent, written in Rust.

> Background: [AGENT_DESIGN.md](AGENT_DESIGN.md) is the research on how Claude Code, Codex CLI, OpenCode and pi work. This document draws on it, and the section names below (pi, Codex, Claude Code, OpenCode) refer back to it.

## 1. Principles: what we take from each

1. **Pi's minimal core first.** Start with a short system prompt and four or five tools, and keep all state in files. Add features only when we need them.
2. **Codex's protocol boundary.** Core talks to every UI only through typed `Op` (input) and `Event` (output) channels. TUI, headless and server modes are just different consumers.
3. **Claude Code's tool semantics.** Line-numbered reads, require a read before editing, unique `old_string`, truncate output with a "how to narrow" hint, put dynamic context in reminders rather than the system prompt (for prompt caching), and read AGENTS.md / CLAUDE.md hierarchically.
4. **OpenCode's robustness tricks.** The fuzzy edit fallback chain, formatter and diagnostics feedback after each edit, a shadow git for undo, doom-loop detection, and repairing dangling tool calls on resume.
5. **Pi's tree-shaped JSONL sessions.** Easy to inspect, append-only, and they support branching and forking.
6. **Permissions are a pluggable layer.** It starts as "yolo" or "ask on write/exec" and moves to rules, then hooks, then a Codex-style sandbox.

## 2. Cargo workspace layout

```
agent-rust/
├── Cargo.toml                 # [workspace]
├── crates/
│   ├── protocol/              # Op, Event, Message, ContentBlock, ToolCall… (serde, no I/O)
│   ├── llm/                   # Provider trait + anthropic, openai-responses, openai-chat-compatible
│   ├── tools/                 # Tool trait + read, write, edit, bash, grep, glob, (apply_patch)
│   ├── core/                  # Session, agent loop, permissions, compaction, context assembly
│   ├── store/                 # JSONL session tree read/write, resume, fork
│   ├── config/                # TOML config layering, AGENTS.md discovery, model catalog
│   ├── tui/                   # ratatui front-end (consumes Event, sends Op)
│   ├── mcp/                   # (later) rmcp client → tools
│   └── sandbox/               # (later) bwrap/landlock/seccomp (linux), seatbelt (mac)
└── src/main.rs  (or crates/cli) # clap: interactive | -p print | --json | rpc
```

**Suggested crates**

| Need | Crate |
|---|---|
| async runtime | `tokio` (full), `tokio-util` (CancellationToken) |
| HTTP + SSE | `reqwest` (stream, rustls), `eventsource-stream` (or `reqwest-eventsource`) |
| JSON | `serde`, `serde_json` |
| tool schemas | `schemars` (derive a JSON Schema from the tool's args struct) |
| CLI | `clap` (derive) |
| TUI | `ratatui`, `crossterm`, `tui-textarea`, `pulldown-cmark`, `syntect` |
| search | `ignore` (gitignore-aware walk, the ripgrep library), `grep-regex` + `grep-searcher`, `globset` |
| diff / patch | `similar` (diffs for previews), `strsim` (Levenshtein for fuzzy edit) |
| config | `toml`, `directories` / `dirs`, `figment` (optional layering) |
| process / pty | `tokio::process`; `portable-pty` later for interactive shells |
| ids / time | `uuid` or `nanoid`, `chrono` / `jiff` |
| errors / logs | `anyhow` (bin), `thiserror` (libs), `tracing`, `tracing-subscriber` |
| MCP (later) | `rmcp` |
| sandbox (later) | `landlock`, `seccompiler`, or shell out to `bwrap` |
| tests | `insta` (snapshots), `wiremock` (fake LLM server), `tempfile`, `assert_cmd` |

## 3. Protocol (`crates/protocol`)

```rust
// ---- Conversation model (provider-neutral) ----
pub enum Role { User, Assistant }

pub enum ContentBlock {
    Text { text: String },
    Thinking { text: String, signature: Option<String> },    // keep signature for Anthropic replay
    Image { media_type: String, data_b64: String },
    ToolCall { id: String, name: String, args: serde_json::Value },
    ToolResult { call_id: String, content: Vec<ContentBlock>, is_error: bool },
}

pub struct Message { pub role: Role, pub content: Vec<ContentBlock> }

// ---- UI -> core ----
pub enum Op {
    UserInput { text: String, images: Vec<PathBuf> },
    Steer { text: String },                 // delivered after the current tool batch (pi/Codex)
    Interrupt,
    ApprovalResponse { request_id: String, decision: Decision }, // Once | Always | Reject
    Compact { focus: Option<String> },
    SetModel { provider: String, model: String },
    Shutdown,
}

// ---- core -> UI ----
pub enum Event {
    TurnStarted { turn_id: String },
    TextDelta { delta: String },
    ThinkingDelta { delta: String },
    ToolCallStarted { call_id: String, name: String, args: serde_json::Value },
    ToolOutputDelta { call_id: String, chunk: String },     // live bash output
    ToolCallFinished { call_id: String, result_preview: String, is_error: bool },
    ApprovalRequested { request_id: String, call_id: String, summary: String, diff: Option<String> },
    Usage { input: u64, output: u64, cache_read: u64, cost_usd: f64 },
    Compacted { summary_tokens: u64 },
    TurnComplete { stop_reason: StopReason },
    Error { message: String, retryable: bool },
}
```

Keep `protocol` free of I/O, and derive `Serialize`/`Deserialize` on everything. `--json` mode then becomes "print each `Event` as a JSON line", and RPC mode becomes "read `Op`s as JSON lines from stdin".

## 4. LLM layer (`crates/llm`)

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn stream(
        &self,
        req: ChatRequest,                 // system, messages, tools (JSON schema), model, max_tokens, reasoning
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, StreamEvent>>;
}

pub enum StreamEvent {                     // normalized, like pi-ai
    TextDelta(String),
    ThinkingDelta { text: String, signature: Option<String> },
    ToolCallStart { id: String, name: String },
    ToolCallArgsDelta { id: String, json_fragment: String }, // accumulate, parse at end
    Done { stop_reason: StopReason, usage: Usage },
    Error { message: String, retryable: bool },              // errors as events, not panics
}
```

**Build order**
1. `anthropic` (Messages API): SSE events `message_start`, `content_block_start`/`delta`/`stop`, `message_delta`, `message_stop`. Add `cache_control` breakpoints on the system prompt and on the last message to get prompt caching.
2. `openai_chat` (Chat Completions compatible). One implementation covers OpenRouter, Groq, DeepSeek, Ollama, LM Studio, vLLM and others.
3. `openai_responses`, for GPT and Codex models with reasoning.

**Also needed**
- A model catalog (context window, prices, supports-images/reasoning), pulled from models.dev JSON and cached on disk.
- Retries with exponential backoff on 429, 5xx and network errors, plus a stream idle timeout.
- A *conversion function* per provider from our `Message` to the wire format. Thinking blocks from another provider become plain text (pi's trick).

## 5. Tools (`crates/tools`)

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;          // schemars::schema_for!(Args)
    fn is_read_only(&self) -> bool { false }        // read-only tools may run in parallel
    async fn call(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolOutput;
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub read_files: Arc<Mutex<HashMap<PathBuf, FileStamp>>>, // read-before-edit tracking
    pub events: mpsc::Sender<Event>,                          // live output
    pub cancel: CancellationToken,
}

pub struct ToolOutput {
    pub for_llm: Vec<ContentBlock>,  // what the model sees
    pub for_ui: Option<String>,      // what the human sees (diff, etc.), like pi
    pub is_error: bool,
}
```

**v1 tool set (pi minimal plus two search tools)**

| Tool | Spec |
|---|---|
| `read` | `{path, offset?, limit?}`. Output is line-numbered (`{n:>6}\t{line}`). Default limit 2000 lines; long lines truncated at 2000 chars. Images are returned as an `Image` block. Records the read in `read_files`. |
| `write` | `{path, content}`. Creates parent directories. An existing file must have been read first. |
| `edit` | `{path, old_string, new_string, replace_all?}`. Requires a prior read. Tries an exact match first, then the fuzzy chain: line-trimmed → whitespace-normalized → indentation-flexible → block-anchor. `old_string` must match exactly once unless `replace_all` is set. The error message tells the model *why* it failed: "found 3 matches, add more context" or "no match; closest candidate at line 42: …". Returns a unified diff. |
| `bash` | `{command, timeout_ms?, description?}`. Runs `bash -lc` in its own process group, so a timeout or Esc can kill the whole group. Merges stdout and stderr, streams `ToolOutputDelta`, and caps the output at about 30k chars, keeping the head and tail. Larger output is saved to a temp file and the model gets its path. Includes the exit code in the result. |
| `grep` | Uses the `ignore` and `grep-searcher` crates: `{pattern, path?, glob?, mode: files|content|count, context?, case_insensitive?, head_limit?}`. |
| `glob` | `{pattern, path?}`, gitignore-aware and sorted by mtime, capped at 100 results with a note when truncated. |

**Later:** `apply_patch` (V4A, for GPT models), `web_fetch`, `todo`/`update_plan`, `task` (subagent), LSP diagnostics after edits, and MCP tools.

**Tool-design rules** from Anthropic's "Writing tools for agents":
- Prefer absolute paths.
- Make error messages actionable.
- Truncate with a hint on how to narrow the query.
- Keep descriptions short but precise; the description *is* the prompt.

## 6. Agent loop (`crates/core`)

```rust
pub async fn run_turn(sess: &mut Session, input: Message, tx: &Sender<Event>, cancel: CancellationToken) -> Result<()> {
    sess.history.push(input);
    loop {
        if sess.needs_compaction() { compact(sess, tx).await?; }

        let req = sess.build_request();                    // system + tools + history (+ reminders)
        let mut stream = sess.provider.stream(req, cancel.child_token()).await?;
        let assistant = accumulate(&mut stream, tx).await?; // forwards deltas as Events
        sess.append(assistant.clone());                     // persist to JSONL immediately

        let calls = assistant.tool_calls();
        if calls.is_empty() || cancel.is_cancelled() { break; }

        // Codex-style scheduler: read-only tools share a read lock, others take the write lock
        let results = execute_tools(sess, calls, tx, &cancel).await; // permission check inside
        sess.append(Message::tool_results(results));        // keep the order the model asked for

        if let Some(steer) = sess.take_steer() { sess.append(steer); }   // queued user message
        if sess.doom_loop_detected() { /* ask the user, or inject a warning */ }
        if sess.step >= sess.max_steps { /* inject "wrap up now" */ }
    }
    tx.send(Event::TurnComplete { .. }).await?;
    Ok(())
}
```

**Runtime shape**
- `Session` runs as a single tokio task that owns the state.
- It receives `Op`s on an `mpsc::Receiver<Op>` and emits `Event`s on an `mpsc::Sender<Event>` (or a `broadcast` channel for multiple UIs).
- `Interrupt` cancels a `CancellationToken` that is passed down to the provider stream and the child processes.

**Invariant to enforce:** every `ToolCall` in history has a matching `ToolResult`. On interrupt or crash-resume, add a synthetic `is_error: true, "interrupted by user"` result (as OpenCode does). Otherwise the provider APIs return an error.

## 7. Context assembly

The order matters for prompt caching. Keep the stable parts first:

1. **System prompt.** Short, pi-style, about 500 tokens: identity, conciseness, "read before editing", "verify with tests" and tool tips.
2. **Environment block:** cwd, OS, shell, date, git branch and status summary. It changes rarely, so place it after the static prompt.
3. **Project instructions:** `AGENTS.md`, then `CLAUDE.md` as a fallback.
   - Load order: the global `~/.config/<agent>/AGENTS.md`, then every directory from the git root down to the cwd, concatenated root-first.
   - Byte budget: 32 KiB.
4. **Tool definitions**, sorted by a fixed order.
5. **History.**
6. **Reminders:** a `<system-reminder>` text block appended to the latest user message. Use it for "file X changed on disk since you read it", the current todo state and the remaining step budget. Keep these out of the system prompt.

**Compaction**
- **Trigger:** when `input_tokens > context_window - reserve`, with reserve = max(16k, 15%).
- **Process:** send the history with a summarization prompt that has fixed sections (Goal, Constraints, Progress, Key decisions, Next steps, Critical context, Files read/modified). The new history is the summary plus the last ~20k tokens kept verbatim. Never split a tool call from its result.
- **Microcompaction (cheap, first):** replace old, large tool outputs with `[output elided – re-run if needed]`.
- Also provide a manual `/compact [focus]`.

## 8. Sessions (`crates/store`)

Pi-style JSONL tree, one file per session:

```
~/.local/share/<agent>/sessions/<cwd-slug>/<timestamp>_<uuid>.jsonl
```

```jsonc
{"type":"header","version":1,"id":"…","cwd":"/home/…","created":"2026-11-17T…","model":"anthropic/claude-sonnet-5-5"}
{"type":"message","id":"a1b2c3d4","parent":null,"role":"user","content":[…],"ts":"…"}
{"type":"message","id":"e5f6a7b8","parent":"a1b2c3d4","role":"assistant","content":[…],"usage":{…}}
{"type":"compaction","id":"…","parent":"…","summary":"…","kept_from":"…"}
{"type":"model_change","id":"…","parent":"…","model":"openai/gpt-…"}
```

- Writes are append-only. Write each entry as soon as it is produced, from a background writer task fed by an mpsc channel (as Codex does).
- The **current branch** is the path from the leaf to the root. Resume means loading the file, picking a leaf and rebuilding the history.
- `--continue` resumes the latest session for this cwd, `--resume <id>` a specific one. `/fork` copies the branch into a new file.

**Undo and snapshots** (phase 2, OpenCode style)
- Use a shadow git repo at `~/.local/share/<agent>/snapshot/<project-hash>`, driven with `git --git-dir=… --work-tree=<cwd> add -A && write-tree` at the start of each step.
- Store the tree hash on the entry. `/undo` restores the files changed since that tree.

## 9. Permissions and safety

Build in phases:

1. **Modes:** `yolo` (pi), `ask` (ask before `write`, `edit` and `bash`; reads are free), and `plan` (read-only tools only).
2. **Rules** in config, evaluated deny → ask → allow:
   ```toml
   [permissions]
   allow = ["bash(cargo test*)", "bash(git status*)", "edit(src/**)"]
   ask   = ["bash(git push*)"]
   deny  = ["read(.env)", "bash(rm -rf *)"]
   ```
   Split compound bash commands with `tree-sitter-bash` (or a simple shell lexer first) and require *every* part to be allowed. Keep a built-in allowlist of read-only commands (`ls`, `cat`, `git status`, `rg`, …). The "Always" answer saves a rule.
3. **Hooks:** run an external command on `pre_tool_use`, `post_tool_use`, `user_prompt_submit` and `stop`. It receives JSON on stdin; exit 2 blocks the action and stderr goes back to the model (Claude Code's contract).
4. **Sandbox** (Linux first): wrap `bash` in `bwrap --ro-bind / / --bind <cwd> <cwd> --ro-bind <cwd>/.git <cwd>/.git --unshare-net --unshare-pid --die-with-parent`. Add `landlock` and `seccompiler` later. On a sandbox denial, ask "retry without sandbox?" (Codex's flow).

## 10. Front-ends

| Mode | How |
|---|---|
| `agent` (interactive) | **ratatui** TUI. Inline viewport, so it keeps the scrollback like pi and Claude Code. Layout: message list, tool cards with a collapsible output, a multi-line input box, and a status line (model, tokens, cost, mode). Enter sends a message, or queues a steer while the agent is working. Esc interrupts. Shift+Tab cycles the permission mode. |
| `agent -p "…"` | Headless. The final text goes to stdout and progress to stderr. Exit code reflects success. |
| `agent -p --json` | Each `Event` printed as a JSON line. Useful for scripts and evals. |
| `agent rpc` | `Op`s as JSONL on stdin and `Event`s as JSONL on stdout, so an editor or GUI can embed it. Later: an HTTP+SSE server (OpenCode style) or ACP. |

**Slash commands:** `/model`, `/compact`, `/clear`, `/resume`, `/fork`, `/undo`, `/cost`, `/mode`, plus user prompt templates from `.agent/commands/*.md` with `$ARGUMENTS`.

## 11. Config

```
~/.config/<agent>/config.toml     # global
<project>/.agent/config.toml      # project (only if trusted)
CLI flags / -c key=value          # highest precedence
```

```toml
model = "anthropic/claude-sonnet-5-5"
small_model = "anthropic/claude-haiku-5-5"   # titles, summaries
permission_mode = "ask"
max_steps = 200

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[providers.openrouter]
kind = "openai_chat"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[[mcp_servers]]            # phase 3
name = "playwright"
command = "npx"
args = ["@playwright/mcp@latest"]
```

## 12. Roadmap

| Milestone | Scope | Done when… |
|---|---|---|
| **M0: Hello loop** | workspace, `protocol`, Anthropic provider with SSE streaming, `read` + `bash` tools, loop, `-p` mode | `agent -p "what files are here?"` runs `bash ls` and answers |
| **M1: Coding tools** | `write`, `edit` (exact plus the fuzzy chain), `grep`, `glob`, read-before-edit, output truncation, parallel read-only tools, cancellation | it can fix a failing test in a small repo end to end |
| **M2: Sessions + context** | JSONL tree store, `--continue`/`--resume`, AGENTS.md loading, env block, prompt caching, usage/cost tracking | resume after Ctrl-C keeps the full history; cache hits show up in usage |
| **M3: TUI** | ratatui inline UI, streaming markdown, tool cards, input box, steer queue, Esc interrupt, slash commands | daily-driver usable |
| **M4: Safety** | permission modes, rules, approval dialog with a diff preview, doom-loop detection | `ask` mode blocks unapproved writes and commands |
| **M5: Long sessions** | auto-compaction + microcompaction, `/compact`, shadow-git `/undo` | a 2-hour session doesn't hit the context limit |
| **M6: More providers** | OpenAI-compatible chat, OpenAI Responses, models.dev catalog, `/model` switching mid-session | the same session continues on a different provider |
| **M7: Extensibility** | hooks, skills (`SKILL.md`, descriptions only until invoked), prompt templates, MCP via `rmcp`, `rpc` mode | – |
| **M8: Sandbox + extras** | bwrap/landlock for bash, `task` subagent, `apply_patch`, LSP diagnostics, formatter after edits, web_fetch | – |

## 13. Pitfalls to plan for

- **Tool-call / result pairing.** Anthropic rejects a history where a `tool_use` has no `tool_result`, and so do the others. Repair it on interrupt and on resume.
- **Streaming tool arguments** arrive as JSON fragments. Accumulate them per call id, and parse only at `content_block_stop` or the finish event. For live UI previews, use a partial-JSON parser.
- **Killing processes.** Spawn bash with `process_group(0)` and kill the group (`killpg`) on timeout or interrupt; otherwise `npm run dev` children survive.
- **Huge outputs.** Always cap them, by bytes and by lines. A `cat` of a minified bundle can blow the context.
- **CRLF, BOM and tabs** in `edit`. Preserve the original line endings and BOM when writing back.
- **Non-UTF-8 files.** Refuse to edit them instead of writing back lossy text.
- **Prompt-cache busting.** Anything dynamic (date with seconds, git status) in the system prompt invalidates the cache on every turn. Put it in reminders.
- **Doom loops.** The same failing call repeated three times or more means you should stop and ask.
- **Thinking-block signatures** (Anthropic) must be replayed unchanged; handle them correctly when switching providers.
- **Tests.** Use `wiremock` to replay recorded SSE streams, so the loop is tested deterministically without spending tokens. Snapshot the TUI with `insta` and a `TestBackend`.

