# CLAUDE.md

> **Note**: This file serves dual duty as both `AGENTS.md` (the real file) and `CLAUDE.md` (symlink → `AGENTS.md`).
> Edit `AGENTS.md` directly; `CLAUDE.md` stays in sync automatically.

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Rust API proxy exposing free DeepSeek model endpoints. Translates standard OpenAI-compatible and Anthropic-compatible requests to DeepSeek's internal protocol with account pool rotation, PoW challenge handling, and streaming response support.

Requires Rust **1.95.0** (pinned in `rust-toolchain.toml`) with **edition 2024**.

Key dependencies and why they matter:
- `wasmtime` — executes DeepSeek's PoW WASM solver; the entire PoW system depends on this
- `tiktoken-rs` — client-side prompt token counting (DeepSeek returns 0 for `prompt_tokens`)
- `pin-project-lite` — underpins every streaming response wrapper (`SseStream`, `StateStream`, etc.)
- `axum` / `reqwest` — HTTP server and client respectively
- `tokio` with `signal` feature — async runtime with graceful shutdown on SIGTERM/SIGINT

## Principles

### 1. Single Responsibility
- `config.rs`: Configuration loading only, no client creation or business logic
- `client.rs`: Raw HTTP calls only, no token caching, retry, or SSE parsing
- `accounts.rs`: Account pool management only, no network requests
- `pow.rs`: WASM computation only, no account management or request sending
- `server/handlers.rs`: Route handling only, delegates to OpenAIAdapter / AnthropicCompat
- `server/stream.rs`: SSE response body only, no business logic
- `server/error.rs`: Error mapping only, no business logic
- `anthropic_compat.rs`: Protocol translation only, no direct ds_core access

### 2. Minimal Viable
- No premature abstractions: Extract traits/structs when needed, not before
- No redundant code: Remove unused imports, avoid over-documenting, no pre-written tests
- Delay dependency introduction: only add deps when actually needed

### 3. Control Complexity
- Explicit over implicit: Dependencies injected via parameters, no global state
- Composition over inheritance: Small modules composed via functions, no deep inheritance
- Clear boundaries: Modules interact via explicit interfaces, no internal logic leakage

## Architecture

### Module Structure

**Core:**
- `main.rs` — thin binary wrapper (~10 lines): init logger, parse CLI, load config, run server
- `lib.rs` — public API surface: exports `Config`, `DeepSeekCore`, `ChatResponse`, `OpenAIAdapter`, `ChatResult`, `AnthropicCompat`, etc.
- `config.rs` — config loader (`-c` flag, `config.toml` default). See `config.example.toml` for all fields.

**ds_core/ (DeepSeek facade + implementation):**
- `ds_core.rs` — facade: `DeepSeekCore`, `CoreError`; declares submodules
- `accounts.rs` — account pool: init validation, idle-aware selection
- `pow.rs` — PoW solver: WASM loading, DeepSeekHashV1 computation
- `completions.rs` — chat orchestration: SSE streaming, account guard, file upload
- `client.rs` — raw HTTP client: API endpoints, envelope parsing, zero business logic

**openai_adapter/ (OpenAI protocol adapter):**
- `openai_adapter.rs` — facade: `OpenAIAdapter`, `OpenAIAdapterError`, `StreamResponse`
- `types.rs` — OpenAI request/response structs (`ChatCompletionsRequest`, etc.)
- `models.rs` — model list/get endpoints
- `request/` — request pipeline: normalize → tools → files → prompt → resolver → tiktoken
- `response/` — response pipeline: sse_parser → state → converter → tool_parser → StopStream

**anthropic_compat/ (Anthropic protocol translator):**
- Sits on top of `openai_adapter`, no direct `ds_core` access
- `request.rs` — Anthropic → OpenAI request mapping
- `response/aggregate.rs` — non-streaming OpenAI JSON → Anthropic JSON
- `response/stream.rs` — streaming OpenAI SSE → Anthropic SSE events

**server/ (HTTP server):**
- `server.rs` — facade: axum router, auth middleware, graceful shutdown
- `handlers.rs` — route handlers for OpenAI + Anthropic endpoints
- `stream.rs` — `SseBody`: wraps `StreamResponse` into `axum::body::Body`
- `error.rs` — `ServerError`: OpenAI-compatible error JSON responses

**Additional:**
- `config.example.toml` — authoritative configuration reference
- `examples/adapter_cli.rs` + `examples/adapter_cli/` — debug CLI + JSON request samples (all features)
- `py-e2e-tests/` — Python e2e test suite (pytest + uv, JSON-driven scenarios)
- `docs/` — `code-style.md`, `logging-spec.md`, `deepseek-prompt-injection.md`, `deepseek-api-reference.md`

