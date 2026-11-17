# Coding agents: research

How four terminal coding agents work inside: **Claude Code**, **OpenAI Codex CLI**, **OpenCode** and **pi**. The Rust design built on this research is in [ARCHITECTURE.md](ARCHITECTURE.md).

> Researched 2026-11-17 from official docs, the public source repos and blog posts (links in each section and under Sources). All four projects move fast. Details marked *(uncertain)* came from third-party write-ups or could not be confirmed.

---

## One loop under all of them

Every coding agent is the same loop with different features around it:

```
loop {
    response = llm.stream(system_prompt, tools, history)   // stream deltas to UI
    history.push(response)
    if response.tool_calls.is_empty() { break }             // model is done
    results = run_tools(response.tool_calls)                // permission check, sandbox, exec
    history.push(results)                                   // as tool_result messages
    maybe_compact(history)                                  // when near the context limit
}
```

What sets them apart:
- which **tools** they offer and how those tools behave
- **permissions and sandboxing**
- **context management** (memory files, compaction, caching)
- **session persistence**
- **extensibility** (hooks, skills, MCP, plugins)
- **UI and transport** (TUI, headless, server or RPC)

---

## 1. Claude Code (Anthropic)

TypeScript, React and Ink, bundled with Bun into a single binary. Closed source, but documented in detail. Docs: https://code.claude.com/docs

**Loop**
- Uses the Messages API. The model replies with `tool_use` blocks, and each `tool_result` goes back in the next `user` message. The loop ends on a response with no tool calls.
- Optional limits: `maxTurns` and `maxBudgetUsd`.
- Parallel tools: tools marked read-only or concurrency-safe (Read, Glob, Grep, read-only MCP tools) run concurrently, about 10 at a time. Edit, Write and Bash run one after another.
- A message typed mid-turn is queued and delivered after the current tool batch. Esc cancels the running tool.
- Retryable API errors are retried with backoff. Cheap side tasks such as session titles go to a small model.

