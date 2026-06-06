# claw-code-local-subagent

**A claw-code fork optimized for sub-agent orchestration, local models, and resilient autonomous runs.**

This fork exists primarily so an orchestrating agent — such as OpenClaw — can launch isolated coding sessions against a codebase with a single terminal command, wait for completion, and receive a bounded diff-oriented result without inheriting the entire session transcript into its own context window.

Secondary strengths include native local-model support (LM Studio, Ollama), OpenRouter compatibility, a self-healing resilience layer for local inference failures, improved session compaction, and a `run-claw-code` command-line entry point designed for agent-to-agent invocation, with named-resource scheduling for GPU queues.

---

## What This Fork Is

The upstream [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) is a clean-room Rust reimplementation of the Claude Code agent harness — a full-featured coding CLI with file tools, bash execution, git integration, MCP bridges, and 130+ slash commands.

This fork — **claw-code-local-subagent** — takes that foundation and reorients it around two goals:

1. **Sub-agent execution.** Let a parent agent (OpenClaw or any orchestration system) launch bounded coding sessions and receive only the artifacts needed for review: a status JSON, a diff patch, and a short summary. No session transcript. No chain-of-thought. No verbose tool trace.
2. **Local-model-first operation.** Run against Ollama, LM Studio, or any OpenAI-compatible endpoint. The fork auto-detects providers from model names and environment variables, applies per-error-type retry strategies tuned for local inference failure modes, and includes a `claw setup` command to configure providers interactively.

Everything else — improved compaction, stream debugging, the named-resource scheduler, preemptive recovery — serves these two goals.

---

## Primary Workflow: Sub-Agent Through `run-claw-code`

The centerpiece of this fork is the `run-claw-code` entry point. It is designed for one-shot, isolated coding sessions launched by another agent.

```bash
run-claw-code --agent dev-backend --dir /path/to/repo --plan "Fix the failing parser tests and update the retry logic" --resource 3090-vram
```

What happens when you run this:

1. A git worktree branch is created from the target directory
2. A `claw` REPL boots inside that worktree, in non-interactive mode with resilience enabled
3. The session runs with a configurable timeout (default 30 minutes)
4. On completion, the diff is captured and a summary is extracted from the session log
5. The script outputs exactly four lines — nothing more

**The calling agent then:**

- reads `status.json` to determine success, failure, or timeout
- reads `summary.md` for a short description of what happened
- applies `diff.patch` if the result is acceptable
- does **not** ingest the full session log, tool trace, or conversation history

This bounded-output contract is the defining design decision of the fork. It makes long-running autonomous coding sessions feasible without overflowing the orchestrator's context window.

### Return Contract

```
task_id=<uuid>
/tmp/claw-runs/<uuid>/status.json
/tmp/claw-runs/<uuid>/diff.patch
/tmp/claw-runs/<uuid>/summary.md
```

### Why This Matters for Agent Orchestration

- **No session bleed.** The child session's full transcript stays on disk at `/tmp/claw-runs/<uuid>/`. The parent agent sees only the compiler-like artifacts: did it compile? what changed? what does the summary say?
- **Deterministic output.** Four lines, always in the same order. Machine-parseable by design.
- **Timeout-safe.** If a local model hangs on the first token or enters an infinite reasoning loop, the timeout kills the session and returns a diff of whatever was changed before the cutoff.
- **Resource-gated.** Tasks can request named hardware resources (e.g., `3090-vram`), and the scheduler serializes access automatically (see below).

### Preset Agents

The `--agent` flag selects a JSON preset that configures the model, environment variables, and any provider-specific flags. Presets live in `scripts/presets/`:

| Preset | Purpose |
|---|---|
| `dev-backend` | Backend development (Rust, API work) — via Ollama (nemotron-3-nano-4b) |
| `dev-frontend` | Frontend development (TypeScript, React) — auto-detect provider (qwen3:14b) |
| `planner` | Software architect using deepseek-v4-flash on OpenRouter — large context, structured planning |
| `bugfix` | Bug fixer using local LM Studio (qwen3:14b) — focused, precise, minimal changes |
| `documentation` | Technical writer using Ollama (nemotron-3-nano-4b) — read-only, fast, good for prose |

Custom presets can be added to `~/.lmcode/presets/` or passed inline through environment overrides.

---

## Named-Resource Scheduling

Tasks can declare a resource dependency via `--resource <name>`. This is designed for environments where GPU VRAM is a shared, limited resource across concurrent agent runs.

How it works:

- Resource locks are enforced through POSIX `flock` files in `/tmp/claw-runs/_locks/`
- A per-resource state file tracks how many slots are currently in use
- `--max-parallel <N>` controls concurrency per resource (default: 1)
- When a task completes, its slot is released and the next waiting task acquires it

