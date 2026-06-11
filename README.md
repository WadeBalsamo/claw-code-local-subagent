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

## Set it up as a Claude Code sub-agent (start here)

claw-code plugs into **Claude Code** as a sub-agent over MCP: Claude Code stays the
orchestrator and calls one tool — `run_subagent` — to hand bulk coding to a **local model**
(LM Studio / Ollama) or a cheap **OpenRouter** model, and gets back a bounded result (status
+ summary + diff stat), never the sub-agent's transcript. `claw mcp serve --subagents` is the stdio MCP
server that exposes `run_subagent` and `list_presets`.

### 1. Build & install

```bash
git clone https://github.com/wadebalsamo/claw-code-local-subagent.git
cd claw-code-local-subagent
./install.sh                 # builds `claw`, installs launchers into ~/.local/bin
claw --version               # confirm (ensure ~/.local/bin is on your PATH)
```

### 2. Register the server in your `.claude/` config

Pick one. The tool Claude Code will expose is `mcp__claw-subagents__run_subagent`.

**Project-scoped (committed, shared with the repo)** — create **`.mcp.json`** at the repo root:

```json
{
  "mcpServers": {
    "claw-subagents": {
      "type": "stdio",
      "command": "claw",
      "args": ["mcp", "serve", "--subagents"],
      "env": { "OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}" }
    }
  }
}
```

(Drop the `env` block if you only use a local backend.) Or let the CLI write it — `--scope
project` writes `.mcp.json`; `--scope user` writes your global `~/.claude.json`:

```bash
claude mcp add --scope project claw-subagents --env OPENROUTER_API_KEY=sk-or-... -- claw mcp serve --subagents
```

### 3. Auto-approve the tools (skip the per-call prompt)

Add to **`.claude/settings.json`** (project) or `~/.claude/settings.json` (user):

```json
{
  "permissions": {
    "allow": [
      "mcp__claw-subagents__run_subagent",
      "mcp__claw-subagents__list_presets"
    ]
  }
}
```

A project `.mcp.json` server is also approved once on first launch. Run `/mcp` inside Claude
Code to confirm `claw-subagents` is connected and exposes `run_subagent` + `list_presets`.

### 4. Point the sub-agent at a model & delegate

- **OpenRouter (default):** set `OPENROUTER_API_KEY` (step 2). The default sub-agent model is
  `deepseek/deepseek-v4-pro`; override per call with `model`, or globally with
  `CLAW_SUBAGENT_MODEL`.
- **Local:** run LM Studio (`localhost:1234`) with a **tool-capable** model — the recommended
  local default is **NVIDIA Nemotron-3-Super 120B** (`nemotron-3-super-120b-a12b`) — and pass
  `provider: "lmstudio"` + the loaded `model` (or just use the `nemotron-3-super-120b` preset).
  Ollama works too (`localhost:11434`, `provider: "ollama"`).

Ask Claude Code to delegate, or it calls `mcp__claw-subagents__run_subagent` directly — only
`prompt` is required:

```json
{ "prompt": "Add input validation to src/api/users.rs and a unit test", "repo_dir": "/abs/path/to/repo" }
```

The **Setup reference** below expands this: choosing/verifying a backend, the in-process vs
`isolated` execution modes, and GPU resource tips.

---

## Set it up as a coder worker for an autonomous agent fleet

The fork's original purpose: let an **autonomous multi-agent system** — a department of
orchestrator agents that themselves run on local/OpenRouter models (never Claude) — spawn
stateless coder workers and review only the artifacts. The preset catalog is tuned for
exactly this.

**Pick a model; the prompt comes with it.** Each preset is keyed to one model and carries the
system prompt that gets the most out of it — no personas, no role-play. The orchestrator picks
the model per task (the smallest that will do), runs it **local-first** (zero marginal cost),
and falls back to a **budget-gated** remote model when local can't serve the call:

```bash
run-claw-code --agent qwen3-coder-30b --dir <repo> --plan "<brief>" \
  --pr-into sprint/<id> --sprint-id <id> \
  [--provider openrouter --model deepseek/deepseek-v4-flash]   # budget-gated remote override
```

`--provider` / `--model` override a preset's local-first default, so the orchestrator can run
the same task on a budget-gated remote model **without editing the preset**. The launcher
never falls back to a remote model on its own — that path stays in the orchestrator, where the
budget gate lives (no bypass).

**The preset catalog** (`scripts/presets/`) — one preset per model:

| Preset | Model | Where it runs | Best for |
|---|---|---|---|
| `qwen3-coder-30b` | Qwen3-Coder-30B (Q5) | local · `gpu` | Default coder for well-scoped, single-file slices |
| `qwen3-coder-next` | Qwen3-Next-Coder 80B | local · `gpu+cpu` | Strongest local coder — complex, multi-file slices |
| `nemotron-3-super-120b` | Nemotron-3-Super 120B | local · `gpu+cpu` | Hardest slices + one-shot planning (1M context) |
| `nemotron-3-nano-30b` | Nemotron-3-Nano 30B | local · `cpu` | Fast, parallel-safe worker: tests, docs, analysis |
| `nemotron-3-nano-4b` | Nemotron-3-Nano 4B | local · `cpu` | Tiny/quick: classify, route, summarize, extract |
| `minimax-m2` | MiniMax-M2 | local · `cpu` | Ops & analysis: synthesis, comparison, decision support |
| `deepseek-v4-flash` | DeepSeek-V4-Flash | OpenRouter · `remote` | Cheap remote tier for routine coding / review |
| `deepseek-v4-pro` | DeepSeek-V4-Pro | OpenRouter · `remote` | Strong remote tier for hard diffs, reviews, audits |

The local coders form a natural escalation ladder (30B → 80B → 120B); the two DeepSeek models
are the remote tiers. CPU presets are parallel-safe; GPU presets serialize on the single GPU
(the orchestrator arbitrates; `--resource` is a local `flock` fallback). The `list_presets`
MCP tool returns each preset's `name`, `description`, `provider`, `model`, and `resource` so
the orchestrator can choose one. The full CLI contract, PR automation (`--pr-into`), and
sprint manifests (`--sprint-id`) are in **Providing claw-code as a tool to OpenClaw agents**
below.

---

## Primary Workflow: Sub-Agent Through `run-claw-code`

The centerpiece of this fork is the `run-claw-code` entry point. It is designed for one-shot, isolated coding sessions launched by another agent.