### Facade Module Pattern

`ds_core.rs`, `openai_adapter.rs`, `server.rs`, `request.rs`, `response.rs`, and `anthropic_compat.rs` are **facades**:
- They declare submodules with `mod` (keeping implementation private)
- They re-export only the minimal public interface via `pub use`
- They sometimes contain `#[cfg(test)]` test modules

This means the file tree does not directly map to the public API. To understand what a module exposes externally, read its facade file, not the directory listing.

### Binary / Library Split

- `main.rs` is a thin binary wrapper (~10 lines): init `env_logger`, parse CLI args, load config, call `server::run()`
- `lib.rs` defines the public API surface: `Config`, `DeepSeekCore`, `CoreError`, `ChatRequest`, `ChatResponse`, `AccountStatus`, `OpenAIAdapter`, `OpenAIAdapterError`, `ChatResult`, `StreamResponse`, `AnthropicCompat`
- The crate can be built as both a library (`cargo build --lib`) and a binary (`cargo build --bin ds-free-api`)

### StreamResponse Type

`StreamResponse` is the unifying bridge between adapter layers and the HTTP server:
- Every adapter's streaming method returns `StreamResponse` (a boxed `Stream<Item = Result<Bytes>> + Send`)
- `server/stream.rs::SseBody` wraps `StreamResponse` and converts it into an `axum::body::Body`
- This decouples the adapters from the HTTP framework — they produce bytes, the server handles SSE framing

## Key Architectural Patterns

### Account Pool Model
1 account = 1 session = 1 concurrency. Scale via more accounts in `config.toml`. `AccountGuard` marks an account as `busy` and releases on `Drop`.

### Request Flow (per-chat)
`v0_chat()` → `get_account()` → `split_history()` → `create_session()` → `upload_files()` → `compute_pow()` → `completion()` → `parse_ready()` → `GuardedStream`

Each `v0_chat()` call creates a dedicated session, uploads multi-turn history as files, then streams the response. The session is destroyed when the stream ends.

### Single-Struct Pipeline (OpenAI)
The adapter uses a **single struct** (`ChatCompletionsRequest`) through the entire request pipeline — no intermediate types:

```
ChatCompletionsRequest
  → normalize::apply |
  → tools::extract   |  reads ChatCompletionsRequest fields directly
  → files::extract   |
  → prompt::build    |
  → resolver::resolve|
  → tiktoken
  → try_chat (ds_core::ChatRequest)
  → if req.stream → ChatCompletionsResponseChunk | else → ChatCompletionsResponse
```

### Response Pipeline (OpenAI)
```
ds_core SSE bytes → SseStream (sse_parser) → StateStream (state/patch machine) → ConverterStream (converter) → ToolCallStream (tool_parser) → StopStream (stop sequences) → SSE bytes
```

All stream wrappers use `pin_project_lite::pin_project!` macro and implement `Stream` with `poll_next`.

### GuardedStream & Account Lifecycle
`AccountGuard` marks an account as `busy` and releases on `Drop`. `GuardedStream` wraps the SSE stream with an `AccountGuard`, holding the account busy until the stream is consumed or dropped. Drop always calls `delete_session`; on abnormal disconnect it also calls `stop_stream`. Sessions tracked in `active_sessions: Arc<Mutex<HashMap<String, ActiveSession>>>`.

### Account Initialization Flow
`AccountPool::init()` spins up accounts concurrently (capped at 13 via `tokio::sync::Semaphore`):
1. `login` — obtain Bearer token
2. `create_session` — create chat session
3. `health_check` — test completion (with PoW) to verify writable session
4. `update_title` — rename session to "managed-by-ai-free-api"

### History Splitting & File Upload
Multi-turn conversations split history at `split_history_prompt()`: the last user+assistant pair + final user message go inline; earlier turns are wrapped in `[file content begin]`/`[file content end]` markers and uploaded as `EMPTY.txt`. External files upload individually first with a separate PoW computation targeting `/api/v0/file/upload_file`.

### Capability Toggles
Request fields mapped in `request/resolver.rs`:
- **Reasoning**: defaults to `"high"` (on). Set `"none"` to disable.
- **Web search**: `web_search_options` enables; omitted = off. See `docs/deepseek-api-reference.md` for details.
- **File upload**: data URL content parts → auto upload to session; HTTP URLs → search mode.