This means two tasks targeting different resources (e.g., `3090-vram` and `cpu-rag`) can run concurrently, while two tasks contending for the same GPU are automatically serialized without the orchestrator needing to track resource state itself.

---

## Resilience and Self-Healing

Local model inference behaves differently from cloud APIs. GPUs get warm. Models get unloaded from VRAM to make room. First tokens stall. Streams die mid-response. The fork's resilience layer treats these as expected failure modes, not edge cases.

### What it does

- **Per-error-type retry budgets.** A model-unloaded error gets 10 retries with 3-second backoff. An empty stream gets 5 retries with 1-second backoff. A context window exceeded gets 2 retries. Each failure type has its own recovery strategy because each needs different handling.
- **Streaming degradation detection.** The `ModelHealthProfile` tracks consecutive failures per model. After enough empty-stream or first-token-timeout events, it automatically falls back to non-streaming requests for that model, then re-enables streaming when the health profile recovers. This prevents repeated stream failure loops without manual intervention.
- **Exponential backoff with jitter.** Retries spread out over time (`attempt^n` with random jitter) to avoid hammering a recovering GPU or inference server.
- **`CLAW_RESILIENCE` environment variable.** Set `CLAW_RESILIENCE=force` to enable resilience on all providers (including cloud), `none` to disable it everywhere, or leave unset for auto-detection of local endpoints.
- **`/resilience` slash command.** Toggle modes interactively during a session: `/resilience force`, `/resilience none`, or `/resilience auto`.
- **`--resilience` CLI flag.** Set the mode at launch: `claw --resilience force --model qwen3:14b`.

This layer is the difference between a five-minute codework session that completes despite a GPU warm-up stall and the same session failing silently at the first hiccup.

---

## Local Model Support

The fork auto-detects which provider to use based on the model name and environment variables, then configures the client accordingly. It does not require an Anthropic API key to function.

### Supported Providers

| Provider | Models | Authentication |
|---|---|---|
| Ollama | Any model pulled locally | `OPENAI_API_KEY=ollama` + `OPENAI_BASE_URL=http://localhost:11434/v1` |
| LM Studio | Any loaded model | `OPENAI_API_KEY=lmstudio` + `OPENAI_BASE_URL=http://localhost:1234/v1` |
| OpenAI | gpt-4o, o1, o3 | `OPENAI_API_KEY=sk-...` |
| xAI (Grok) | grok-3, grok-3-mini | `XAI_API_KEY=xai-...` |
| Anthropic | claude-opus, sonnet, haiku | `ANTHROPIC_API_KEY=sk-ant-...` |
| Any OpenAI-compatible | Provider-dependent | `OPENAI_API_KEY=...` + `OPENAI_BASE_URL=https://...` |
| DashScope | qwen-* (OpenAI wire format) | `DASHSCOPE_API_KEY=...` (routed only when `OPENAI_BASE_URL` is not set) |

### Launcher Commands

The `claw setup` subcommand configures and launches provider-specific sessions:

**`claw setup lmstudio [model]`**
- Probes known LM Studio addresses (recent IPs, localhost:1234, configured host:port)
- Fetches the model list from `/v1/models`
- Sets `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `CLAW_RESILIENCE=force`
- Launches claw with the selected model

**`claw setup openrouter [model]`**
- Manages API key in `~/.config/openroutercode/.env` (falls back to the legacy `~/.config/opencode/.env` for existing users)
- Fetches the tool-capable model catalog from OpenRouter
- Sets `OPENAI_BASE_URL=https://openrouter.ai/api/v1`, `CLAW_RESILIENCE=none`
- Launches claw with the selected model

### Ollama and OpenRouter Launchers

The install.sh script also deploys standalone shell launchers to `~/.local/bin/`:

| Command | Purpose |
|---|---|
| `lmcode` | LM Studio — auto-discovery, model list, config save |
| `ollamacode` | Ollama — server management, model TUI, context length detection |
| `openroutercode` | OpenRouter — 300+ model browser with pagination and filter chaining |

### Using claw as an MCP sub-agent server

`claw mcp serve` runs a stdio MCP server that exposes a purpose-built
`run_subagent` tool (plus `list_presets`) so a parent agent — Claude Code,
openclaw, or any MCP client — can spawn a **local or OpenRouter sub-agent** and
get a single bounded result back, instead of the entire raw toolbox.

Register it with Claude Code:

```bash
claude mcp add claw-subagents --env OPENROUTER_API_KEY=sk-or-... -- claw mcp serve
```

