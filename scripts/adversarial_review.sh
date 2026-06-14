#!/usr/bin/env bash
#
# Adversarial-review hook driver.
#
# Invoked by Claude Code's hooks (see .claude/settings.json):
#   - PostToolUse matcher ExitPlanMode  -> --mode plan          (review the plan)
#   - Stop                              -> --mode implementation (review the diff)
#
# It runs a READ-ONLY claw-code sub-agent over OpenRouter to critique the
# artifact and maps the reviewer's verdict to a Claude Code hook decision:
# a `VERDICT: BLOCK` becomes {"decision":"block","reason":...} so Claude must
# address the findings before finishing; anything else lets Claude proceed.
#
# Design rule: FAIL OPEN. A missing key, offline reviewer, or parse error must
# never hard-block the session — it degrades to "no review" with a stderr note.
#
# Configuration (env):
#   CLAW_REVIEW_MODEL     reviewer model (default: CLAW_SUBAGENT_MODEL, else deepseek/deepseek-v4-pro)
#   CLAW_BIN              path to the claw binary (default: claw on PATH)
#   CLAW_REVIEW_TIMEOUT   hard wall-clock bound on the reviewer, seconds (default: 180)
#   OPENROUTER_API_KEY    required; without it the review is skipped (fail open)

set -uo pipefail

MODE="implementation"
while [ $# -gt 0 ]; do
  case "$1" in
    --mode) MODE="${2:-implementation}"; shift 2 ;;
    --mode=*) MODE="${1#*=}"; shift ;;
    *) shift ;;
  esac
done

PAYLOAD="$(cat 2>/dev/null || true)"

PY="$(command -v python3 || command -v python || true)"
CLAW_BIN="${CLAW_BIN:-claw}"
REVIEW_MODEL="${CLAW_REVIEW_MODEL:-${CLAW_SUBAGENT_MODEL:-deepseek/deepseek-v4-pro}}"
REVIEW_TIMEOUT="${CLAW_REVIEW_TIMEOUT:-180}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || pwd)"

note() { printf 'adversarial-review: %s\n' "$1" >&2; }
fail_open() { note "$1 — skipping review"; exit 0; }

command -v "$CLAW_BIN" >/dev/null 2>&1 || fail_open "claw binary '$CLAW_BIN' not found"
[ -n "$PY" ] || fail_open "python3 not found"
[ -n "${OPENROUTER_API_KEY:-}" ] || fail_open "OPENROUTER_API_KEY not set"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
printf '%s' "$PAYLOAD" > "$TMP/payload.json"

# --- Assemble the artifact under review --------------------------------------
if [ "$MODE" = "plan" ]; then
  PLAN="$("$PY" - "$TMP/payload.json" <<'PYEOF'
import sys, json
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    d = {}
ti = d.get("tool_input") or {}
print(ti.get("plan") or "")
PYEOF
)"
  if [ -z "${PLAN//[[:space:]]/}" ]; then
    fail_open "no plan text found in hook payload"
  fi
  KIND="plan"
  ARTIFACT="=== PLAN UNDER REVIEW ===
${PLAN}"
else
  # Stop hook: avoid infinite loops — if we're already inside a stop-hook
  # continuation, surface nothing and let Claude stop.
  ACTIVE="$("$PY" - "$TMP/payload.json" <<'PYEOF'
import sys, json
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    d = {}
print("1" if d.get("stop_hook_active") else "0")
PYEOF
)"
  if [ "$ACTIVE" = "1" ]; then
    note "already in a stop-hook continuation; not re-blocking"
    exit 0
  fi
  DIFF="$( { git -C "$REPO_DIR" diff; git -C "$REPO_DIR" diff --staged; } 2>/dev/null )"
  if [ -z "${DIFF//[[:space:]]/}" ]; then
    note "no uncommitted changes to review"
    exit 0
  fi
  KIND="implementation"
  ARTIFACT="=== DIFF UNDER REVIEW (git diff + git diff --staged) ===
${DIFF}"
fi

