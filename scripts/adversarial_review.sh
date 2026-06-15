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
# Design rule: FAIL OPEN, BUT LOUDLY. A missing key, offline reviewer, or parse
# error must never hard-block the session — it degrades to "no review", but with
# a visible banner + systemMessage so the skip is never mistaken for a clean pass.
#
# Before invoking the reviewer we prepend the USER'S ORIGINAL REQUEST and a
# CONVERSATION CONTEXT block, reconstructed from the hook payload's
# `transcript_path`. The reviewer needs the user's intent to judge scope/plan
# deviations — the plan/diff alone doesn't say what was actually asked. For long
# transcripts we summarize the conversation with a single OpenRouter call first;
# short ones are passed through (lightly trimmed). This is best-effort: if the
# transcript is unavailable or the summary call fails, we degrade to whatever
# context we have (at minimum the first prompt) and never block.
#
# Configuration. Everything below is also settable (without env vars) in the
# claw-code settings file under an `adversarialReview` object — see
# docs/subagents.md. Precedence: env var > settings file > built-in default.
#
#   Settings keys (.claude/settings.json -> "adversarialReview"):
#     enabled                 set false to skip the review entirely (CLAW_REVIEW_FORCE overrides)
#     model                   reviewer model
#     timeoutSecs             hard wall-clock bound on the reviewer
#     contextMaxTokens        token cap on the context summarized + sent to the reviewer
#     contextThresholdTokens  token count above which the conversation is summarized
#     contextModel            model used for the conversation summary
#     contextTimeoutSecs      timeout for the summary OpenRouter call
#
#   Env overrides:
#     CLAW_REVIEW_MODEL / CLAW_SUBAGENT_MODEL   reviewer model (default deepseek/deepseek-v4-pro)
#     CLAW_BIN                                  path to the claw binary (default: claw on PATH)
#     CLAW_REVIEW_TIMEOUT                       reviewer timeout, seconds (default 180)
#     OPENROUTER_API_KEY                        required; without it the review is skipped (fail open)
#     CLAW_REVIEW_CONTEXT_MODEL                 model for the conversation summary
#     CLAW_REVIEW_CONTEXT_THRESHOLD             context CHAR count above which we summarize (default 12000)
#     CLAW_REVIEW_CONTEXT_TIMEOUT               summary call timeout, seconds (default 60)
#     CLAW_REVIEW_CONTEXT_MAX                   CHAR cap on the context block (default 100000 = 25k tokens)

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
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || pwd)"

note() { printf 'adversarial-review: %s\n' "$1" >&2; }

# --- Settings (claw-code config) --------------------------------------------
# Read the `adversarialReview` block from the project's claw-code settings so
# the reviewer + context budget are configurable WITHOUT env vars. Precedence
# is: env var > settings file > built-in default. `.claude/settings.local.json`
# (machine-local) overrides `.claude/settings.json` (shared). Token-based
# settings are converted to the script's internal char budget at ~4 chars/token.
CHARS_PER_TOKEN=4
AR_MODEL=""; AR_TIMEOUT=""; AR_CTX_MODEL=""; AR_CTX_TIMEOUT=""
AR_CTX_MAX_TOKENS=""; AR_CTX_THRESHOLD_TOKENS=""; AR_ENABLED=""
if [ -n "${PY:-}" ]; then
  while IFS='=' read -r _k _v; do
    case "$_k" in
      AR_MODEL) AR_MODEL="$_v" ;;
      AR_TIMEOUT) AR_TIMEOUT="$_v" ;;
      AR_CTX_MODEL) AR_CTX_MODEL="$_v" ;;
      AR_CTX_TIMEOUT) AR_CTX_TIMEOUT="$_v" ;;
      AR_CTX_MAX_TOKENS) AR_CTX_MAX_TOKENS="$_v" ;;
      AR_CTX_THRESHOLD_TOKENS) AR_CTX_THRESHOLD_TOKENS="$_v" ;;
      AR_ENABLED) AR_ENABLED="$_v" ;;
    esac
  done <<EOF
