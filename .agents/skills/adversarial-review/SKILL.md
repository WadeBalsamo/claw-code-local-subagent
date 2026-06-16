---
name: adversarial-review
description: Get an independent, adversarial second opinion on a plan or implementation from a read-only claw-code sub-agent (DeepSeek-V4-Pro via OpenRouter by default). Use after writing a plan and after finishing an implementation/diff, before declaring work done. The reviewer re-derives the critique from scratch and hunts for incorrect/inert logic, missing edge cases, false-green tests, security regressions, and scope/plan deviations. It cannot modify the workspace.
---

# Adversarial review (claw-code as a read-only second opinion)

You (Codex) are the master. This skill delegates an **adversarial review** to a
separate, disinterested model running as a **read-only** claw-code sub-agent over OpenRouter.
Because the reviewer has no stake in the plan and re-derives its critique from the raw plan/diff,
it catches the failure modes a self-review tends to rationalize: inert code, false-green tests,
and silent scope creep.

The review rubric is bundled next to this file as `rubric.md`. Read that file from the resolved
skill directory and use it verbatim. In this repo the source path is
`.agents/skills/adversarial-review/rubric.md`; after a user-level Codex install it is usually
`${CODEX_HOME:-~/.codex}/skills/adversarial-review/rubric.md`. The repo hook driver reads the
project copy first, then the user-level Codex copy, then legacy Claude skill locations for
compatibility.

## When to run it

- **After writing a plan**, before you start implementing it.
- **After finishing an implementation**, before you report the work as done.
- Whenever a change touches correctness-, security-, or test-sensitive code.

## How to run it

Call the MCP tool `mcp__claw-subagents__run_subagent` with a read-only Explore sub-agent for a
quick synchronous review. For a long or recursive review, call
`mcp__claw-subagents__start_subagent` with the same payload, then poll
`mcp__claw-subagents__get_subagent` with the returned `run_id` or `status_file`. Build the prompt
from the bundled rubric (read `rubric.md` next to this `SKILL.md` and use it verbatim), then the
**user's original request and a short context summary**, then the artifact under review (the plan
text, or the diff):

```json
{
  "prompt": "<rubric verbatim>\n\n=== USER'S ORIGINAL REQUEST ===\n<the user's first prompt>\n\n=== CONVERSATION CONTEXT ===\n<your terse summary of the goal, constraints, key decisions, and what changed>\n\n=== ARTIFACT UNDER REVIEW ===\n<plan text and/or `git diff` output>",
  "provider": "openrouter",
  "permission_mode": "read-only",
  "subagent_type": "Explore",
  "repo_dir": "<absolute path to the repo>",
  "max_output_chars": 8000,
  "review_depth": "deep",
  "focus": "correctness,tests,security,scope",
  "artifact_scope": "diff_plus_tests",
  "require_evidence": true
}
```

Pollable status example:

```json
{
  "run_id": "<run_id returned by start_subagent>",
  "activity_limit": 20,
  "event_limit": 20,
  "stale_after_secs": 300
}
```

You already hold the conversation, so write the request + context blocks yourself — the reviewer
cannot judge scope/plan deviations without knowing what was actually asked. (The automatic hook path
reconstructs the same two blocks from the session transcript, summarizing long conversations with a
single OpenRouter call before the reviewer starts.)

Notes:
- **Read-only is enforced** (two layers): `subagent_type: "Explore"` restricts the tool set to
  exactly `read_file`, `grep_search`, `glob_search`, `WebFetch`, `WebSearch`, `ToolSearch`, `Skill`,
  `StructuredOutput` — all read/search/report only — and `permission_mode: "read-only"` applies a
  `PermissionEnforcer` policy on top. There is no edit/write/bash-mutation path. A non-empty
  `diff_stat` in the result would indicate the reviewer changed something; it should always be empty.
- **Model selection**: for a direct MCP call, an explicit `model` wins, else `CLAW_SUBAGENT_MODEL`,
  else the default `deepseek/deepseek-v4-pro:nitro` OpenRouter Nitro route. The hook driver
  additionally honors `CLAW_REVIEW_MODEL`, which takes precedence over `CLAW_SUBAGENT_MODEL`.
  All resolve against OpenRouter.
- **OpenRouter endpoint**: use `OPENAI_BASE_URL=https://openrouter.ai/api/v1/chat/completions`
  with model slug `deepseek/deepseek-v4-pro:nitro`.
- **Recursive/deep reviews**: use `start_subagent` plus `get_subagent` polling instead of adding a
  short wall-clock timeout. `get_subagent` reports the current `status`, `phase`, `pid`,
  best-effort `worker_alive`, `stale`, recent JSONL `events`, observed `activity`, `read_paths`,
  `grep_patterns`, and `web_queries` while the reviewer is still running. If it is stale and you
  decide it is actually frozen, cancel it with `mcp__claw-subagents__stop_subagent` or the returned
  `stop_command`.
- Pass enough context: include the diff (`git diff` and `git diff --staged`) and, for a plan
  review, the full plan. The reviewer can read the rest of the repo itself.

## The review rubric (put this in the `prompt`)

The rubric is maintained as the bundled `rubric.md` file next to this `SKILL.md`. Read that file
and use its contents verbatim as the prefix of the `prompt`, followed by the artifact under review.
Don't paraphrase or restate it here.

## Automatic enforcement (hooks)

This repo can also run the same review **automatically**, even if you never invoke the skill:
- `PostToolUse(ExitPlanMode)` reviews the plan; `Stop` reviews the working-tree diff. The Codex
  hook configuration is project-scoped in `.codex/hooks.json` and driven by
  `scripts/adversarial_review.sh`; installing this skill user-level does not install those hooks.
- A `VERDICT: BLOCK` is **enforced**: the hook returns `{"decision":"block"}`, so you cannot finish
  until the findings are addressed (or rebutted with evidence) and a re-review returns `VERDICT: PASS`.
- Each run is a **real OpenRouter API call** (cost + latency). The automatic hook uses the
  pollable `start`/`status` path by default and has no review timeout unless `CLAW_REVIEW_TIMEOUT`
  or `adversarialReview.timeoutSecs` is explicitly set.
- The hook **fails open but loudly**: if `OPENROUTER_API_KEY` is unset, the `claw` binary is missing,
  or an explicit reviewer timeout fires, the review is skipped with a visible banner +
  `systemMessage` — never silently. Treat a skip as "no review ran" and fall back to your own
  judgment.

## How to act on the result

The synchronous tool returns structured JSON: `{status, provider, model, repo_dir, summary,
truncated, diff_stat, error, duration_ms}`. The pollable path returns `{run_id, status_file, pid}`
from `start_subagent`; `get_subagent` then returns the same final fields plus `phase`, `pid`,
best-effort `worker_alive`, `stale`, `events`, `activity`, `read_paths`, `grep_patterns`, and
`web_queries`. The reviewer's findings are in `summary` once `status` is `completed`.

1. If `status` is not `completed` (e.g. `failed`/`timeout`, or missing `OPENROUTER_API_KEY`),
   report that the review could not run and proceed with your own judgment — do not silently skip.
2. If the verdict is **BLOCK**: address each blocking finding (or explicitly rebut it with
   evidence if you believe it is wrong), then re-run this skill until the verdict is PASS or only
   rebuttals remain. Do not report the work done while a real blocking finding stands.
3. If the verdict is **PASS**: note any nits, apply the cheap ones, and proceed.

Always surface the reviewer's verdict and the key findings to the user — do not bury them.
