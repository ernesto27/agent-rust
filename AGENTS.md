# Repository guide

This project is a terminal coding agent written in Rust (edition 2024). Build it
incrementally, starting with a minimal OpenAI-compatible client and adding the
agent loop and tools in working steps.

## Design references

Read these documents before making implementation or design decisions:

- [PLAN.md](PLAN.md): current implementation scope, step order, acceptance checks,
  and progress. Follow this plan for the work being implemented.
- [ARCHITECTURE.md](ARCHITECTURE.md): long-term architecture, component boundaries,
  protocol, tools, sessions, and future UI and provider layers.
- [AGENT_DESIGN.md](AGENT_DESIGN.md): research on Claude Code, Codex CLI, OpenCode,
  and pi, explaining the ideas behind the architecture. Treat it as background;
  check current primary sources before relying on version-sensitive details.

The plan deliberately starts smaller than the architecture. When their scope
differs, follow PLAN.md for the current step and use ARCHITECTURE.md to guide
future evolution. In particular, keep one crate for now; the proposed workspace
is a future design.

## Implementation rules

- Complete and verify the requested step before starting another. Mark its
  checklist items complete in PLAN.md only after checking the behavior.
- Keep the core small: a short system prompt and eventually four tools (`read`,
  `write`, `edit`, `bash`). Add features and dependencies when the current step
  needs them.
- Keep code in `src/main.rs` until it grows past roughly 300 lines, then split
  into focused modules.
- Use OpenAI Chat Completions with a configurable base URL for the initial phases.
  Introduce streaming, other APIs, and provider abstractions at their planned steps.
- Use typed serde structures for the request and response fields needed by the
  current behavior. Return clear errors with context using `anyhow`.
- Keep successful answer text on stdout and errors on stderr. Exit nonzero on
  failure, and include the API response body for unsuccessful HTTP statuses.
- Never print or commit API keys. Read configuration from the environment.

## Configuration and running

- `OPENAI_API_KEY`: required and nonblank.
- `OPENAI_BASE_URL`: defaults to `https://api.openai.com/v1`.
- `OPENAI_MODEL`: defaults to `gpt-4.1`.

With the key exported, run the current one-question CLI with:

```bash
cargo run -- "say hi"
```

## Validation

```bash
cargo fmt --check
cargo build --locked
cargo test --locked
```

Check behavior relevant to the change. For HTTP changes, prefer a local mock
server to verify request paths, authorization, JSON, response parsing, and error
handling without paid API calls. State whether validation used mocks or a live
provider. Test agent tool execution in a throwaway folder or container.

Keep `Cargo.lock` tracked for this binary application and `/target` ignored.