Or add it to a project's `.mcp.json`:

```json
{
  "mcpServers": {
    "claw-subagents": {
      "command": "claw",
      "args": ["mcp", "serve"],
      "env": { "OPENROUTER_API_KEY": "sk-or-..." }
    }
  }
}
```

The parent then sees two tools: `mcp__claw-subagents__run_subagent` and
`mcp__claw-subagents__list_presets`. A `run_subagent` call accepts `prompt`
(required), plus optional `provider`
(`local|openrouter|ollama|lmstudio|anthropic|openai|auto`), `model`,
`repo_dir`, `subagent_type`, `permission_mode`, `timeout_secs`,
`max_output_chars`, and `isolated`. It returns structured JSON
(`status`/`summary`/`diff_stat`/`duration_ms`/…), encoding failures and
timeouts in `status`/`error` rather than as transport errors.

Defaults: with no `model`, the sub-agent uses a DeepSeek model via OpenRouter
(override per-call or with `CLAW_SUBAGENT_MODEL`). Notes:

- `provider: "local" | "ollama" | "lmstudio"` requires the local server to be
  running **and** an explicit `model` (there is no local default).
- `provider: "anthropic"` requires `ANTHROPIC_API_KEY` and an explicit Claude
  `model`.
- `provider: "openrouter"` reads `OPENROUTER_API_KEY` (or
  `~/.config/openroutercode/.env`, then `~/.config/opencode/.env`).

---

## Improved Compaction

Session compaction controls context window pressure by summarizing older conversation turns. The fork extends upstream compaction with strategic granularity:

- **`CompactionStrategy` enum** — Standard (default), Aggressive (minimize context), Conservative (preserve more), Emergency (minimal viable summary for critical overflow)
- **Per-strategy configuration** — each strategy defines different token budgets, preservation windows, and summarization aggressiveness
- **System prompt overhead tracking** — compaction decisions account for the system prompt's token cost, preventing accidental overshoot
- **Timeline capping** — summaries are capped to the last 10 messages, preventing unbounded timeline growth in long sessions
- **Preemptive compaction** — a token health check runs before each API call; if the estimated context exceeds a warning threshold, compaction triggers preemptively rather than waiting for a context-window error

The result is that local model sessions — which often run for many turns without cloud-grade context windows — stay stable longer before hitting limits.

---

## Human Operator Improvements

The fork includes several quality-of-life refinements beyond the upstream baseline:

- **Stream debugging hooks** — a `HookStreamDebugger` trait with callbacks for `on_stream_start`, `on_stream_chunk`, `on_stream_end`, and `on_stream_error`. Useful for diagnosing empty-stream and first-token-stall issues with local models.
- **`--output-format json`** — every CLI action supports structured JSON output, making the binary consumeable by automation without terminal scraping.
- **Piped stdin reading** — `echo "summarize this repo" | claw prompt` works, merging piped content with the prompt argument.
- **Model provenance tracking** — the status output shows where the resolved model string came from (flag, env var, config file, or default), reducing confusion about which model is actually running.
- **`/history [count]` slash command** — show recent conversation history without full session replay.
- **HTTP proxy support** — `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` environment variables are honored for enterprise deployments behind proxies.
- **Tool message sanitization** — orphaned tool messages (those without a matching assistant `tool_calls`) are stripped before sending, preventing 400 errors from local model APIs that enforce strict message pairing.

---

## Quickstart

### Prerequisites
- Rust toolchain (1.70+)
- An LLM provider: Ollama running locally, LM Studio, or a cloud API key

### Install

```bash
git clone https://github.com/wadebalsamo/claw-code-local-subagent.git
cd claw-code-local-subagent
./install.sh
```

This builds the `claw` binary and installs launcher shortcuts to `~/.local/bin/`. Add that directory to your PATH if needed.

### Run

```bash
# Interactive session with a local model
ollamacode --model qwen3:14b

# One-shot sub-agent task from an orchestrator
run-claw-code --agent dev-backend --dir /workspace/repo \
  --plan "Add input validation to the user registration endpoint"

# Direct invocation
claw --model qwen3:14b "Refactor this module to use async/await"
```

---

## Example: Full Sub-Agent Lifecycle

```bash
# Step 1: Launch an isolated coding session
run-claw-code \
  --agent dev-backend \
  --dir /home/user/project \
  --plan "Add a /health endpoint that returns 200 and the build timestamp" \
  --resource 3090-vram \
  --timeout 1200

# Output:
# task_id=a1b2c3d4-e5f6-7890-abcd-ef1234567890
# /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/status.json
# /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/diff.patch
# /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/summary.md
```