**Tools** (https://code.claude.com/docs/en/tools-reference)

| Tool | Behaviour worth copying |
|---|---|
| `Read` | Returns line-numbered output (`cat -n` style) with `offset`/`limit`. Caps output at about 25k tokens and says so when it truncates. Reads images and PDFs. Refuses binaries and directories. |
| `Edit` | Exact `old_string` → `new_string` replacement. `old_string` **must be unique** unless `replace_all` is set. The file must have been **read first** in the session. Refuses non-UTF-8 files. |
| `Write` | Writes the whole file. Overwriting an existing file requires a prior read. |
| `Bash` | Each call is a new process, but `cwd` persists. Timeout is 2 min by default, 10 min max. Output over about 30k chars is saved to a file, and the model gets the path plus a preview. Commands can run in the background. |
| `Glob` / `Grep` | Grep wraps ripgrep, with modes `files_with_matches`, `content` and `count`. Glob results are sorted by mtime. Recent versions drop both on Linux and macOS in favour of `find`/`grep` through Bash. |
| `WebFetch` | Converts the page to Markdown, then a **second model call** answers a prompt about it, so the full page never enters the main context. |
| `WebSearch` | Search runs server-side and returns titles and URLs. |
| `Agent` (Task) | A subagent with a fresh context. Only its final report returns to the parent. |
| `TodoWrite` → `Task*` | A visible task list. |
| `ToolSearch` | MCP tool schemas are **deferred**: only their names sit in the context until the model fetches a schema. |

**Permissions**
- Modes: `default` (ask), `acceptEdits`, `plan` (read-only), `auto` (a classifier model reviews actions), `dontAsk`, `bypassPermissions`.
- Rule syntax: `Tool` or `Tool(specifier)`, e.g. `Bash(npm run test *)`, `Read(./.env)`, `Edit(/src/**)`, `WebFetch(domain:x.com)`. Rules are evaluated **deny → ask → allow**.
- Compound Bash commands (`&&`, `|`, `;`) are split, and every part must be allowed. Wrappers such as `timeout` and `nohup` are stripped before matching. Known read-only commands never prompt.
- Settings are layered: managed > CLI > `.claude/settings.local.json` > `.claude/settings.json` > `~/.claude/settings.json`.
- Sandbox: Seatbelt on macOS; bubblewrap plus a network namespace on Linux, with a proxy that only allows listed domains. Released as `@anthropic-ai/sandbox-runtime`.

**Context**
- `CLAUDE.md` files are **concatenated**, not overridden: managed → user → every directory from the root down to the cwd, plus `CLAUDE.local.md`. Subdirectory files load the first time the agent touches that directory. `@path` imports are supported.
- **Auto-compaction** starts at about 80% of the window. History is summarized, `CLAUDE.md` is re-injected, and the most recent files are re-attached. *Microcompaction* clears old tool outputs.
- **Prompt caching:** the system prompt, tools and CLAUDE.md form a stable prefix. Dynamic information goes into `<system-reminder>` blocks inside *user* messages so the cached prefix is not invalidated.

**Extensibility**
- **Hooks:** about 30 events (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SessionStart`, `PreCompact`, …). A hook gets JSON on stdin; **exit 2 blocks** the action and stderr is fed back to the model. JSON output can allow, deny or ask, rewrite the input, or add context.
- **Skills:** `skills/<name>/SKILL.md` with frontmatter. Only the `description` is in the context, and the body loads when the skill is invoked (progressive disclosure). Slash commands have been merged into skills.
- **Subagents:** `.claude/agents/*.md`, with fields `tools`, `model`, `permissionMode`, `isolation: worktree`.
- **MCP** over stdio, http, sse and ws; tools are named `mcp__server__tool`.
- **Plugins** bundle all of the above, under `.claude-plugin/plugin.json`.

**Sessions and headless mode**
- Each session is append-only JSONL at `~/.claude/projects/<cwd-slug>/<id>.jsonl`. Entries carry `uuid` and `parentUuid`, so history is a tree. `--resume` and `--continue` reopen a session.
- Files are snapshotted before every edit, which powers rewind (Esc Esc).
- `claude -p` prints a result non-interactively. Output formats: `text`, `json` and `stream-json`, which starts with `system/init` and ends with `result`. The Agent SDKs (TS and Python) run the CLI as a subprocess and talk to it in stream-json.

---

## 2. OpenAI Codex CLI: **written in Rust, the closest reference for us**

Repo: https://github.com/openai/codex, workspace `codex-rs/`, about 180 crates, edition 2024.

**Crate layering**
```
protocol      wire types: Op (submission) / Event{id, msg: EventMsg}, SandboxPolicy, AskForApproval
core          session + turn loop, model client, tools, compaction, AGENTS.md, exec, MCP calls
codex-api     Responses API HTTP client (SSE + optional WebSocket)
config        ~/.codex/config.toml, profiles, providers
rollout       session JSONL + SQLite index
apply-patch   patch-format parser/applier (also its own binary)
sandboxing    Seatbelt / bwrap / Landlock policy builders
linux-sandbox helper binary (reached via arg0 dispatch of the main binary)
execpolicy    .rules: allow / prompt / forbid command prefixes
rmcp-client   MCP client (official `rmcp` SDK)
app-server    JSON-RPC front door (stdio / websocket / unix socket)
tui, exec, cli  front-ends; the TUI and exec run an in-process app-server
```

**Main dependencies:** `tokio`, `reqwest`, `eventsource-stream`, `ratatui` + `crossterm`, `serde`, `toml`/`toml_edit`, `clap`, `landlock`, `seccompiler`, `rmcp`, `similar`, `tree-sitter`, `tracing`, `ts-rs`, `pulldown-cmark` + `syntect`, and `insta` + `wiremock` for tests.

**Protocol (SQ/EQ pattern)**
- Clients send `Op`s into a submission queue: `UserTurn`, `Interrupt`, `ExecApproval`, `PatchApproval`, `Compact`, `Shutdown`, …
- Core emits `Event`s on an event queue (about 90 `EventMsg` variants): `TurnStarted`, `AgentMessageDelta`, `ExecCommandBegin`/`OutputDelta`/`End`, `ExecApprovalRequest`, `PatchApplyBegin`/`End`, `TurnDiff`, `PlanUpdate`, `TokenCount`, `TurnComplete`, …
- Every UI (TUI, exec, IDE, desktop) is a consumer of this stream, which decouples the UI from core.

**Loop** (`core/src/session/turn.rs`)
- Uses only the Responses API. `store=false`, the encrypted reasoning content is sent back each turn, and `prompt_cache_key` is set to the session id.
- `run_turn` may compact first. It then runs `run_sampling_request` (retrying the stream up to `stream_max_retries`) and repeats while the model emitted tool calls **or the user queued a "steer" message**.
- Tool execution: each call is a `tokio::spawn`, and all calls share an `Arc<RwLock<()>>`. **Tools that can run in parallel take the read lock and the rest take the write lock**, which is a neat, simple scheduler. Cancellation uses `tokio::select!`.

**Tools**
- `exec_command` (shell, optionally PTY-backed) plus `write_stdin` for interactive sessions. Older versions had `shell{command: [argv], workdir, timeout_ms}`.
- `update_plan{explanation?, plan: [{step, status}]}` only emits an event for the UI.
- `view_image`, plus MCP tools.
- `apply_patch` in the **V4A patch format**. It is sent as a freeform tool with a grammar, so the patch is not wrapped in JSON:
  ```
  *** Begin Patch
  *** Add File: src/hello.rs
  +pub fn hi() {}
  *** Update File: src/lib.rs
  *** Move to: src/core.rs
  @@ fn main() {
       let x = 1;
  -    println!("{}", x);
  +    println!("x={}", x);
   }
  *** Delete File: old.txt
  *** End Patch
  ```
  The applier finds each hunk with `seek_sequence` in four passes: exact match, then trailing whitespace stripped, then fully trimmed, then Unicode punctuation normalized.

**Sandbox and approvals**
- Modes: `read-only`, `workspace-write` (writable roots, network off by default) and `danger-full-access`.
- Approval policies: `untrusted`, `on-request`, `never`, `granular`. There is also an optional reviewer agent ("guardian").
- Linux: bubblewrap mounts `/` read-only, binds the writable roots back in, keeps `.git` read-only, and unshares the user, pid and net namespaces. Then `PR_SET_NO_NEW_PRIVS` is set and a seccomp filter blocks network syscalls and `io_uring`. Landlock plus seccomp was the original design and is now legacy.
- macOS: `sandbox-exec -p <policy>`, with the policy assembled from `.sbpl` fragments.
- Flow: decide whether approval is needed → run in the sandbox → if the sandbox denies it, ask "retry without sandbox?" → rerun unsandboxed.

**Config and context**
- `~/.codex/config.toml` with profiles and `[model_providers.x]` (`base_url`, `env_key`, retries, timeouts). Project `.codex/config.toml` loads only for trusted projects.
- AGENTS.md: a global file, then a walk from the git root down to the cwd, taking `AGENTS.override.md` or `AGENTS.md` in each directory. Files are joined root-first, within a 32 KiB budget.
- Compaction: the model summarizes the history. The new history is the base instructions, recent user messages (up to 20k tokens) and the summary. On context overflow it drops the oldest item and retries.
- Rollouts: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`, written by a background task through a bounded mpsc channel.
- `codex exec`: the final message goes to stdout and progress to stderr. `--json` emits JSONL events; `--output-schema` is also available.

> Tip from the research: for learning, the **2025-era codex-rs** (about v0.20–0.40 in git history) is a cleaner template. It had `core` with an async-channel SQ/EQ, `mcp-client`, a Landlock+seccomp sandbox and Chat Completions support, without the size of the current app-server.

---

## 3. OpenCode (anomalyco, formerly sst)

Repo: https://github.com/anomalyco/opencode. TypeScript on Bun, MIT-licensed. Two lines exist: **V1** (`packages/opencode`) and **V2**, an Effect-based rewrite.

**Architecture: client/server first**
- `opencode` starts a headless **HTTP server** and a **TUI client** that talks to it over HTTP. Events stream over **SSE** (`GET /event`).
- The OpenAPI 3.1 spec is served at `/doc`, and the SDKs are generated from it. The TUI, desktop app, web UI, the VS Code extension and ACP clients (Zed) all use this API.
- The TUI started in Go with Bubble Tea and was rewritten in v1.0 on **OpenTUI** (a Zig core with SolidJS bindings).
- V2 adds a **durable, sequence-numbered event log** per session alongside the lossy live stream, so a client can reconnect and catch up.

**Providers**
- V1 uses the Vercel AI SDK plus the **models.dev** catalog for model metadata (context size, prices, capabilities). Provider packages are installed on demand.
- V2 has its own LLM client covering about 35 providers and wire protocols.
- Auth: API keys, ChatGPT sign-in, Copilot device OAuth, AWS and GCP credential chains, local Ollama and LM Studio.

**Data model and loop**
- A Session holds Messages (`user` or `assistant`), and each Message holds **Parts**: `text`, `reasoning`, `tool` (a state machine `pending → running → completed | error`), `file`, `step-start`, `step-finish`, `patch`, `compaction`, `subtask`.
- Interrupted tool calls are replayed as errors, so the history never contains a tool call without a result.
- Storage moved from JSON files to **SQLite with Drizzle** (`opencode.db`).
- **Doom-loop detection:** three identical tool calls in a row trigger a permission ask.
- Max steps are set per agent; the last step gets a "wrap up" prompt.

**Snapshots and undo**
- A **shadow git repo** at `<data>/snapshot/<project>/<hash>`, used through `--git-dir` and `--work-tree`. It borrows objects from the real repo through `objects/info/alternates` and never touches the user's `.git`.
- A snapshot (`add --all` + `write-tree`) is taken at the start of each step. At the end of the step, a `patch` part records which files changed.
- Undo and redo restore with `checkout <tree> -- file`.

**Tools**
- `bash`, `read` (which also lists directories), `glob`, `grep`, `task`, `webfetch`, `todowrite`, `skill`, `edit`/`write`, or `apply_patch` for GPT models. Optional: `websearch`, `lsp`, `question`.
- **Fuzzy edit fallback chain**, adapted from cline and gemini-cli: Exact → LineTrimmed → BlockAnchor (first and last lines plus Levenshtein on the middle) → WhitespaceNormalized → IndentationFlexible → EscapeNormalized → TrimmedBoundary → ContextAware → MultiOccurrence.
- **After every edit:** show a diff preview for the permission ask → write → run the **formatter** → run **LSP diagnostics** and append the errors to the tool output ("LSP errors detected… please fix").

**Agents and permissions**
- Primary agents are `build` (full access) and `plan` (no edits). Subagents are `general` and `explore`. Hidden agents handle `compaction`, `title` and `summary`.
- Agents are defined as markdown files with frontmatter (`description`, `mode`, `model`, `permission`, `steps`), or in JSON.
- Permissions are `allow`, `ask` or `deny` per tool, with glob patterns for bash. V2 uses an ordered rule list where the last match wins.
- Bash is parsed with **tree-sitter-bash** so compound commands are checked part by part.
- Answers to an ask: `once`, `always` (saved as an allow rule) or `reject`.

**Config:** `opencode.json(c)`, layered system → global → project, with `{env:X}` and `{file:...}` substitution. Also AGENTS.md (with CLAUDE.md as a fallback), markdown commands with `$ARGUMENTS` and `` !`cmd` ``, MCP over stdio and HTTP with OAuth, JS plugins with about 20 hooks, and formatters picked by file extension.

---

## 4. pi (Mario Zechner, now under earendil-works)

Repo: https://github.com/earendil-works/pi (formerly `badlogic/pi-mono`). Blog: https://mariozechner.at/posts/2025-11-30-pi-coding-agent/. TypeScript. **Minimal by design.**

**Philosophy:** "if I don't need it, it won't be built."
- The **system prompt plus tool definitions is under 1000 tokens**, against about 10k in other harnesses. Frontier models are already trained on agent behaviour and don't need long instructions.
- **Four tools:** `read`, `write`, `edit` and `bash`. `grep`, `find` and `ls` exist but are off by default.
- **No MCP.** Its tool schemas eat context: Playwright MCP is about 13.7k tokens. Pi prefers CLI tools with READMEs that the agent reads only when needed.
- **No todos, plan mode or subagents.** Plans go in `PLAN.md` or `TODO.md`, which are visible and versioned. Sub-agents are black boxes.
- **No background bash:** use tmux, so the human can watch and attach.
- **YOLO:** no permission prompts and no sandbox. Pi calls prompts "security theater" and says to use a container if that is a concern. A project-trust prompt was added later to gate project-local extensions.
- **Observability:** every bit of state lives in files, the session format is documented, and sessions export to HTML.

**Packages**
- `pi-ai`: a single LLM API across `anthropic-messages`, `openai-responses`, `openai-completions`, `google-generative-ai`, `bedrock-converse-stream` and `mistral-conversations`.
  - Stream events: `start`, `text_*`, `thinking_*`, `toolcall_*`, `done`, `error`. **Errors are events, not exceptions**, and an abort returns the partial content with `stopReason: "aborted"`.
  - Unified reasoning levels: `minimal` through `max`.
  - **Cross-provider handoff:** thinking blocks from one provider become `<thinking>` text for another. The context is plain JSON, so a session can resume on a different model.
  - Tracks tokens and cost. The model registry is generated from models.dev.
- `pi-agent-core`: the agent loop with events `agent_start → turn_start → message_start/update/end → tool_execution_start/update/end → turn_end → agent_end`.
  - Tools run in parallel by default, and results come back in the order the model requested them.
  - `beforeToolCall` can block a call; `afterToolCall` can patch the result.
  - Queues: `steer()` (delivered after the current tools) and `followUp()` (delivered after the agent finishes).
- `pi-tui`: its own **differential renderer**, not Ink.
  - Keeps the terminal scrollback instead of taking over the full screen.
  - Components implement `render(width) → lines` and cache their output once finished.
  - Redraws from the first changed line, wrapped in **synchronized output** (`CSI ?2026h … ?2026l`) to avoid flicker.
- `pi-coding-agent`: the CLI. Modes: interactive, `-p` (print), JSON, RPC and an SDK.

**Sessions:** JSONL at `~/.pi/agent/sessions/--<cwd>--/<ts>_<uuid>.jsonl`. **Tree-shaped:** each entry has an `id` and a `parentId`, and branches live in the same file. `/tree` jumps to any point and summarizes the abandoned branch; `/fork` and `/clone` create new files.

**Compaction:** starts when the context exceeds the window minus 16k. It keeps about 20k recent tokens and never splits a tool call from its result. The summary has fixed sections (Goal, Constraints, Progress, Key Decisions, Next Steps, Critical Context) plus lists of files read and modified.

**Extensions:** TypeScript files with about 30 events (`tool_call` can block a call, and `context` can rewrite the messages before each LLM call). They can `registerTool`, `registerCommand` and `registerProvider`. Features such as permission gates and sub-agents are left to extensions. Also: skills (`SKILL.md`), prompt templates and `SYSTEM.md` / `APPEND_SYSTEM.md`.

---

## 5. Comparison

| | Claude Code | Codex CLI | OpenCode | pi |
|---|---|---|---|---|
| Language | TS (Bun) | **Rust** | TS (Bun) | TS (Node) |
| UI | React/Ink TUI | ratatui | OpenTUI (Solid) | custom differential TUI |
| Front-end ↔ core | in-process, plus stream-json | **Op/Event queues**, JSON-RPC app-server | **HTTP + SSE server** | in-process events, plus RPC mode |
| Providers | Anthropic | OpenAI Responses (configurable base URL) | ~35 via models.dev | ~6 wire APIs, many providers |
| Edit style | exact unique string replace | `apply_patch` (V4A) | string replace + **fuzzy fallback chain**, or apply_patch | exact string replace |
| Post-edit feedback | LSP diagnostics | – | **formatter + LSP diagnostics** | – |
| Permissions | modes + deny/ask/allow rules + classifier | approval policy + execpolicy rules | allow/ask/deny per tool or agent | none (YOLO) |
| Sandbox | Seatbelt / bwrap + proxy | Seatbelt / **bwrap + seccomp** | none | none |
| Memory file | CLAUDE.md (hierarchical) | AGENTS.md | AGENTS.md (+ CLAUDE.md) | AGENTS.md / CLAUDE.md |
| Sessions | JSONL tree (uuid / parentUuid) | JSONL rollouts + SQLite index | SQLite | **JSONL tree, branches in one file** |
| Undo | file snapshots per edit | turn diff | **shadow git repo** | /tree branching |
| Subagents | yes | yes (multi-agent tools) | yes (task) | no (by design) |
| MCP | yes (deferred schemas) | yes (rmcp) | yes | no (by design) |
| Hooks/plugins | shell hooks, skills, plugins | hooks, skills | JS plugins | TS extensions |
| System prompt | large | medium | medium | **< 1k tokens** |


---

## 6. Sources

- Anthropic: *Building effective agents*: https://www.anthropic.com/engineering/building-effective-agents
- Anthropic: *Writing tools for agents*: https://www.anthropic.com/engineering/writing-tools-for-agents
- Anthropic: *Effective context engineering*: https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- Claude Code docs: https://code.claude.com/docs (tools-reference, permissions, hooks, skills, memory, agent-sdk/agent-loop)
- Codex source: https://github.com/openai/codex/tree/main/codex-rs (`protocol/src/protocol.rs`, `core/src/session/turn.rs`, `core/src/tools/`, `apply-patch/`, `linux-sandbox/`)
- OpenCode source: https://github.com/anomalyco/opencode (`packages/opencode/src/session/{prompt,processor}.ts`, `tool/edit.ts`, `snapshot/index.ts`)
- pi: https://mariozechner.at/posts/2025-11-30-pi-coding-agent/ and https://github.com/earendil-works/pi (`docs/session-format.md`, `docs/compaction.md`)
- Armin Ronacher on pi: https://lucumr.pocoo.org/2026/1/31/pi/
