---
name: adversarial-review
description: Get an independent, adversarial second opinion on a plan or implementation from a read-only claw-code sub-agent (DeepSeek-V4-Pro via OpenRouter by default). Use after writing a plan and after finishing an implementation/diff, before declaring work done. The reviewer re-derives the critique from scratch and hunts for incorrect/inert logic, missing edge cases, false-green tests, security regressions, and scope/plan deviations. It cannot modify the workspace.
---

# Adversarial review (claw-code as a read-only second opinion)

You (Claude Code) are the master. This skill delegates an **adversarial review** to a
separate, disinterested model running as a **read-only** claw-code sub-agent over OpenRouter.
Because the reviewer has no stake in the plan and re-derives its critique from the raw plan/diff,
it catches the failure modes a self-review tends to rationalize: inert code, false-green tests,
and silent scope creep.

This skill is the canonical rubric. The same review also runs automatically via the `Stop` and
`PostToolUse(ExitPlanMode)` hooks in `.claude/settings.json` (driver:
`scripts/adversarial_review.sh`); invoke this skill directly when you want a deeper or ad-hoc
review, or to re-review after addressing findings.

## When to run it

- **After writing a plan**, before you start implementing it.
- **After finishing an implementation**, before you report the work as done.
- Whenever a change touches correctness-, security-, or test-sensitive code.

## How to run it

Call the MCP tool `mcp__claw-subagents__run_subagent` with a read-only Explore sub-agent. Build
the prompt from the rubric below plus the artifact under review (the plan text, or the diff):

```json
{
  "prompt": "<the rubric below>\n\n=== ARTIFACT UNDER REVIEW ===\n<plan text and/or `git diff` output>",
  "provider": "openrouter",
  "permission_mode": "read-only",
  "subagent_type": "Explore",
  "repo_dir": "<absolute path to the repo>",
  "max_output_chars": 8000
}
```

Notes:
- **Read-only is enforced**: `permission_mode: "read-only"` + `subagent_type: "Explore"` give the
  reviewer only `read_file`/`grep_search`/`glob_search`/web tools — no edit/write/bash-mutation.
  A non-empty `diff_stat` in the result would indicate the reviewer changed something; it should
  always be empty.
- **Model is configurable**: omit `model` to use the default `deepseek/deepseek-v4-pro`, or set the
  `CLAW_SUBAGENT_MODEL` env var (or pass `model`) to repoint at another OpenRouter model. The hook
  driver reads `CLAW_REVIEW_MODEL` for the same purpose.
- Pass enough context: include the diff (`git diff` and `git diff --staged`) and, for a plan
  review, the full plan. The reviewer can read the rest of the repo itself.

## The review rubric (put this in the `prompt`)

> You are an adversarial code reviewer. You are NOT the author and have no stake in the plan being
> right — your job is to find what is wrong before it ships. You are read-only.
>
> Re-derive the critique from first principles. Verify claims against the actual code; do not trust
> comments or commit messages. Hunt specifically for:
> - **Incorrect or inert logic**: code that compiles but does nothing, is never called, or doesn't
>   match the stated intent.
> - **Missing edge cases / error handling**: empty/None, overflow, concurrency, partial failure,
>   untrusted input.
> - **False-green or placeholder tests**: tests that assert nothing meaningful, are skipped/ignored,
>   are tautological, or mock away the thing under test.
> - **Security regressions**: injection, path traversal, secret leakage, auth/permission bypass.
> - **Scope / plan deviations**: changes outside the stated plan, dropped requirements, silently
>   weakened behavior.
>
> Cite `file:line` for every finding. Be concrete and terse. End with exactly one verdict line:
> `VERDICT: BLOCK` (followed by a numbered list of blocking issues, each with `file:line` and a
> one-line fix) or `VERDICT: PASS` (with a one-line rationale). Minor style nits go under a
> separate `Nits:` heading and never trigger BLOCK.

## How to act on the result

The tool returns structured JSON: `{status, provider, model, repo_dir, summary, truncated,
diff_stat, error, duration_ms}`. The reviewer's findings are in `summary`.

1. If `status` is not `completed` (e.g. `failed`/`timeout`, or missing `OPENROUTER_API_KEY`),
   report that the review could not run and proceed with your own judgment — do not silently skip.
2. If the verdict is **BLOCK**: address each blocking finding (or explicitly rebut it with
   evidence if you believe it is wrong), then re-run this skill until the verdict is PASS or only
   rebuttals remain. Do not report the work done while a real blocking finding stands.
3. If the verdict is **PASS**: note any nits, apply the cheap ones, and proceed.

Always surface the reviewer's verdict and the key findings to the user — do not bury them.