**The calling agent then:**

```bash
# Step 2: Poll status
cat /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/status.json
# {"status": "done", "files_changed": "3", "lines_added": "45", ...}

# Step 3: Read the summary (not the full session log)
cat /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/summary.md
# "Added a /health endpoint handler, updated the router, added a build timestamp utility..."

# Step 4: Review the diff
cat /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/diff.patch
# diff --git a/src/router.rs b/src/router.rs
# +    .route("/health", get(health_handler))
# ...

# Step 5: Apply if acceptable
cd /home/user/project
git apply /tmp/claw-runs/a1b2c3d4-e5f6-7890-abcd-ef1234567890/diff.patch
```

The parent agent never sees the raw session output, the failed tool calls, the chain-of-thought, or any of the internal dialogue. Just what changed, whether it succeeded, and a human-readable summary.

---

## Implementation Status

This fork is under active development. The features listed below are verified implemented unless noted otherwise.

### Implemented

- **run-claw-code entry point** — Complete. Shell script with worktree creation, timeout, diff capture, summary extraction, resource locking, and structured output contract.
- **Named-resource scheduler** — Complete. POSIX `flock`-based serialization with slot tracking and `--max-parallel` support.
- **Local-model provider dispatch** — Complete. Auto-detection from model name and env vars for Ollama, LM Studio, OpenAI, xAI, Anthropic, DashScope.
- **`claw setup lmstudio` and `claw setup openrouter`** — Complete. Rust-native LM Studio auto-discovery, model fetching, env setup, and REPL launch.
- **`ResilienceConfig` with per-error-type retry budgets** — Complete. 30+ fields with per-error retry counts, backoffs, `force_enable()`, `force_disable()`, `from_env()`.
- **`ErrorClassifier` + `RecoveryStateMachine` + `ModelHealthProfile`** — Complete. `local_model_recovery.rs` module with streaming degradation detection.
- **`CompactionStrategy` enum** — Complete. Standard, Aggressive, Conservative, Emergency with per-strategy configs.
- **Preemptive compaction** — Complete. Token health check before API calls, auto-compact at warning threshold.
- **`extra_body` on MessageRequest** — Complete. Enables provider-specific parameters (repetition_penalty, top_k, etc.) for local models.
- **`HookStreamDebugger` trait** — Complete. Callbacks for stream lifecycle events and error capture.
- **`CLAW_RESILIENCE` env var** — Complete. Wired through CLI flag, slash command, and provider client construction.
- **`/resilience` slash command** — Complete. With `force`/`none`/`auto` modes and status display.
- **`--output-format json`** — Complete. All CLI actions support structured JSON output.
- **Piped stdin** — Complete. `echo "... | claw prompt` for non-interactive input.
- **Model provenance tracking** — Complete. Source tracking (flag/env/config/default) in status output.
- **HTTP proxy support** — Complete. `http_client.rs` with `ProxyConfig::from_env()`.
- **Per-base-url request building** — Complete. Different serialization for LM Studio vs OpenAI endpoints.
- **Tool message sanitization** — Complete. Strips orphaned tool messages before sending.
- **Six preset agents** — Complete. `planner`, `bugfix`, `documentation`, `dev-backend`, `dev-frontend`, and custom user presets for `run-claw-code`.

### In Active Integration

- **TUI/fzf model browser for `setup openrouter`** — The shell launcher (`openroutercode`) has this via external scripts. The native `claw setup openrouter` command currently accepts model names directly; a TUI model selector is a planned enhancement.
- **Provider-specific request serialization refinements** — Per-base-url functions are in place; broader coverage for additional local inference server variants is ongoing.
 
---

## Relationship to Upstream

This is a fork of [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code), which is a clean-room Rust reimplementation of Claude Code's agent harness — not a copy of Anthropic's source code. The fork tracks upstream changes selectively, adopting beneficial improvements while preserving its differentiators:

- **ResilienceConfig** — the fork's 30+ field version with per-error-type self-healing budgets is kept; upstream's minimalist version is rejected.
- **`extra_body`** — kept for local-model parameter passthrough; upstream removed it for strict protocol conformance.
- **Per-base-url request building** — kept for LM Studio and provider-specific compatibility; upstream consolidated to a single implementation.
- **DashScope routing** — the fork adopted upstream's conditional guard (`&& OPENAI_BASE_URL not set`) to prevent conflicts when users explicitly set `OPENAI_BASE_URL` for local providers.
- **Everything else** — session simplifications, error classification, thinking/reasoning removal, glob search simplification, and MCP/plugin lifecycle cleanups are fully in sync with upstream.

---

## License

MIT — same as upstream.
