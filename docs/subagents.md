# claw-code subagents

A practical guide to using the built-in adversarial reviewer, tuning it, and
building your own subagents.

## 1. What a claw-code subagent is

A claw-code subagent is a **bounded, single-turn delegated task**. You hand it a
prompt; it runs in its own context with a restricted toolset, does the work, and
returns a structured result. It is not a long-lived conversational agent — one
prompt in, one result out.

Subagents are exposed through the curated MCP tools served by:

```bash
claw mcp serve --subagents
```

This is wired up in `.mcp.json` as the `claw-subagents` server:

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

A run returns a structured JSON object:

```json
{
  "status": "completed",
  "provider": "openrouter",
  "model": "deepseek/deepseek-v4-pro:nitro",
  "repo_dir": "/path/to/repo",
  "summary": "...the subagent's output...",
  "truncated": false,
  "diff_stat": "",
  "error": null,
  "duration_ms": 4213
}
```

**Read-only is enforced two ways**, independently:

1. The `subagent_type` (e.g. `Explore`) restricts the **tool allowlist** — an
   `Explore` agent simply cannot be handed edit/write/bash tools.
2. `permission_mode: "read-only"` applies a **PermissionEnforcer** that rejects
   mutating actions at the permission layer.

In addition, **reads are confined to `repo_dir`** by a workspace boundary check —
a subagent cannot wander outside its working directory.

## 2. Invoking a subagent — three paths

### a. The MCP tool

Call `mcp__claw-subagents__run_subagent` for a synchronous result, or
`mcp__claw-subagents__start_subagent` for a pollable background run, with:

| field              | required | notes                                              |
|--------------------|----------|----------------------------------------------------|
| `prompt`           | yes      | the task                                           |
| `provider`         | no       | `openrouter`, `local`, `lmstudio`, `ollama`, …     |
| `model`            | no       | defaults to `CLAW_SUBAGENT_MODEL`, then a DeepSeek model |
| `subagent_type`    | no       | e.g. `Explore` — restricts the tool allowlist      |
| `permission_mode`  | no       | `read-only`, `workspace-write`, `danger-full-access` |
| `repo_dir`         | no       | defaults to the server cwd                         |
| `max_output_chars` | no       | truncates the returned summary                     |
| `timeout_secs`     | no       | optional wall-clock bound; omit for long reviews   |
| `isolated`         | no       | run in a throwaway git worktree (see below)        |
| `preset`           | only if `isolated` | preset name to launch                     |
| `review_depth`     | no       | `quick`, `standard`, `deep`, or `exhaustive`       |
| `focus`            | no       | comma-separated review focus areas                 |
| `artifact_scope`   | no       | `diff_only`, `diff_plus_tests`, or `full_repo_context` |
| `stop_on_first_blocker` | no  | ask a reviewer to stop after one concrete blocker  |
| `require_evidence` | no       | ask a reviewer to cite concrete evidence           |

```json
{
  "prompt": "Summarize the error-handling strategy in crates/tools.",
  "subagent_type": "Explore",
  "permission_mode": "read-only"
}
```

`start_subagent` returns immediately with a `run_id`, `status_file`, CLI-ready
`status_command` and `stop_command`, plus worker `pid`. Poll with
`mcp__claw-subagents__get_subagent`:

```json
{ "run_id": "subagent-123", "activity_limit": 20, "event_limit": 20, "stale_after_secs": 300 }
```

The status record includes the current `status`, final `summary` when complete,
`phase`, `pid`, best-effort `worker_alive` while active, `stale`,
`stale_reason`, JSONL `events`, `activity` tool events, and observed
`read_paths`, `grep_patterns`, and `web_queries`. Omit `timeout_secs` for work
that should keep running until the agent completes. If a run is stale and you
decide it is truly frozen, call `mcp__claw-subagents__stop_subagent` or:

```bash
claw subagent stop "$run_id"
```

