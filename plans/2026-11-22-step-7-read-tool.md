# Plan for Step 7 — Read tool

- Date: 2026-11-22
- Domain: Rust CLI, filesystem tools, and Chat Completions integration
- Author: plan-from-spec, reviewed with the repository user
- Status: approved

## Scope and decisions

Implement Step 7 before Step 6 in the existing crate. Add a dedicated `read`
tool alongside `bash`, using Chat Completions, the existing history shape,
and sequential execution. Bash guardrails, `write`, and `edit` remain deferred.

Relative paths use the CLI working directory. Absolute paths, parent traversal,
normal OS permissions, and symlinks to regular files are allowed. Accept only
UTF-8 regular files without NUL bytes. Read errors are recoverable tool results;
invalid batch metadata, unknown tool names, and malformed Bash arguments remain
fatal before any tool executes.

## Contracts

- `path`: required nonempty string, passed to the filesystem as supplied.
- `offset`: optional positive integer, default 1, using absolute line numbers.
- `limit`: optional integer from 1 through 2,000, default 2,000; reject larger limits.
- Each line uses `{number:>6}\t{text}\n`. Strip LF or CRLF terminators and retain
  at most 2,000 Unicode scalar values of file text per line.
- When more lines remain, append
  `[showing lines START-END of TOTAL, use offset=NEXT to read more]\n`.
- Empty files and offsets beyond EOF return successful explanatory messages.
- Failures start with `Error: read ...` and distinguish invalid arguments,
  missing files, directories, non-regular files, inspection/open/read failures,
  invalid UTF-8, and NUL bytes.
- Print one read diagnostic line on stderr. File contents enter tool history;
  streamed assistant answers go to stdout.

## Implementation tasks

1. Add `src/read.rs` for argument validation, path handling, and formatting.
   Scan sequentially to EOF with a reusable line buffer, validate the entire
   file, retain only the formatted page, and count exact totals.
2. Add `src/tools.rs` with one stable ordered registry (`bash`, then `read`),
   schemas, and dispatch. Use that registry in `src/openai.rs` to validate names
   while preserving Bash argument preflight validation. In `src/main.rs`, await
   calls by index and append the completed assistant message followed by all
   matching tool results.
3. Add reader unit tests and CLI subprocess tests using local HTTP mocks and
   throwaway directories. Update the exact tools schema assertion and replace
   the old unknown-tool `read` fixture with `unknown`.
4. After validation passes, mark all five Step 7 items in `PLAN.md` complete
   and record verification with the plan link. Keep Step 6 deferred.

## Validation and acceptance

Run `cargo fmt --check`, `cargo build --locked`, and `cargo test --locked`.
Report mock validation and whether a live provider was used.

Unit tests cover defaults, numeric boundaries, object shape, line endings,
pagination, empty files, out-of-range offsets, Unicode truncation, and binary
bytes beyond the requested page. Subprocess tests assert both tool schemas,
mixed tool ordering, relative and absolute paths, result IDs, recovery after
read errors, stdout, stderr diagnostics, and Unix symlink behavior. Existing
Bash schema, batch validation, execution, and output tests must keep passing.

A scripted mock requests `read`, receives numbered contents, requests another
page or retries after a failure, then answers. Each page obeys the line and
character caps and includes the next offset and total when more lines remain.

## Risks

Exact totals and whole-file validation require a full scan for every page.
One very long line can grow the reusable input buffer; retained page size is
bounded. Local mocks script tool selection; model preference for `read` over
`cat` requires a live check if requested.