```bash
run-claw-code --agent qwen3-coder-30b --dir /path/to/repo --plan "Fix the failing parser tests and update the retry logic" --resource gpu
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

The `--agent` flag selects a JSON preset in `scripts/presets/` that carries the local-first
`provider`/`model`, the broker `resource` slot, the model-tuned `system_prompt`,
`permission_mode` / `plan_mode`, and operational fields (`completion_webhook`, `budget_gate`,
`open_pr`). The shipped catalog is **one preset per model** — `qwen3-coder-30b`,
`qwen3-coder-next`, `nemotron-3-super-120b`, `nemotron-3-nano-30b`, `nemotron-3-nano-4b`,
`minimax-m2`, `deepseek-v4-flash`, `deepseek-v4-pro` — detailed under
[Set it up as a coder worker for an autonomous agent fleet](#set-it-up-as-a-coder-worker-for-an-autonomous-agent-fleet).

Add your own under `~/.lmcode/presets/` (takes precedence) or `scripts/presets/`; run
`run-claw-code --help` for the full preset JSON schema. The `list_presets` MCP tool
enumerates whatever is installed.

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

See [docs/local-openai-compatible-providers.md](docs/local-openai-compatible-providers.md) for a full walkthrough of OpenAI-compatible routing, the `claw doctor` reachability check, `CLAW_RESILIENCE`, `extra_body`, and the launchers.

See the next section for a complete, step-by-step walkthrough of wiring claw-code into Claude Code (or OpenClaw) as a sub-agent.

---

## Setup reference: backends, verification & execution modes

This expands the two quickstarts above with the full detail — backends, verification, and the
two execution modes.

### The mental model

Claude Code (running Sonnet) stays the **orchestrator**. claw-code becomes a cheap, sandboxed **sub-agent** it calls through a single MCP tool — `run_subagent` — to offload bulk coding work to a **local model** or an **OpenRouter model** (DeepSeek by default). You delegate to save Sonnet tokens and/or to run fully local, and you get back a **bounded result** (status + summary + diff stat), never the sub-agent's transcript.

`claw mcp serve --subagents` is the bridge: a stdio MCP server that exposes exactly two tools — `run_subagent` and `list_presets`.

### Step 1 — Install

```bash
git clone https://github.com/wadebalsamo/claw-code-local-subagent.git
cd claw-code-local-subagent
./install.sh
```

`install.sh` builds the `claw` binary and then runs the Rust-native `claw install` subcommand, which writes the launcher shortcuts (`claw`, `lmcode`, `ollamacode`, `openroutercode`, `run-claw-code`) into `~/.local/bin/` with the repo root embedded — no shell `sed` hacks. Make sure `~/.local/bin` is on your `PATH`, then confirm:

```bash
claw --version
```

### Step 2 — Choose the sub-agent's model backend

Point the sub-agent at OpenRouter (simplest) or a local server. You can configure several — `run_subagent` takes a `provider` per call.

#### Option A — OpenRouter (cloud, default)

1. Create an OpenRouter API key.
2. Store it (either works):
   - `claw setup openrouter` — interactive; saves the key to `~/.config/openroutercode/.env`, **or**
   - export it for the session: `export OPENROUTER_API_KEY=sk-or-...`
3. The default sub-agent model is `deepseek/deepseek-v4-pro` — the strong, tool-capable default. Use `deepseek/deepseek-v4-flash` for a cheaper/faster option, or any tool-capable OpenRouter model. Override globally with `CLAW_SUBAGENT_MODEL`, or per call with the `model` field.

#### Option B — Local: Ollama

1. Install Ollama and pull a **tool-capable** model (it must support function/tool calling to act as an agent):
   ```bash
   ollama pull qwen3:14b
   ollama serve            # if not already running (serves http://localhost:11434)
   ```
2. (Optional) Keep the model warm between calls so each delegated task doesn't pay reload latency:
   ```bash
   export OLLAMA_KEEP_ALIVE=30m
   ```
3. The sub-agent reaches Ollama at `http://localhost:11434/v1` automatically when you call it with `provider: "ollama"` (or `"local"`).

#### Option C — Local: LM Studio

1. In LM Studio, load a **tool-capable** model and start the local server (defaults to `http://localhost:1234`).
2. Call the sub-agent with `provider: "lmstudio"` and the loaded model's id.

> **Tool-calling is mandatory.** A plain chat model can answer questions but cannot read/edit files or run commands, so it cannot function as a coding sub-agent. Choose a model whose card advertises tools/function-calling.

### Step 3 — Verify the backend before wiring it in

```bash
# Reachability + environment check (includes the OPENAI_BASE_URL probe)
claw doctor

# Quick interactive smoke test of the backend you chose:
openroutercode                      # OpenRouter — model browser + chat
ollamacode --model qwen3:14b        # Ollama
lmcode                              # LM Studio (auto-discovers the server)
```

If `claw doctor` reports the endpoint reachable and a smoke chat works, the backend is ready.

### Step 4 — Register claw-code as an MCP sub-agent in Claude Code

`claw mcp serve --subagents` is the server Claude Code spawns. Register it once:

**OpenRouter:**
```bash
claude mcp add claw-subagents --env OPENROUTER_API_KEY=sk-or-... -- claw mcp serve --subagents
```