> **`isolated: true` requires a `preset`.** Without one it now **fails fast** with
> `status: "failed"` — it will *not* silently downgrade to an in-process run. The
> isolated launcher is preset-driven and has no ad-hoc provider/model flag, so
> pass a `preset` or drop `isolated`.

```json
{ "isolated": true, "preset": "qwen3-coder-30b", "prompt": "Fix the failing test in foo.rs" }
```

### b. The isolated launcher (git worktree + hard kill)

```bash
scripts/launchers/run-claw-code.sh --agent <preset> --dir <path> --plan "<task>"
```

This spawns a claw session in a temporary git worktree, waits for completion,
captures the diff/summary, and can hard-kill on timeout. It emits a 4-line
output contract: `task_id=<uuid>` followed by three bare file paths
(the `status.json`, the diff patch, and the summary), plus an optional
`pr_request=` line.

### c. The CLI

Synchronous:

```bash
claw subagent run \
  --provider openrouter \
  --permission-mode read-only \
  --subagent-type Explore \
  --model deepseek/deepseek-v4-pro:nitro \
  --repo-dir . \
  --max-output-chars 8000
```

Pollable:

```bash
run_id=$(
  claw subagent start \
    --provider openrouter \
    --permission-mode read-only \
    --subagent-type Explore \
    --model deepseek/deepseek-v4-pro:nitro \
    --repo-dir . \
    --prompt "Read README.md and summarize the first heading" |
  jq -r .run_id
)
claw subagent status "$run_id" --activity-limit 20
```

The prompt can also be read from stdin. The adversarial-review hook driver uses
the pollable path (`scripts/adversarial_review.sh`) so long reviews can expose
progress and stale state without a default wall-clock cap.

### Listing presets

The `list_presets` MCP tool enumerates every available preset (no inputs
required).

## 3. Presets — the easy way to define a subagent

A preset is a JSON file that fully describes a subagent. Presets live in
`scripts/presets/*.json` and `~/.lmcode/presets/*.json`.

The annotated template lives at
`scripts/presets/templates/subagent.template.json` (in a subdirectory, so the
template itself is **not** listed as a usable preset). Its fields:

| field             | meaning                                                            |
|-------------------|--------------------------------------------------------------------|
| `preset_name`     | unique id; how you select the subagent (`preset`, `--agent`)       |
| `description`     | one-line summary of what it is for                                 |
| `provider`        | `openrouter \| local \| lmstudio \| ollama \| anthropic \| openai \| auto` |
| `model`           | provider model id (e.g. `deepseek/deepseek-v4-pro:nitro`)                |
| `resource`        | broker hint: `remote \| gpu \| cpu \| gpu+cpu` (local-first scheduling) |
| `env`             | extra environment (e.g. `OPENAI_BASE_URL`, `OPENROUTER_API_KEY`)   |
| `system_prompt`   | the subagent's role/instructions; keep it self-contained          |
| `allowed_tools`   | tool whitelist — omit edit/write/bash for a read-only agent        |
| `max_context`     | max context tokens the model is told it has                        |
| `temperature`     | sampling temperature                                               |
| `permission_mode` | `read-only \| workspace-write \| danger-full-access`               |
| `plan_mode`       | `normal \| ultraplan \| plan-only`                                 |
| `budget_gate`     | enforce a spend gate for remote runs                               |
| `open_pr`         | open a PR with the result                                          |
| `timeout_seconds` | hard wall-clock bound for an isolated run                          |
| `tier_ref`        | *optional* orchestrator key into `config/model_tiers.json` for local→remote laddering |

Keys beginning with `_` (e.g. `_fields`) in the template are **documentation
only** and are stripped by the scaffold.

The real, shipped example is `scripts/presets/adversarial-review.json` — a
read-only reviewer pointed at DeepSeek-V4-Pro over OpenRouter, with a narrow
`allowed_tools` list (`read_file`, `grep_search`, `glob_search`, `WebFetch`,
`WebSearch`) and `permission_mode: "read-only"`.