### Tool Calls via XML
Tool definitions injected as natural language into the prompt inside a `<think>` block. Response `<tool_calls>` XML parsed back into structured JSON. Primary tag: `<tool_calls>` (plural); configurable fallback tags via `TagConfig.extra_starts`/`extra_ends` in `config.toml`. `arguments` field normalized to always be a JSON string. See `docs/deepseek-prompt-injection.md` for the prompt injection strategy research.

### Anthropic Compatibility Layer
Pure protocol translator on top of `openai_adapter`:
- Request: `Anthropic JSON → to_openai_request() → OpenAIAdapter::chat_completions() / try_chat()`
- Response: `OpenAI SSE/JSON → from_chat_completion_stream() / from_chat_completion_bytes() → Anthropic SSE/JSON`
- ID mapping: `chatcmpl-{hex}` → `msg_{hex}`, `call_{suffix}` → `toolu_{suffix}`
- `ToolUnion` in `request.rs` defaults to `Custom` type when absent (backward compat with Claude Code)

### Error Translation Chain
Errors propagate upward with translation at module boundaries:
1. `client.rs`: `ClientError` (HTTP, business errors, JSON parse)
2. `accounts.rs`: `PoolError` (`ClientError` | `PowError` | validation errors)
3. `ds_core.rs`: `CoreError` (`Overloaded` | `ProofOfWorkFailed` | `ProviderError` | `Stream`)
4. `openai_adapter.rs`: `OpenAIAdapterError` (`BadRequest` | `Overloaded` | `ProviderError` | `Internal`)
5. `anthropic_compat.rs`: `AnthropicCompatError` (`BadRequest` | `Overloaded` | `Internal`)
6. `server/error.rs`: `ServerError` (`Adapter` | `Unauthorized` | `NotFound`)

`client.rs` parses DeepSeek's wrapper envelope `{code, msg, data: {biz_code, biz_msg, biz_data}}` via `Envelope::into_result()`.

### Overloaded Retry
`OpenAIAdapter::try_chat()` retries up to **6 times** with **exponential backoff** (1s → 2s → 4s → 8s → 16s) on `CoreError::Overloaded`, triggered by DeepSeek's `rate_limit_reached` SSE hint or all accounts busy.

### Request Tracing & Account Header
Each request gets a `req-{n}` ID at the handler, threaded through adapter → ds_core. Key log points carry `req=` for cross-layer tracing:
```bash
RUST_LOG=debug 2>&1 | grep 'req=req-1'
```
The `x-ds-account` HTTP response header carries the account identifier upstream.

### HTTP Routes
**OpenAI-compatible:** `GET /`, `POST /v1/chat/completions`, `GET /v1/models`, `GET /v1/models/{id}`
**Anthropic-compatible:** `POST /anthropic/v1/messages`, `GET /anthropic/v1/models`, `GET /anthropic/v1/models/{id}`
Optional Bearer auth via `[[server.api_tokens]]` in config; no auth when empty.

### Model ID Mapping
`model_types` in `[deepseek]` config (default: `["default", "expert"]`) maps to OpenAI model ID `deepseek-{type}` (e.g., `deepseek-default`, `deepseek-expert`). Anthropic compat uses the same IDs.

## Troubleshooting

| Issue | Symptom | Likely Cause / Fix |
|-------|---------|--------------------|
| WASM load failure | `PowError::Execution` on startup | DeepSeek recompiled WASM and changed export ordering. Check `__wbindgen_export_0` symbol in `pow.rs` or update `wasm_url` in `config.toml` |
| Account init failure | All accounts stuck in init | Bad credentials (login fails first) or rate-limited (too many sessions). Check `[accounts]` in config |
| Tool call parse failure | No `tool_calls` in response, raw XML visible | Model output a tag variant not in the parse list. Add fallback `extra_starts`/`extra_ends` in `config.toml` `[deepseek]` |
| Rate limited | Repeated `CoreError::Overloaded` | Add more accounts or reduce concurrency. 6x exponential backoff handles transient spikes |
| Session errors mid-stream | `invalid message id`, session not found | Usually handled by `GuardedStream::drop` cleanup. If persistent, check concurrent access to same account |
| Streaming stalls | No SSE events after initial connection | Check `RUST_LOG=adapter=trace,ds_core::accounts=debug,info` for where the pipeline halts |

## Where to Look

