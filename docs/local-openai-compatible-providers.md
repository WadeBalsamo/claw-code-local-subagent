# Local OpenAI-compatible providers and skills setup

This guide covers two common offline/local workflows:

1. running Claw against an OpenAI-compatible local model server such as Ollama, llama.cpp, or vLLM; and
2. installing local skills from disk so Claw can discover them without network access.

> This is the **claw-code-local-subagent fork's** copy of the upstream
> `local-openai-compatible-providers.md`. The core OpenAI-compatible routing is
> the same as upstream; the **Fork addendum** at the bottom documents the
> divergences this fork adds (extra_body passthrough, `CLAW_RESILIENCE`
> self-healing, the `lmcode`/`ollamacode`/`openroutercode` launchers, and the
> `claw mcp serve` sub-agent tools). It was adapted from upstream
> `ultraworkers/claw-code@main` and then verified against this fork's code.

## Claw is not Claude-only

Claw Code is a Claude-Code-shaped workflow/runtime, not a Claude-only product. It supports Anthropic directly and can target OpenAI-compatible, provider-routed, and local models depending on configuration. Non-Claude providers are supported honestly: they may require stricter tool-call and response-shape compatibility, and some slash/tool workflows can be rougher than first-party Anthropic/OpenAI paths. Provider-specific identity leaks are bugs, not intended product positioning.

If you need the most polished daily-driver experience for a specific non-Claude model today, compare that provider's native tools. If you need runtime/provider hackability, Claw's OpenAI-compatible route is the intended extension path. This fork in particular reorients around local-model-first operation; see the README for the broader provider auto-detection story.

## OpenAI-compatible routing basics

Set `OPENAI_BASE_URL` to the server's `/v1` endpoint and set `OPENAI_API_KEY` to either the required token or a harmless placeholder for local servers that expect an Authorization header. Authless local/private OpenAI-compatible servers can leave `OPENAI_API_KEY` unset. The model name must match what the server exposes.

```bash
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
export OPENAI_API_KEY="local-dev-token"
claw --model "qwen3:latest" prompt "Reply exactly HELLO_WORLD_123"
```

Routing notes:

- The fork auto-detects the provider from the model name and environment
  variables. Setting `OPENAI_BASE_URL` plus a local-looking model tag such as
  `llama3.2` or `qwen2.5-coder:7b` selects the local OpenAI-compatible route.
- For local servers, prefer the exact model ID reported by the server
  (`qwen3:latest`, `llama3.2`, etc.).
- Tool workflows need model/server support for OpenAI-compatible tool calls.
  Plain prompt smoke tests can pass even when slash/tool workflows still fail
  because the server returns an incompatible tool-call shape. This fork strips
  orphaned tool messages before sending to avoid 400s from strict local APIs.

## Confirm reachability with `claw doctor`

Before debugging a turn, run `claw doctor`. When `OPENAI_BASE_URL` is set this
fork adds an **"OpenAI base URL"** check that parses the host/port out of the URL
and does a short (~2s) TCP reachability probe against the server:

```bash
OPENAI_BASE_URL=http://127.0.0.1:1234/v1 claw doctor
```

- **reachable** → the check is `ok` and reports the resolved endpoint.
- **unreachable** → the check `warn`s and says the local server (LM Studio /
  Ollama / OpenRouter) may be down or the URL may be wrong, with a suggested
  action to start the server or re-run `claw setup`.
- **unset** → the check is a skipped/`ok` no-op (the default Anthropic path is
  covered by the `Auth` check), so it never fails a vanilla setup.

This catches the most common cause of a "hang" on the first turn — a down or
mis-typed local endpoint — before you start a real request.

## Raw `/v1/chat/completions` smoke test

Before debugging Claw, verify the local server speaks the expected wire format:

```bash
curl -sS "$OPENAI_BASE_URL/chat/completions" \
  -H "Authorization: Bearer ${OPENAI_API_KEY:-local-dev-token}" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen3:latest",
    "messages": [{"role": "user", "content": "Reply exactly HELLO_WORLD_123"}],
    "stream": false
  }'
```

Expected result: a JSON response with one assistant message containing `HELLO_WORLD_123`. If this fails, fix the local server, model name, or auth token before changing Claw settings.

## Ollama

Start Ollama and pull a model:

```bash
ollama pull qwen3:latest
ollama serve
```

In another shell, point Claw at the local endpoint:

```bash
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
export OPENAI_API_KEY="ollama"
claw --model "qwen3:latest" prompt "Reply exactly HELLO_WORLD_123"
```