$(CC_REPO="$REPO_DIR" "$PY" - <<'PYEOF' 2>/dev/null || true
import os, json
repo = os.environ.get("CC_REPO", ".")
cfg = {}
for name in (".claude/settings.json", ".claude/settings.local.json"):
    try:
        with open(os.path.join(repo, name)) as f:
            ar = json.load(f).get("adversarialReview")
        if isinstance(ar, dict):
            cfg.update(ar)
    except Exception:
        pass
keymap = {
    "model": "AR_MODEL", "timeoutSecs": "AR_TIMEOUT",
    "contextModel": "AR_CTX_MODEL", "contextTimeoutSecs": "AR_CTX_TIMEOUT",
    "contextMaxTokens": "AR_CTX_MAX_TOKENS",
    "contextThresholdTokens": "AR_CTX_THRESHOLD_TOKENS",
    "enabled": "AR_ENABLED",
}
for k, var in keymap.items():
    v = cfg.get(k)
    if v is None or isinstance(v, (dict, list)):
        continue
    if isinstance(v, bool):
        v = "true" if v else "false"
    elif isinstance(v, float) and v.is_integer():
        v = int(v)  # 25000.0 -> 25000 so the shell integer guard accepts it
    print("%s=%s" % (var, v))
PYEOF
)
EOF
fi

# Resolve effective config: env var > settings > default.
REVIEW_MODEL="${CLAW_REVIEW_MODEL:-${CLAW_SUBAGENT_MODEL:-${AR_MODEL:-deepseek/deepseek-v4-pro}}}"
REVIEW_TIMEOUT="${CLAW_REVIEW_TIMEOUT:-${AR_TIMEOUT:-180}}"
CONTEXT_MODEL="${CLAW_REVIEW_CONTEXT_MODEL:-${AR_CTX_MODEL:-$REVIEW_MODEL}}"
CONTEXT_TIMEOUT="${CLAW_REVIEW_CONTEXT_TIMEOUT:-${AR_CTX_TIMEOUT:-60}}"
# Char budgets: explicit char env wins; else tokens-from-settings * chars/token;
# else default. The token settings are integer-guarded BEFORE the arithmetic so a
# malformed value (string, float, empty) falls back to the default instead of
# crashing `$(( ))` under `set -u` (which would abort before fail_open could fire).
if [ -n "${CLAW_REVIEW_CONTEXT_MAX:-}" ]; then
  CONTEXT_MAX="$CLAW_REVIEW_CONTEXT_MAX"
else
  case "$AR_CTX_MAX_TOKENS" in
    ''|*[!0-9]*) CONTEXT_MAX=100000 ;;
    *) CONTEXT_MAX=$(( AR_CTX_MAX_TOKENS * CHARS_PER_TOKEN )) ;;
  esac
fi
if [ -n "${CLAW_REVIEW_CONTEXT_THRESHOLD:-}" ]; then
  CONTEXT_THRESHOLD="$CLAW_REVIEW_CONTEXT_THRESHOLD"
else
  case "$AR_CTX_THRESHOLD_TOKENS" in
    ''|*[!0-9]*) CONTEXT_THRESHOLD=12000 ;;
    *) CONTEXT_THRESHOLD=$(( AR_CTX_THRESHOLD_TOKENS * CHARS_PER_TOKEN )) ;;
  esac
fi

# Settings can disable the review entirely (deliberate opt-out, not a failure).
if [ "$AR_ENABLED" = "false" ] && [ -z "${CLAW_REVIEW_FORCE:-}" ]; then
  note "disabled via adversarialReview.enabled=false in settings; skipping"
  exit 0
fi

# FAIL OPEN, but LOUDLY. A missing key / offline reviewer / timeout must never
# hard-block the session, but it must also never be mistaken for a clean review.
# We emit a banner to stderr and, when python is available, a `systemMessage` on
# stdout so the skip surfaces to the user and the model (not just buried logs).
fail_open() {
  printf '\n========================================\n' >&2
  printf '⚠  adversarial-review SKIPPED: %s\n' "$1" >&2
  printf '   No independent review ran — rely on your own judgment.\n' >&2
  printf '========================================\n' >&2
  if [ -n "${PY:-}" ]; then
    REASON="$1" "$PY" - <<'PYEOF' || true
import json, os
msg = "⚠ adversarial-review SKIPPED: %s. No independent review ran — rely on your own judgment and say so to the user." % os.environ.get("REASON", "")
print(json.dumps({"systemMessage": msg}))
PYEOF
  fi
  exit 0
}

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

# --- The adversarial rubric --------------------------------------------------
# Single canonical source: .claude/skills/adversarial-review/rubric.md. SKILL.md
# and the preset's system_prompt mirror that file; this driver reads it directly
# so the hook path can never silently drift from the canonical text.
RUBRIC_FILE="$SCRIPT_DIR/../.claude/skills/adversarial-review/rubric.md"
if [ -r "$RUBRIC_FILE" ]; then
  RUBRIC="$(cat "$RUBRIC_FILE")"