## 4. Scaffolding a new subagent

```bash
scripts/new-subagent.sh <name> "one-line description"
```

This copies the template to `scripts/presets/<name>.json`, fills in
`preset_name` and `description`, and **strips the doc-only `_` keys**. Then you
edit `system_prompt`, `model`, and `allowed_tools`.

Worked example:

```bash
$ scripts/new-subagent.sh doc-summarizer "Summarize docs for a reviewer"
Created subagent preset: .../scripts/presets/doc-summarizer.json

Next steps:
  1. Edit .../scripts/presets/doc-summarizer.json — set system_prompt, model, provider, and allowed_tools.
  2. Run it via the subagent MCP tools:
       {"isolated": true, "preset": "doc-summarizer", "prompt": "<your task>"}
     Use start_subagent + get_subagent instead when you need pollable status.
     or the launcher:
       scripts/launchers/run-claw-code.sh --agent doc-summarizer --dir . --plan "<your task>"
```

The name must match `[A-Za-z0-9_-]`, and the script refuses to overwrite an
existing preset.

## 5. The built-in adversarial reviewer

The adversarial reviewer is a **read-only subagent** (DeepSeek-V4-Pro via
OpenRouter by default) whose job is to critique a plan or a diff from scratch —
hunting for incorrect or inert logic, missing edge cases, false-green or
placeholder tests, security regressions, and silent scope/plan deviations. It
**cannot modify the workspace**.