If Ollama is running without auth, any placeholder `OPENAI_API_KEY` works; use a
placeholder token rather than a real cloud API key. For an interactive,
auto-discovering experience, prefer the `ollamacode` launcher or
`claw setup ollama` (see the Fork addendum).

## llama.cpp server

Start a llama.cpp OpenAI-compatible server with the model name you want Claw to send:

```bash
llama-server -m ./models/qwen2.5-coder.gguf --host 127.0.0.1 --port 8080 --alias qwen2.5-coder
```

Then smoke-test through Claw:

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8080/v1"
export OPENAI_API_KEY="local-dev-token"
claw --model "qwen2.5-coder" prompt "Reply exactly HELLO_WORLD_123"
```

## vLLM or another OpenAI-compatible server

Start vLLM with an OpenAI-compatible API server:

```bash
vllm serve Qwen/Qwen2.5-Coder-7B-Instruct --host 127.0.0.1 --port 8000
```

Then route Claw to it:

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8000/v1"
export OPENAI_API_KEY="local-dev-token"
claw --model "Qwen/Qwen2.5-Coder-7B-Instruct" prompt "Reply exactly HELLO_WORLD_123"
```

## Local skills install from disk

Skills are discovered from Claw skill roots such as `.claw/skills/` in a workspace and `~/.claw/skills/` for user-level installs.

A skill directory should contain a `SKILL.md` file with frontmatter:

```text
my-skill/
└── SKILL.md
```

```markdown
---
name: my-skill
description: Explain when this skill should be used.
---

# My Skill

Instructions for the agent go here.
```

Install a skill from a local path in the interactive REPL:

```text
/skills install /absolute/path/to/my-skill
/skills list
/skills my-skill
```

Or inspect skills from the direct CLI surface:

```bash
claw skills --output-format json
```

Offline install checklist:

- Install the specific skill directory, not only the repository root, unless that repository root itself contains `SKILL.md`.
- Keep the frontmatter `name` aligned with the directory name users will type.
- After installing, run `/skills list` or `claw skills --output-format json` to confirm the discovered name and source path.
- If a skill invocation fails with an HTTP/provider error, the skill may have installed correctly but the current model/provider call failed. Run `claw doctor`, verify provider credentials, and try a simple prompt smoke test before reinstalling the skill.

## Troubleshooting

| Symptom | Check |
|---|---|
| Claw still asks for Anthropic credentials | Use an explicit OpenAI-compatible model route or remove unrelated Anthropic env vars during local smoke tests. |
| First turn hangs or times out connecting | Run `claw doctor`; the "OpenAI base URL" check probes reachability of `OPENAI_BASE_URL`. |
| `model not found` from local server | Use the exact model ID exposed by Ollama/llama.cpp/vLLM. |
| Plain prompt works but tools fail | Confirm the model/server supports OpenAI-compatible tool calls and response shapes. |
| Local model loops or stalls on first token | This fork's self-healing layer (`CLAW_RESILIENCE`) detects and recovers; see the Fork addendum. |
| Skill says installed but `/skills <name>` fails | Check `/skills list` for the discovered name and source; verify provider credentials separately with `claw doctor`. |

---

# Fork addendum (claw-code-local-subagent)

This section documents behavior that exists in **this fork** and is intentionally
**not** in upstream's document. Everything below was verified against the code in
this repository.

## `extra_body` parameter passthrough (fork divergence)

Local inference servers often accept sampling/decoding parameters that are not
part of the strict Anthropic request schema — for example `top_k`, `min_p`,
`repetition_penalty`, or a server-specific `stop` list. Upstream removed this
escape hatch for strict protocol conformance; **this fork keeps it.**

Internally the request profile carries an `extra_body` map
(`AnthropicRequestProfile::extra_body`, `with_extra_body_param(...)`), and the
request builder merges those keys into the top level of the outgoing JSON body
before sending (`render_json_body` in the `telemetry` crate). This lets
local-model-specific parameters ride along to OpenAI-compatible servers without
the runtime needing to know each server's bespoke knobs.

Notes:

- `extra_body` keys are merged at the **top level** of the request body, so they
  must match the parameter names the target server expects.
- This is a programmatic passthrough on the request profile (a fork-protected
  divergence), not a single magic environment variable. Presets and provider
  wiring use it to set local-friendly defaults; see `scripts/presets/`.

## `CLAW_RESILIENCE=force|none|auto` and local-model self-healing