| Task | Location | Notes |
|------|----------|-------|
| Config loading | `src/config.rs` | Single unified entry, `-c` flag support |
| Config reference | `config.example.toml` | All fields documented with examples (authoritative) |
| DeepSeek chat flow | `src/ds_core/` | accounts → pow → completions → client |
| Chat orchestration + file upload | `src/ds_core/completions.rs` | `v0_chat()`, history splitting, upload retry, `GuardedStream` |
| OpenAI request parsing | `src/openai_adapter/request/` | normalize → tools → files → prompt → resolver |
| File upload extraction | `src/openai_adapter/request/files.rs` | data URL → FilePayload, HTTP URL → search mode |
| OpenAI response conversion | `src/openai_adapter/response/` | sse_parser → state → converter → tool_parser |
| Tool call tag config | `src/openai_adapter/response/tool_parser.rs` | `TagConfig` with extra_starts/extra_ends fallback arrays |
| Stream pipeline config | `src/openai_adapter/response.rs` | `StreamCfg` struct (consolidates 8 stream params) |
| Anthropic compat layer | `src/anthropic_compat/` | Built on openai_adapter, no direct ds_core access |
| Anthropic streaming response | `src/anthropic_compat/response/stream.rs` | OpenAI SSE → Anthropic SSE event stream |
| Anthropic aggregate response | `src/anthropic_compat/response/aggregate.rs` | OpenAI JSON → Anthropic JSON |
| OpenAI protocol types | `src/openai_adapter/types.rs` | Request/response structs, `#![allow(dead_code)]` |
| Model listing | `src/openai_adapter/models.rs` | Model registry and listing |
| HTTP server/routes | `src/server/` | handlers → stream → error |
| Unified debug CLI | `examples/adapter_cli.rs` + `examples/adapter_cli-script.txt` | Modes: chat/raw/compare/concurrent/status/models |
| Example request JSON | `examples/adapter_cli/` | Pre-built ChatCompletionsRequest samples (chat, stream, stop, reasoning, web_search, tool_call, etc.) |
| Scripted regression test | `just adapter-cli -- source examples/adapter_cli-script.txt` | Runs all JSON samples in sequence |
| Stress test scripts | `py-e2e-tests/stress_test_tools_openai.py`, `py-e2e-tests/stress_test_tools_anthropic.py` | Load testing for OpenAI and Anthropic endpoints |
| e2e scenario test framework | `py-e2e-tests/runner.py`, `py-e2e-tests/scenarios/` | JSON-driven scenarios with checks; `stress_runner.py` for load testing |
| CI pipeline | `.github/workflows/ci.yml` | `cargo check + clippy + fmt + audit + machete` and `cargo test` |
| Release workflow | `.github/workflows/release.yml` | Tag `v*` triggers multi-platform build (8 targets, 4 OS) + CHANGELOG release notes |
| Code style | `docs/code-style.md` | Comments, naming conventions (Chinese in source files) |
| Logging spec | `docs/logging-spec.md` | Targets, levels, message format for the `log` crate |
| Prompt injection strategy | `docs/deepseek-prompt-injection.md` | DeepSeek native tags, claude-3.5-sonnet system prompt research |
| API reference | `docs/deepseek-api-reference.md` | DeepSeek endpoint details |
| Claude config | `AGENTS.md` | Agent delegation patterns for this repo |

## Conventions

- **Config**: Uncommented values in `config.toml` = required; commented = optional with default
- **Module files**: `foo.rs` declares sub-modules, `foo/` contains implementation
- **Comments**: Chinese in source files (team preference)
- **Errors**: Chinese error messages for user-facing output
- **Logging**: `log` crate with explicit targets. Untargeted logs (e.g., bare `log::info!`) are prohibited. Targets used:
  - `ds_core::accounts`, `ds_core::client`
  - `adapter` (for `openai_adapter`)
  - `http::server`, `http::request`, `http::response` (for `server`)
  - `anthropic_compat`, `anthropic_compat::models`, `anthropic_compat::request`, `anthropic_compat::response::stream`, `anthropic_compat::response::aggregate`
  - See `docs/logging-spec.md` for full target/level mapping
- **Visibility**: `pub(crate)` for types not part of the public API; facade modules keep submodules private with `mod`
- **Tests**: All tests are inline (`#[cfg(test)]` within `src/` files). `request.rs` has sync unit tests for parsing logic; `response.rs` has `tokio::test` async tests for stream aggregation. No separate `tests/` directory.
- **Test output**: `println!` / `eprintln!` are allowed inside `#[cfg(test)]` blocks for debugging test failures; they remain prohibited in library code
- **Import grouping**: std → third-party → `crate::` → local (`super`, `self`), separated by blank lines
- **Comments**: Follow `docs/code-style.md`:
  - `//!` — module docs: first line = responsibility, then key design decisions
  - `///` — public API docs: verb-led, note side effects and panic conditions
  - `//` — inline: explain "why", not "what"