**Local (Ollama / LM Studio)** — no key needed; just keep the local server running:
```bash
claude mcp add claw-subagents -- claw mcp serve --subagents
```

**Project-scoped** (commit to share with a repo) — `.mcp.json`:
```json
{
  "mcpServers": {
    "claw-subagents": {
      "command": "claw",
      "args": ["mcp", "serve", "--subagents"],
      "env": { "OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}" }
    }
  }
}
```

Confirm inside Claude Code with `/mcp` — you should see `claw-subagents` exposing `run_subagent` and `list_presets`.

**How the env flows:** Claude Code launches `claw mcp serve --subagents` as a child process and injects the `--env` (or `.mcp.json` `env`) values into it. Inside, `run_subagent` reads `OPENROUTER_API_KEY` (or the `~/.config/openroutercode/.env` → `~/.config/opencode/.env` fallback) for OpenRouter; for local providers it sets the localhost base URL itself.

### Step 5 — Delegate work to the sub-agent from Claude Code

Ask Claude Code to use the tool, or it calls `mcp__claw-subagents__run_subagent` directly. The only required field is `prompt`.

**OpenRouter (uses the DeepSeek default):**
```json
{
  "prompt": "Add input validation to src/api/users.rs and a unit test for it",
  "repo_dir": "/abs/path/to/repo"
}
```

**Local Ollama (explicit model required):**
```json
{
  "provider": "ollama",
  "model": "qwen3:14b",
  "prompt": "Refactor the parser module to return Result instead of panicking",
  "repo_dir": "/abs/path/to/repo",
  "permission_mode": "workspace-write"
}
```

Full input: `prompt` (required) plus optional `provider` (`local|openrouter|ollama|lmstudio|anthropic|openai|auto`), `model`, `repo_dir`, `subagent_type`, `permission_mode` (`read-only|workspace-write|danger-full-access`), `isolated`, `timeout_secs`, `max_output_chars`. The tool returns compact JSON — `status` (`completed|failed|timeout`), `summary`, `diff_stat`, `model`, `duration_ms` — and **never** the sub-agent's transcript. Failures and timeouts are encoded in `status`/`error`, not raised as transport errors. Claude Code reads the summary + diff stat, inspects the changed files in `repo_dir`, and decides what to keep.

Provider notes:
- `provider: "local" | "ollama" | "lmstudio"` requires the local server running **and** an explicit `model` (there is no local default).
- `provider: "anthropic"` requires `ANTHROPIC_API_KEY` and an explicit Claude `model`.
- with no `model`, the sub-agent uses the DeepSeek default via OpenRouter.

Discover ready-made (provider, model, system-prompt) bundles with the `list_presets` tool (sourced from `scripts/presets/` and `~/.lmcode/presets/`).

**Two execution modes:**
- **Default (in-process):** fast; the sub-agent edits files directly in `repo_dir`. The MCP server processes calls one at a time.
- **`isolated: true` (with a `preset`):** runs through `run-claw-code` in a throwaway **git worktree** with a hard kill-on-timeout and a captured `diff.patch` — use it for long/untrusted tasks, or when you want GPU scheduling (below).

### Resource management for local GPUs

Local inference is VRAM-bound, and that shapes how you run local sub-agents:

- **One GPU usually holds one model.** Firing several local sub-agents at the same GPU forces the server to swap models (slow) or run out of VRAM. Plan for **one resident model per GPU**.
- **A single Claude Code session is already serialized.** The MCP server processes `run_subagent` calls **one at a time**, so one Claude Code session won't fan out concurrent GPU hits through a given `claw mcp serve --subagents`.
- **For fleets / multiple orchestrators, use the named-resource scheduler.** Concurrency across *different* agents is where GPUs collide. Tag work to a named resource and the scheduler serializes it with `flock`:
  ```bash
  run-claw-code --agent qwen3-coder-30b --dir /repo --plan "..." \
    --resource gpu --max-parallel 1
  ```
  All tasks tagged `gpu` are serialized (cap = how many models fit in VRAM, usually `1`), while a task on a different resource (e.g. `cpu`) runs concurrently. From Claude Code you reach this path via `run_subagent` with `isolated: true` and a preset whose `resource` is a GPU slot (`gpu` or `gpu+cpu`), or by calling `run-claw-code` directly. See [Named-Resource Scheduling](#named-resource-scheduling).
- **Keep models warm:** `OLLAMA_KEEP_ALIVE=30m` avoids paying model-load latency on every delegated task.
- **Leave resilience on for local:** `CLAW_RESILIENCE` auto-enables for localhost endpoints (force it with `CLAW_RESILIENCE=force`). It converts GPU warm-up stalls and model-unload blips into automatic retries instead of hard failures — see [Resilience and Self-Healing](#resilience-and-self-healing).

**Rule of thumb:** one physical GPU → one resource name → `--max-parallel 1`, one resident model, keep-alive on, resilience on.

### Providing claw-code as a tool to OpenClaw agents

OpenClaw is the orchestration system this fork was built for. There are two ways to give it claw-code as a sub-agent; both preserve the bounded-output contract.

**Mode 1 — MCP tool (interactive, single result).** If your OpenClaw agent speaks MCP, register `claw mcp serve --subagents` exactly as in Step 4 and call `run_subagent` / `list_presets`. Best when the orchestrator wants one delegated result inline and lets claw manage provider/model.

**Mode 2 — `run-claw-code` CLI contract (fleets, GPU scheduling, PRs).** The original design, and the better fit for many concurrent agents. OpenClaw shells out:
```bash
run-claw-code --agent <preset> --dir <repo> --plan "<task>" \
  --resource <gpu-name> --max-parallel <N> \
  [--timeout <seconds>] [--pr-into <base-branch>] [--sprint-id <id>]
```
and reads the deterministic return contract:
```
task_id=<uuid>
/tmp/claw-runs/<uuid>/status.json
/tmp/claw-runs/<uuid>/diff.patch
/tmp/claw-runs/<uuid>/summary.md
# plus pr_request.json when --pr-into is set
```
This path layers on what fleet orchestration needs beyond `run_subagent`: git-worktree isolation, hard kill-on-timeout, the `flock` GPU scheduler, and optional PR creation (`--pr-into`) and sprint manifests (`--sprint-id`). The orchestrator polls `status.json`, reads `summary.md`, applies `diff.patch` — never ingesting the transcript. Presets (`scripts/presets/`, or per-user `~/.lmcode/presets/`) carry the local-first `provider`/`model`, the `tier_ref`, the broker `resource` slot, and the permission/plan modes, so the orchestrator only picks a preset and a task — and may override `--provider`/`--model` to run a budget-gated OpenRouter rung.

**Which to use:** MCP `run_subagent` for interactive, one-off delegation (Claude Code, or an MCP-native OpenClaw agent); `run-claw-code` for batch/parallel fleets that need GPU scheduling and PR automation.

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
run-claw-code --agent qwen3-coder-30b --dir /workspace/repo \
  --plan "Add input validation to the user registration endpoint"

# Direct invocation
claw --model qwen3:14b "Refactor this module to use async/await"
```

---

## Example: Full Sub-Agent Lifecycle

```bash
# Step 1: Launch an isolated coding session
run-claw-code \
  --agent qwen3-coder-30b \
  --dir /home/user/project \
  --plan "Add a /health endpoint that returns 200 and the build timestamp" \
  --resource gpu \
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
- **Model-keyed preset catalog** — Complete. One preset per model — `qwen3-coder-30b`, `qwen3-coder-next`, `nemotron-3-super-120b`, `nemotron-3-nano-30b`, `nemotron-3-nano-4b`, `minimax-m2`, `deepseek-v4-flash`, `deepseek-v4-pro` — plus custom user presets for `run-claw-code`.

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