The fork adds a self-healing resilience layer tuned for local-inference failure
modes (empty streams, first-token stalls, runaway reasoning loops,
loading/cold-start errors). It is controlled by the `CLAW_RESILIENCE`
environment variable, parsed by `ResilienceConfig::from_env`:

| Value | Behavior |
|---|---|
| `force` | Force-enable resilience recovery on **all** providers/URLs (including cloud). |
| `none` | Force-disable resilience everywhere. |
| `auto` *(or unset)* | Default: auto-detect localhost-style endpoints and enable recovery for them. |

The value is case-insensitive. Per-error-type retry/recovery strategies (error
classification, a recovery state machine, and per-model health profiles) sit
behind this switch so a flaky local model can recover mid-run instead of failing
the whole session. The launchers set this for you: `lmcode` and `ollamacode`
export `CLAW_RESILIENCE=force` (local servers), while `openroutercode` exports
`CLAW_RESILIENCE=none` (a hosted gateway that doesn't need local recovery).

## Provider setup commands and launchers

You can configure providers two ways in this fork.

### `claw setup <provider>`

```bash
claw setup ollama      [model]       # probe local Ollama, pick a model, launch
claw setup lmstudio    [model]       # auto-discover LM Studio, fetch models, launch
claw setup openrouter  [model] [--set-key KEY] [--list-models]
```

- `claw setup lmstudio` probes known LM Studio addresses (recent IPs,
  `localhost:1234`, configured host:port), fetches the loaded model list, and
  sets `OPENAI_BASE_URL`, `OPENAI_API_KEY`, and `CLAW_RESILIENCE=force`.
- `claw setup openrouter` manages the API key (see config dir below), fetches the
  tool-capable model catalog, and sets `OPENAI_BASE_URL=https://openrouter.ai/api/v1/chat/completions`
  with `CLAW_RESILIENCE=none`.

### Standalone launchers (`~/.local/bin`)

`install.sh` also deploys standalone shell launchers:

| Launcher | Provider | Notable env it exports |
|---|---|---|
| `lmcode` | LM Studio (auto-discovery, model list) | `OPENAI_BASE_URL=http://<host>:<port>/v1`, `CLAW_RESILIENCE=force` |
| `ollamacode` | Ollama (server lifecycle, model TUI, context detection) | `OPENAI_BASE_URL=http://<host>:<port>/v1`, `CLAW_RESILIENCE=force` |
| `openroutercode` | OpenRouter (tool-capable model browser) | `OPENAI_BASE_URL=https://openrouter.ai/api/v1/chat/completions`, `CLAW_RESILIENCE=none` |

Example:

```bash
ollamacode --model qwen3:14b        # start/connect Ollama, pick context, launch claw
lmcode                              # auto-discover LM Studio and pick a model
openroutercode                      # browse OpenRouter's tool-capable catalog
```

### `openroutercode` config directory and legacy fallback

The `openroutercode` launcher (and `claw setup openrouter`) stores the OpenRouter
API key and saved-model state under:

```text
~/.config/openroutercode/.env        # OPENROUTER_API_KEY=...
~/.config/openroutercode/selected_model
~/.config/openroutercode/recent_models
```

For users migrating from the previously-named `opencode` launcher, the fork falls
back to the **legacy** `~/.config/opencode/.env` when
`~/.config/openroutercode/` does not yet exist, so existing keys keep working
without re-entry.

## `claw mcp serve` sub-agent tools

`claw mcp serve` runs a stdio MCP server exposing a purpose-built sub-agent
surface — `run_subagent`, `start_subagent`, `get_subagent`, and `list_presets` —
so a parent agent (Claude Code, openclaw, or any MCP client) can spawn a local or
OpenRouter sub-agent and get bounded artifacts back instead of the whole raw
toolbox. Use `run_subagent` for a single inline result; use `start_subagent` and
poll `get_subagent` for long work where the parent needs status, activity, and
observed read paths.

`run_subagent` takes a `provider`
(`local|openrouter|ollama|lmstudio|anthropic|openai|auto`) and an optional
`model`. With no `model` it defaults to a **DeepSeek** model via OpenRouter
(`CLAW_SUBAGENT_MODEL`, then the built-in `deepseek/deepseek-v4-pro:nitro` default);
`provider=local|ollama|lmstudio` requires the local server running **and** an
explicit `model` (there is no local default). See the README's
"Using claw as an MCP sub-agent server" section for the full tool contract.