- **Naming**: `snake_case` for modules/functions, `PascalCase` for types/enum variants, `SCREAMING_SNAKE_CASE` for constants
- **Test code**: `println!` / `eprintln!` are allowed inside `#[cfg(test)]` for debugging failures; prohibited in library code

## Anti-Patterns

- Do NOT create separate config entry points — `src/config.rs` is the single source
- Do NOT implement provider logic outside its `*_core/` module
- Do NOT commit `config.toml` (only `config.example.toml`)
- Do NOT use `println!`/`eprintln!` in library code — use `log` crate with target
- Do NOT use untargeted log macros — always specify `target: "..."`
- Do NOT access `ds_core` directly from `anthropic_compat` — always go through `OpenAIAdapter`
- Do NOT add `#[allow(...)]` outside `src/ds_core/client.rs` — dead API methods and deserialized fields for API symmetry are expected only in the raw HTTP client layer

## Fork Management (easonlao/ds-free-api)

### Remote Setup
- `origin` = `easonlao/ds-free-api` (your fork)
- `upstream` = `NIyueeE/ds-free-api` (original author)

### Branch Workflow
- Feature/fix branches branch off `main`, not directly off upstream
- Naming: `fix/<desc>`, `feat/<desc>`, `refactor/<desc>`
- Always work in a branch, never directly on `main` or `upstream/*`

### Syncing with Upstream
```bash
# Sync main with upstream
git checkout main
git pull upstream main
git push origin main

# Update feature branch against latest upstream
git checkout fix/anthropic-stream
git rebase main
# Resolve conflicts, then force-push if already pushed
git push --force-with-lease origin fix/anthropic-stream

# Once fix is upstream or superseded, clean up local branch
git branch -d fix/anthropic-stream
git push origin --delete fix/anthropic-stream
```

### Before Merging PR
1. `git rebase main` on the feature branch
2. `cargo test` passes
3. `cargo clippy -- -D warnings` clean
4. `cargo build --release` succeeds
5. Verify with actual e2e test: `just e2e-serve` then `just e2e-basic`

## Commands

```bash
# Setup (do not commit config.toml)
cp config.example.toml config.toml

# Enable pre-commit hook (check + clippy + fmt + audit + machete + cargo test)
git config core.hooksPath .githooks

# One-pass check (check + clippy + fmt + audit + unused deps)
just check

# Run the HTTP server with basic logging
just serve
RUST_LOG=info just serve
# Trace through the entire SSE pipeline
RUST_LOG=adapter=trace,ds_core::accounts=debug,info just serve
# Module-level logging filters
RUST_LOG=ds_core::accounts=debug,ds_core::client=warn,info just serve
RUST_LOG=adapter=debug,anthropic_compat=debug just serve

# Run unified protocol debug CLI (modes: chat, raw, compare, concurrent N, status, models, model <id>)
just adapter-cli
RUST_LOG=debug just adapter-cli
# Script mode — runs all JSON samples in sequence (full regression)
just adapter-cli -- source examples/adapter_cli-script.txt
# Interactive mode with a specific config
cargo run --example adapter_cli -- -c /path/to/config.toml

# Run specific test modules (pass test name filter and args)
just test-adapter-request
just test-adapter-response
just test-adapter-request converter_emits_role_and_content -- --exact

# Run a single Rust test (use -- --exact for precise name matching)
cargo test converter_emits_role_and_content -- --exact

# Run all Rust tests
cargo test

# Run only library tests (skips example compilation, faster iteration)
cargo test --lib

# e2e tests (requires `uv`, server on port 5317)
just e2e-basic    # 基础功能（OpenAI + Anthropic 双端点）
just e2e-repair   # 工具调用损坏修复专项
just e2e-stress   # 全部场景 × 3 次迭代压测

# Start server with e2e config
just e2e-serve

# Individual checks
cargo check
cargo clippy -- -D warnings
cargo fmt --check
cargo audit        # requires: cargo install cargo-audit
cargo machete      # requires: cargo install cargo-machete

# Build
cargo build
cargo build --release

# Release (tag push triggers CI: 8 targets x 4 platforms via cross)
git tag v0.x.x
git push origin v0.x.x
# CI extracts changelog from CHANGELOG.md, creates GitHub release
```