else
  fail_open "rubric file not found at $RUBRIC_FILE"
fi

# --- User intent + conversation context --------------------------------------
# The reviewer must see WHAT THE USER ASKED FOR, not just the plan/diff, or it
# cannot judge scope/plan deviations. We rebuild this from the hook payload's
# `transcript_path`: always include the user's first prompt; for long
# transcripts, summarize the whole conversation with one OpenRouter call;
# otherwise pass the (trimmed) conversation through. Best-effort — any failure
# degrades to the first prompt (or nothing) and never blocks.
TRANSCRIPT="$("$PY" - "$TMP/payload.json" <<'PYEOF'
import sys, json
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    d = {}
print(d.get("transcript_path", "") or "")
PYEOF
)"

CONTEXT=""
if [ -n "$TRANSCRIPT" ] && [ -r "$TRANSCRIPT" ]; then
  CONTEXT="$(CTX_TRANSCRIPT="$TRANSCRIPT" \
             CTX_THRESHOLD="$CONTEXT_THRESHOLD" \
             CTX_MODEL="$CONTEXT_MODEL" \
             CTX_TIMEOUT="$CONTEXT_TIMEOUT" \
             CTX_MAX="$CONTEXT_MAX" \
             "$PY" - <<'PYEOF' 2>/dev/null || true
import os, json, urllib.request

path = os.environ["CTX_TRANSCRIPT"]
threshold = int(os.environ.get("CTX_THRESHOLD", "12000"))
model = os.environ.get("CTX_MODEL", "")
timeout = float(os.environ.get("CTX_TIMEOUT", "60"))
api_key = os.environ.get("OPENROUTER_API_KEY", "")
# Hard cap on history fed to the summarizer and on the context block we emit.
max_chars = int(os.environ.get("CTX_MAX", "100000"))

def message_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for c in content:
            if isinstance(c, dict) and c.get("type") == "text":
                parts.append(c.get("text", ""))
        return "\n".join(p for p in parts if p)
    return ""

msgs = []
try:
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                continue
            m = obj.get("message") if isinstance(obj, dict) else None
            if not isinstance(m, dict):
                continue
            role = m.get("role")
            if role not in ("user", "assistant"):
                continue
            text = message_text(m.get("content")).strip()
            if text:
                msgs.append((role, text))
except Exception:
    msgs = []

first_prompt = next((t for r, t in msgs if r == "user"), "")
convo = "\n\n".join("[%s] %s" % (r, t) for r, t in msgs)

summary = ""
if len(convo) > threshold and api_key and model:
    sys_prompt = (
        "Summarize this Claude Code coding session into a brief for an "
        "independent code reviewer. Capture: the user's original goal and any "
        "explicit constraints, the key decisions and trade-offs made, what was "
        "actually built/changed (file by file where it matters), and any open "
        "concerns or deviations. Be factual with no preamble; thorough but "
        "compact (target 300-800 words; go longer only for a large, complex "
        "session)."
    )
    payload = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": sys_prompt},
            {"role": "user", "content": convo[:max_chars]},
        ],
        "temperature": 0.1,
    }).encode()
    try:
        req = urllib.request.Request(
            "https://openrouter.ai/api/v1/chat/completions",
            data=payload,
            headers={
                "Authorization": "Bearer " + api_key,
                "Content-Type": "application/json",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.load(resp)
        summary = (data["choices"][0]["message"]["content"] or "").strip()
    except Exception:
        summary = ""

blocks = []
if first_prompt:
    blocks.append("=== USER'S ORIGINAL REQUEST ===\n" + first_prompt)
if summary:
    blocks.append("=== CONVERSATION CONTEXT (summarized) ===\n" + summary)
elif convo:
    # No summary (short session, or summary call failed): send raw history up to
    # the budget; only truncate when it exceeds the cap.
    if len(convo) > max_chars:
        convo = convo[:max_chars] + "\n...[context truncated]"
    blocks.append("=== CONVERSATION CONTEXT ===\n" + convo)
out = "\n\n".join(blocks)
# Final guard: never emit a context block larger than the budget.
if len(out) > max_chars:
    out = out[:max_chars] + "\n...[context truncated]"
print(out)
PYEOF
)"
fi

if [ -n "${CONTEXT//[[:space:]]/}" ]; then
  PROMPT="${RUBRIC}

${CONTEXT}

${ARTIFACT}"
else
  note "no transcript context available; reviewing artifact without user-intent context"
  PROMPT="${RUBRIC}

${ARTIFACT}"
fi

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
