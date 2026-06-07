# Upstream Sync Review — ultraworkers/claw-code `main`

> **Date:** 2026-06-07
> **Verdict:** ⚠️ **Manual review required.** An automated merge of upstream `main`
> is **not safe** and was **not performed**. No merge PR was opened.
>
> _(This document substitutes for a tracking issue because GitHub Issues are
> disabled on this fork.)_

## Summary

I checked `ultraworkers/claw-code@main` for commits not yet in this fork and
evaluated merging them. Upstream is far ahead, but the two histories have
diverged deeply in exactly the files this fork customizes — local-model
providers, subagent tooling, MCP, and resilience. A straight merge would leave
the tree in conflict and would break fork-specific behavior. The upstream work
should be ported manually and in stages instead.

## Divergence facts

- **Common ancestor:** `650a24b` — _"feat: terminal markdown rendering with ANSI
  colors"_, **2026-04-01** (~2 months ago).
- **Upstream ahead:** **1448 commits** on `ultraworkers/claw-code@main`
  (HEAD `3acb677`) not in this fork.
- **Fork ahead:** **349 custom commits** not in upstream (local-model
  compatibility, subagent capability, resilience/self-healing, custom installer
  + launchers, rebranding).
- **Upstream change scope since divergence:** 271 files changed,
  **+70,037 / −15,683**.

## Why this is risky, not routine

A no-commit test merge of `upstream/main` into `claude/blissful-tesla-6lYfo`
produced:

- **71 conflicted files** — **25** content conflicts (`UU`), **43** add/add
  conflicts (`AA`), plus modify/delete conflicts. `AA` conflicts mean both sides
  independently created files with the same name: a hallmark of parallel,
  divergent development of the same feature.
- **68 conflict markers in `rust/crates/api/src/providers/openai_compat.rs`
  alone** — the core local-model OpenAI-compatible provider.
- **165 of upstream's 271 changed files (61%) overlap directly with files the
  fork modified.** The overlap covers every critical fork surface:
  - `rust/crates/api/src/providers/openai_compat.rs`, `providers/anthropic.rs`,
    `providers/mod.rs`, `client.rs`, `http_client.rs`
  - `rust/crates/runtime/src/config.rs`, `config_validate.rs`, `mcp*.rs`,
    `recovery_recipes.rs`, `lane_events.rs`
  - `rust/crates/tools/src/lane_completion.rs`
  - `docs/MODEL_COMPATIBILITY.md`, `docs/local-openai-compatible-providers.md`,
    `install.sh`

### Concrete example of the incompatibility

In `openai_compat.rs`, the fork added an entire resilience subsystem that
upstream has no knowledge of, while upstream independently rewrote the same file:

```
<<<<<<< HEAD (fork)
use crate::local_model_recovery::{
    ErrorClassifier, HealthProfileCache, ProviderCapabilities, RecoveryContext,
    RecoveryStateMachine, RetryableErrorKind,
};
use crate::resilience_config::ResilienceConfig;
======= (upstream/main)
>>>>>>>
```

These are two divergent implementations of the same module, not complementary
edits. Auto-merge cannot reconcile them; each hunk needs a human decision about
which behavior wins.

## Compatibility assessment

| Fork capability | Status under an upstream merge |
|---|---|
| Local-model compatibility (LMStudio / OpenRouter / DeepSeek / OpenAI base URL) | **At risk** — `openai_compat.rs`, `client.rs`, `config.rs` all conflict; upstream rewrote provider routing (`bcc5bfd route local OpenAI-compatible models`, `54d785d preserve DeepSeek V4 thinking history`). |
| Subagent capability (`run_subagent`, presets, GPU queue) | **At risk** — upstream landed its own Agent delegation (`ba220d2 Enable real Agent tool delegation`, multiple `rcc/subagent` merges) that overlaps the fork's MCP/preset integration. |
| Resilience / self-healing (`local_model_recovery`, `resilience_config`) | **At risk** — fork-only modules wired into files upstream rewrote; 68 conflict markers in one file. |
| Custom installer + launchers, rebranding, README/docs | **Conflicts** — `install.sh`, `README.md`, `USAGE.md` all conflict. |

There is **no cleanly separable "safe subset"** to merge via PR: even the
docs-only upstream commits (e.g. `README.md`) conflict with the fork's rewrites,
and the non-conflicting upstream additions depend on the rewritten APIs, so they
can't be cherry-picked in isolation.

## Recommendation — manual, staged integration

1. **Do not auto-merge `upstream/main`.** It would leave 71 conflicts and a
   broken build.
2. Decide direction first: rebase the fork onto upstream, or selectively port
   upstream improvements onto the fork. Given the fork's deep customizations,
   **porting upstream features onto the fork** is likely safer than rebasing.
3. Port high-value upstream work deliberately, re-applying the fork's
   resilience/subagent layers on top and validating with `scripts/fmt.sh
   --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and
   `cargo test --workspace` after each step. Candidates worth a manual port:
   - **`be8112f` — native Ollama provider via `OLLAMA_HOST`** (plus roadmap
     `eaa2e32`): directly complements the fork's local-model focus; reconcile
     against the fork's provider routing.
   - DeepSeek V4 reasoning/thinking fixes (`54d785d`, `fdcb05b`, `75c08bc`).
   - Permission-enforcer hardening (`5b15197`).
   - The broad JSON/resume-mode CLI contract fixes, if the fork relies on those
     surfaces.
4. Treat each ported area as its own reviewable change against the fork's tests,
   rather than one mega-merge.

## Reproduction

```sh
git remote add upstream https://github.com/ultraworkers/claw-code.git
git fetch upstream main
git log --oneline origin/main..upstream/main        # 1448 commits
git merge --no-commit --no-ff upstream/main          # 71 conflicts, then: git merge --abort
```