# --- The adversarial rubric (kept in sync with the preset + SKILL.md) --------
read -r -d '' RUBRIC <<'RUBRIC_EOF' || true
You are an adversarial code reviewer. You are NOT the author and have no stake in the plan being right — your job is to find what is wrong before it ships. You are read-only.

Re-derive the critique from first principles. Verify claims against the actual code; do not trust comments or commit messages. Hunt specifically for:
- Incorrect or inert logic: code that compiles but does nothing, is never called, or doesn't match the stated intent.
- Missing edge cases / error handling: empty/None, overflow, concurrency, partial failure, untrusted input.
- False-green or placeholder tests: tests that assert nothing meaningful, are skipped/ignored, are tautological, or mock away the thing under test.
- Security regressions: injection, path traversal, secret leakage, auth/permission bypass.
- Scope / plan deviations: changes outside the stated plan, dropped requirements, silently weakened behavior.

Cite file:line for every finding. Be concrete and terse. End with exactly one verdict line: "VERDICT: BLOCK" (followed by a numbered list of blocking issues, each with file:line and a one-line fix) or "VERDICT: PASS" (with a one-line rationale). Minor style nits go under a separate "Nits:" heading and never trigger BLOCK.
RUBRIC_EOF

PROMPT="${RUBRIC}

${ARTIFACT}"

# --- Run the read-only reviewer ----------------------------------------------
# Hard-bound the call: the restored resilience layer retries failed network
# calls with backoff, so an unreachable/misconfigured endpoint must not stall
# the session. `timeout` exit 124 is treated as a fail-open skip.
TIMEOUT_BIN="$(command -v timeout || true)"
reviewer() {
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$REVIEW_TIMEOUT" "$CLAW_BIN" subagent run "$@"
  else
    "$CLAW_BIN" subagent run "$@"
  fi
}
printf '%s' "$PROMPT" | reviewer \
  --provider openrouter \
  --permission-mode read-only \
  --subagent-type Explore \
  --model "$REVIEW_MODEL" \
  --repo-dir "$REPO_DIR" \
  --timeout-secs "$REVIEW_TIMEOUT" \
  --max-output-chars 8000 \
  > "$TMP/result.json" 2> "$TMP/err"
rc=$?
if [ "$rc" != "0" ]; then
  if [ "$rc" = "124" ]; then
    fail_open "reviewer timed out after ${REVIEW_TIMEOUT}s"
  fi
  fail_open "reviewer invocation failed: $(head -c 200 "$TMP/err" 2>/dev/null)"
fi

STATUS="$("$PY" - "$TMP/result.json" <<'PYEOF'
import sys, json
try:
    print(json.load(open(sys.argv[1])).get("status", ""))
except Exception:
    print("")
PYEOF
)"

if [ "$STATUS" != "completed" ]; then
  fail_open "reviewer did not complete (status=${STATUS:-unknown})"
fi

# --- Map the verdict to a hook decision --------------------------------------
# Block ONLY on an explicit "VERDICT: BLOCK". A truncated/missing verdict fails
# open so the reviewer can never wedge the session on ambiguous output.
DECISION="$("$PY" - "$TMP/result.json" "$KIND" <<'PYEOF'
import sys, json
try:
    res = json.load(open(sys.argv[1]))
except Exception:
    print("")
    sys.exit(0)
kind = sys.argv[2]
summary = res.get("summary", "") or ""
if "VERDICT: BLOCK" in summary:
    reason = (
        "Adversarial review of the %s found blocking issues. Address each finding "
        "(or rebut it with concrete evidence if it is wrong), then do not finish "
        "until a re-review returns VERDICT: PASS.\n\nReviewer (%s via %s):\n%s"
        % (kind, res.get("model", "?"), res.get("provider", "?"), summary)
    )
    print(json.dumps({"decision": "block", "reason": reason}))
else:
    print("")
PYEOF
)"

if [ -n "$DECISION" ]; then
  printf '%s\n' "$DECISION"
  exit 0
fi

note "review passed (no blocking findings)"
exit 0