It runs automatically through two hooks in `.codex/hooks.json`, both driven
by `scripts/adversarial_review.sh`:

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "ExitPlanMode",
        "hooks": [{ "type": "command",
          "command": "bash \"${CODEX_PROJECT_DIR:-${CLAUDE_PROJECT_DIR:-$PWD}}/scripts/adversarial_review.sh\" --mode plan" }] }
    ],
    "Stop": [
      { "matcher": "",
        "hooks": [{ "type": "command",
          "command": "bash \"${CODEX_PROJECT_DIR:-${CLAUDE_PROJECT_DIR:-$PWD}}/scripts/adversarial_review.sh\" --mode implementation" }] }
    ]
  }
}
```

- `PostToolUse(ExitPlanMode)` → reviews the **plan**.
- `Stop` → reviews the **diff** (`git diff` + `git diff --staged`).

You can also run it on demand via the `/adversarial-review` skill.

**Installing the skill globally (user-level).** The skill ships project-scoped in
`.agents/skills/adversarial-review/`. To make it available in every project on the
machine, copy both files to the user-level skills directory (the skill reads
`rubric.md` by path, so both must travel together):

```bash
mkdir -p "${CODEX_HOME:-$HOME/.codex}/skills/adversarial-review"
cp .agents/skills/adversarial-review/{SKILL.md,rubric.md} "${CODEX_HOME:-$HOME/.codex}/skills/adversarial-review/"
```

The on-demand skill (MCP `run_subagent`, or `start_subagent` + `get_subagent` for
long reviews) and the headless `claw subagent run` / `claw subagent start` paths
only require the `claw` binary on `PATH` and `OPENROUTER_API_KEY` in the environment.
The automatic `Stop` / `PostToolUse(ExitPlanMode)` hooks remain project-scoped — they
run `scripts/adversarial_review.sh` from the repo that defines them.

> **`claw doctor` caveat:** the **Auth** check only looks at `ANTHROPIC_API_KEY` /
> `ANTHROPIC_AUTH_TOKEN` / `OPENAI_API_KEY` — not `OPENROUTER_API_KEY` or the
> `provider.apiKey` in `~/.claw/settings.json`. With OpenRouter it shows a harmless
> "no supported auth env vars were found" warning (0 failures); verify auth with a
> real call: `echo "ping" | claw subagent run --provider openrouter --permission-mode
> read-only --subagent-type Explore` should return `status: "completed"`.

If the reviewer returns `VERDICT: BLOCK`, the hook emits
`{"decision":"block", ...}` and **completion is blocked** until the findings are
addressed (or rebutted) and a re-review returns `VERDICT: PASS`.

Before invoking the reviewer, the driver prepends the **USER'S ORIGINAL REQUEST**
and a **CONVERSATION CONTEXT** block reconstructed from the session transcript —
the reviewer needs the user's intent to judge scope/plan deviations. Long
conversations are summarized first via a single OpenRouter call; short ones are
passed through (lightly trimmed).

**It fails open, loudly.** If `OPENROUTER_API_KEY` is unset, the claw binary is
missing, or an explicitly configured reviewer timeout fires, the review is
*skipped* with a visible banner and a `systemMessage` — never silently treated
as a clean pass, and never a hard block. By default the reviewer has no timeout;
the hook polls `claw subagent status`, prints phase/liveness/stale updates, and
prints the `claw subagent stop` command when a run looks stale.

## 6. Configuring the reviewer via claw-code settings

You can tune the reviewer **without env vars** by adding an `adversarialReview`
object to `.codex/settings.json` (shared) or `.codex/settings.local.json`
(machine-local override — it wins over the shared file).

Keys (validated by the config schema):

| key                      | type   | meaning                                                                 |
|--------------------------|--------|-------------------------------------------------------------------------|
| `enabled`                | bool   | set `false` to skip the review entirely                                 |
| `model`                  | string | reviewer model                                                          |
| `timeoutSecs`            | number | optional reviewer wall-clock bound; omit for no review timeout          |
| `staleAfterSecs`         | number | seconds without activity before polling reports stale                   |
| `pollSecs`               | number | seconds between hook status polls                                       |
| `reviewDepth`            | string | `quick`, `standard`, `deep`, or `exhaustive`                            |
| `focus`                  | string | comma-separated review focus areas                                      |
| `artifactScope`          | string | `diff_only`, `diff_plus_tests`, or `full_repo_context`                  |
| `stopOnFirstBlocker`     | bool   | ask reviewer to stop after one concrete blocker                         |
| `requireEvidence`        | bool   | ask reviewer to cite concrete evidence                                  |
| `contextMaxTokens`       | number | **token** cap on the conversation context summarized + sent (driver converts at ~4 chars/token; default 25000 tokens ≈ 100000 chars) |
| `contextThresholdTokens` | number | token count above which the conversation is summarized rather than sent raw |
| `contextModel`           | string | model used for the summary                                              |
| `contextTimeoutSecs`     | number | timeout for the summary call                                            |

**Precedence: environment variable > settings file > built-in default.**

Matching env overrides:

| env var                         | overrides                                            |
|---------------------------------|------------------------------------------------------|
| `CLAW_REVIEW_MODEL`             | reviewer model                                       |
| `CLAW_SUBAGENT_MODEL`           | reviewer model (fallback)                            |
| `CLAW_REVIEW_TIMEOUT`           | optional reviewer timeout (secs)                     |
| `CLAW_REVIEW_STALE_AFTER`       | stale threshold (secs)                               |
| `CLAW_REVIEW_POLL_SECS`         | poll interval (secs)                                 |
| `CLAW_REVIEW_DEPTH`             | review depth                                         |
| `CLAW_REVIEW_FOCUS`             | review focus areas                                   |
| `CLAW_REVIEW_ARTIFACT_SCOPE`    | review artifact scope                                |
| `CLAW_REVIEW_STOP_ON_FIRST_BLOCKER` | stop-on-first-blocker hint                       |
| `CLAW_REVIEW_REQUIRE_EVIDENCE`  | evidence-required hint                               |
| `CLAW_REVIEW_CONTEXT_MODEL`     | summary model                                        |
| `CLAW_REVIEW_CONTEXT_THRESHOLD` | summarize threshold, in **chars**                    |
| `CLAW_REVIEW_CONTEXT_TIMEOUT`   | summary call timeout (secs)                          |
| `CLAW_REVIEW_CONTEXT_MAX`       | context cap, in **chars**                            |
| `OPENROUTER_API_KEY`            | required; without it the review is skipped (fail open) |

> Note the unit difference: the **settings** keys are in *tokens*; the
> `CLAW_REVIEW_CONTEXT_*` **env** overrides are in *chars*. The driver converts
> token-based settings at ~4 chars/token.

Complete example `.codex/settings.json` snippet:

```json
{
  "permissions": {
    "allow": [
      "mcp__claw-subagents__run_subagent",
      "mcp__claw-subagents__start_subagent",
      "mcp__claw-subagents__get_subagent",
      "mcp__claw-subagents__stop_subagent",
      "mcp__claw-subagents__list_presets"
    ]
  },
  "adversarialReview": {
    "enabled": true,
    "model": "deepseek/deepseek-v4-pro:nitro",
    "staleAfterSecs": 300,
    "pollSecs": 15,
    "reviewDepth": "deep",
    "focus": "correctness,tests,security,scope",
    "artifactScope": "diff_plus_tests",
    "stopOnFirstBlocker": false,
    "requireEvidence": true,
    "contextMaxTokens": 25000,
    "contextThresholdTokens": 3000,
    "contextModel": "deepseek/deepseek-v4-pro:nitro",
    "contextTimeoutSecs": 60
  },
  "hooks": {
    "PostToolUse": [
      { "matcher": "ExitPlanMode",
        "hooks": [{ "type": "command",
          "command": "bash \"${CODEX_PROJECT_DIR:-${CLAUDE_PROJECT_DIR:-$PWD}}/scripts/adversarial_review.sh\" --mode plan" }] }
    ],
    "Stop": [
      { "matcher": "",
        "hooks": [{ "type": "command",
          "command": "bash \"${CODEX_PROJECT_DIR:-${CLAUDE_PROJECT_DIR:-$PWD}}/scripts/adversarial_review.sh\" --mode implementation" }] }
    ]
  }
}
```

## 7. Build your own: a `security-reviewer` subagent

Scaffold it:

```bash
scripts/new-subagent.sh security-reviewer "Read-only security reviewer for the current diff"
```

Edit `scripts/presets/security-reviewer.json` — keep it read-only and give it a
focused system prompt:

```json
{
  "preset_name": "security-reviewer",
  "description": "Read-only security reviewer for the current diff.",
  "provider": "openrouter",
  "model": "deepseek/deepseek-v4-pro:nitro",
  "resource": "remote",
  "env": {
    "OPENAI_BASE_URL": "https://openrouter.ai/api/v1/chat/completions",
    "OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"
  },
  "system_prompt": "You are a read-only application-security reviewer. Given a diff, hunt only for security regressions: injection, path traversal, SSRF, secret leakage, auth/permission bypass, unsafe deserialization, and missing input validation on untrusted data. Cite file:line for every finding and propose a one-line fix. End with exactly one of 'VERDICT: BLOCK' (numbered blocking issues) or 'VERDICT: PASS' (one-line rationale).",
  "allowed_tools": ["read_file", "grep_search", "glob_search"],
  "max_context": 131072,
  "temperature": 0.1,
  "permission_mode": "read-only",
  "plan_mode": "normal",
  "budget_gate": true,
  "open_pr": false,
  "timeout_seconds": 600
}
```

Invoke it via the MCP tool (isolated):

```json
{ "isolated": true, "preset": "security-reviewer", "prompt": "Review the current diff for security regressions." }
```

or via the launcher:

```bash
scripts/launchers/run-claw-code.sh --agent security-reviewer --dir . --plan "Review the current diff for security regressions."
```

Because it ships with `permission_mode: "read-only"` and a read-only
`allowed_tools` list, this subagent can read and search the repo but never write
to it.
