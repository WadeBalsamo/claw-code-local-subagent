#!/usr/bin/env bash
# run-claw-code — Agent-facing entry point for claw-code
# Installed by install.sh to ~/.local/bin/run-claw-code
#
# Spawns a claw session in a temporary git worktree, waits for completion,
# captures diff/summary, and returns structured output for parent agents.
#
# Output contract (4 lines on stdout):
#   task_id=<uuid>
#   status_file=/tmp/claw-runs/<uuid>/status.json
#   diff_file=/tmp/claw-runs/<uuid>/diff.patch
#   summary_file=/tmp/claw-runs/<uuid>/summary.md
#
# Preset JSON schema (every field is optional):
#   {
#     "preset_name": "qwen3-coder-30b",
#     "tier_ref": "presets.<name>",         # optional: orchestrator key into its config/model_tiers.json
#     "provider": "lmstudio|openrouter|ollama|anthropic|openai|auto",
#     "model": "qwen3-coder-30b-a3b-q5",    # local-first default; the orchestrator can override
#                                           # it (budget-gated) via --provider/--model
#     "lmstudio_model_id": "qwen3-coder-30b-a3b-q5",
#     "resource": "gpu|cpu|gpu+cpu|remote", # broker slot (RESOURCE_BROKER V3 vocabulary)
#     "env": { "OPENAI_BASE_URL": "...", "OPENAI_API_KEY": "...", ... },
#     "system_prompt": "...",
#     "plan_mode": "normal|ultraplan|plan-only",
#     "temperature": 0.2,
#     "max_context": 262144,
#     "permission_mode": "danger-full-access|read-only|default",
#     "allowed_tools": ["read_file", "edit_file", ...],
#     "budget_gate": true,
#     "open_pr": false,
#     "requires_feature_flag": "CONSULTANT_ENABLED",  # gated presets only
#     "max_cost_usd": 0.50,                            # remote presets only
#     "cfo_budget_endpoint": "http://localhost:9000/mcp/budget/consultant",
#     "completion_webhook": "http://localhost:8765/claw-run-complete",
#     "timeout_seconds": 1800
#   }
#
# Local-first / OpenRouter-fallback: a preset names ONE concrete provider/model — its
# local-first default. The hybrid ladder (local rung, then budget-gated OpenRouter rungs)
# lives in config/model_tiers.json under `tier_ref`; the orchestrator (mcp.py) resolves the
# rung, budget-gates the OpenRouter path, and re-dispatches with `--provider/--model` to
# override the preset default. This launcher never falls back to OpenRouter on its own — that
# would bypass the budget gate. See meta/MODEL_ROUTING.md + meta/clawcode-specs.md.
#
# Presets are resolved from (in order):
#   1. ~/.lmcode/presets/<agent>.json
#   2. <REPO_ROOT>/scripts/presets/<agent>.json
#
# If no preset is found, the agent runs with defaults (model from env / --model).

set -euo pipefail

REPO_ROOT="${CLAW_CODE_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/../.." && pwd)}"
CLI_BIN="$REPO_ROOT/rust/target/debug/claw"
if [ ! -x "$CLI_BIN" ]; then
  CLI_BIN="$REPO_ROOT/rust/target/release/claw"
fi

RUN_ROOT="/tmp/claw-runs"
LOCK_DIR="$RUN_ROOT/_locks"
mkdir -p "$RUN_ROOT" "$LOCK_DIR"

# shellcheck disable=SC3045
MAX_WAIT_LOCK=900  # 15 min timeout for resource locks

# ---------------------------------------------------------------------------
# Arg parsing
# ---------------------------------------------------------------------------
AGENT=""
WORK_DIR=""
PLAN=""
TASK_ID=""
REMOTE=0
RESOURCE=""
TIMEOUT=1800
MAX_PARALLEL=1
PR_INTO=""
SPRINT_ID=""
BRANCH_OVERRIDE=""
PROVIDER_OVERRIDE=""
MODEL_OVERRIDE=""

list_visible_presets() {
  # Enumerate presets in both search paths, hiding any whose
  # requires_feature_flag env var is unset/0/false.
  python3 - <<PY
import json, os, glob
seen = set()
paths = [os.path.expanduser("~/.lmcode/presets"), "${REPO_ROOT}/scripts/presets"]
for d in paths:
    for f in sorted(glob.glob(os.path.join(d, "*.json"))):
        name = os.path.splitext(os.path.basename(f))[0]
        if name in seen:
            continue
        try:
            p = json.load(open(f))
        except Exception:
            continue
        flag = p.get("requires_feature_flag")
        if flag:
            v = os.environ.get(flag, "")
            if v.lower() not in ("1", "true", "yes", "on"):
                continue
        seen.add(name)
        desc = p.get("description", "")
        rt = p.get("resource") or ""
        print(f"  {name:<32} [{rt:<14}] {desc[:80]}")
PY
}

show_help() {
  cat <<EOF
Usage: run-claw-code --agent <preset> --dir <path> --plan <prompt>
       [--provider <p>] [--model <m>] [--id <uuid>] [--remote]
       [--resource <name>] [--timeout <sec>] [--max-parallel <N>]
       [--pr-into <base_branch>] [--sprint-id <id>] [--branch <name>] [--help]

  --provider / --model override the preset's local-first default. The orchestrator uses them
  to run a preset's task on a budget-gated remote rung (e.g. the qwen3-coder-30b preset's task
  on deepseek-v4-flash) without editing the preset.

Output (4 lines):
  task_id=<uuid>
  /tmp/claw-runs/<uuid>/status.json
  /tmp/claw-runs/<uuid>/diff.patch
  /tmp/claw-runs/<uuid>/summary.md

When --pr-into is set, a 5th file is written:
  /tmp/claw-runs/<uuid>/pr_request.json   (consumed by the EM's MCP layer)

Available presets (gated presets hidden when their feature flag is off):
EOF
  list_visible_presets
  cat <<EOF

Preset JSON schema fields:
  preset_name           - canonical name (defaults to the filename)
  tier_ref              - orchestrator key into config/model_tiers.json (hybrid ladder)
  provider              - lmstudio|openrouter|ollama|anthropic|openai|auto (local-first default)
  model                 - local-first model id (overridable with --model)
  lmstudio_model_id     - exact ID for the broker's LMStudio swap
  resource              - gpu|cpu|gpu+cpu|remote (the broker slot the local model holds)
  env                   - object of env var name:value pairs to set
  plan_mode             - normal|ultraplan|plan-only
  system_prompt         - prepended to the plan text
  temperature           - model temperature (0.0-1.0)
  max_context           - context window limit
  permission_mode       - danger-full-access|read-only|default
  allowed_tools         - advisory list of tool names (metadata; not enforced by this launcher)
  budget_gate           - true: every OpenRouter dispatch is budget-gated upstream (no bypass)
  open_pr               - true: worker may open a PR (security boundary; default false)
  requires_feature_flag - env var that must be truthy to expose this preset
  max_cost_usd          - per-invocation cost cap (remote presets)
  cfo_budget_endpoint   - URL for live budget MCP check
  completion_webhook    - URL to POST on task exit (wake-on-completion)
  timeout_seconds       - default wall-clock timeout (overridable with --timeout)
EOF
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    --agent) AGENT="$2"; shift 2 ;;
    --dir) WORK_DIR="$2"; shift 2 ;;
    --plan) PLAN="$2"; shift 2 ;;
    --id) TASK_ID="$2"; shift 2 ;;
    --provider) PROVIDER_OVERRIDE="$2"; shift 2 ;;
    --model) MODEL_OVERRIDE="$2"; shift 2 ;;
    --remote) REMOTE=1; shift ;;
    --resource) RESOURCE="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --max-parallel) MAX_PARALLEL="$2"; shift 2 ;;
    --pr-into) PR_INTO="$2"; shift 2 ;;
    --sprint-id) SPRINT_ID="$2"; shift 2 ;;
    --branch) BRANCH_OVERRIDE="$2"; shift 2 ;;
    --help|-h) show_help ;;
    *) echo "Unknown: $1" >&2; echo "Usage: run-claw-code --agent <preset> --dir <path> --plan <prompt>" >&2; exit 1 ;;
  esac
done

: "${AGENT:?Missing --agent}"
: "${WORK_DIR:?Missing --dir}"
: "${PLAN:?Missing --plan}"

if [ ! -d "$WORK_DIR" ]; then echo "Error: $WORK_DIR is not a directory" >&2; exit 1; fi
if [ ! -x "$CLI_BIN" ]; then echo "Error: claw not found at $CLI_BIN" >&2; exit 1; fi

# ---------------------------------------------------------------------------
# Resolve preset JSON
# ---------------------------------------------------------------------------
PRESET_FILE=""
for p in "$HOME/.lmcode/presets/${AGENT}.json" "$REPO_ROOT/scripts/presets/${AGENT}.json"; do
  if [ -f "$p" ]; then PRESET_FILE="$p"; break; fi
done

# Parse preset with python3
PRESET_JSON="{}"
if [ -n "$PRESET_FILE" ]; then
  PRESET_JSON=$(python3 -c "
import json
with open('$PRESET_FILE') as f:
    print(json.dumps(json.load(f)))
" 2>/dev/null || echo "{}")
fi

# ---------------------------------------------------------------------------
# Resolve provider env vars from preset
# ---------------------------------------------------------------------------
PROVIDER=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('provider','auto'))" 2>/dev/null || echo "auto")
MODEL=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('model','') or '')" 2>/dev/null || echo "")
PLAN_MODE=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('plan_mode','normal'))" 2>/dev/null || echo "normal")
SYSTEM_PROMPT=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('system_prompt','') or '')" 2>/dev/null || echo "")
PERMISSION_MODE=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('permission_mode','danger-full-access'))" 2>/dev/null || echo "danger-full-access")
ALLOWED_TOOLS=$(python3 -c "
import json; d=json.loads('$PRESET_JSON');
t = d.get('allowed_tools');
if t:
    print(','.join(t))
else:
    print('')
" 2>/dev/null || echo "")

# Caller-supplied provider/model win over the preset's local-first defaults. This is how the
# orchestrator (mcp.py) re-dispatches a budget-gated OpenRouter rung onto a preset whose
# default is the local model — keeping the budget gate upstream, never in this launcher.
if [ -n "$PROVIDER_OVERRIDE" ]; then PROVIDER="$PROVIDER_OVERRIDE"; fi
if [ -n "$MODEL_OVERRIDE" ]; then MODEL="$MODEL_OVERRIDE"; fi

# Apply env vars from preset (user can override)
python3 -c "
import json, os
d = json.loads('$PRESET_JSON')
for k, v in d.get('env', {}).items():
    os.environ.setdefault(k, v)
print('env applied')
" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Extended preset fields (workflow wiring)
# ---------------------------------------------------------------------------
RESOURCE_TYPE=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('resource') or '')" 2>/dev/null || echo "")
LMSTUDIO_MODEL_ID=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('lmstudio_model_id','') or '')" 2>/dev/null || echo "")
REQUIRES_FLAG=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('requires_feature_flag','') or '')" 2>/dev/null || echo "")
MAX_COST_USD=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('max_cost_usd','') or '')" 2>/dev/null || echo "")
CFO_BUDGET_ENDPOINT=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('cfo_budget_endpoint','') or '')" 2>/dev/null || echo "")
COMPLETION_WEBHOOK=$(python3 -c "import json; d=json.loads('$PRESET_JSON'); print(d.get('completion_webhook','') or '')" 2>/dev/null || echo "")

# If the preset declares a slot (gpu|cpu|gpu+cpu|remote) and the caller didn't pass
# --resource, use it as the local flock-lock name: the RTX 3090 is the one contended mutex, so
# a gpu+cpu bundle serializes on "gpu" and "remote" needs no lock. Cross-agent arbitration is
# the orchestrator's job; this flock is only a local fallback for direct CLI use.
if [ -z "$RESOURCE" ] && [ -n "$RESOURCE_TYPE" ]; then
  case "$RESOURCE_TYPE" in
    remote)  RESOURCE="" ;;
    gpu+cpu) RESOURCE="gpu" ;;
    *)       RESOURCE="$RESOURCE_TYPE" ;;   # gpu, cpu
  esac
fi

# Feature-flag gate: refuse to dispatch if required env var is not truthy.
if [ -n "$REQUIRES_FLAG" ]; then
  FLAG_VAL="$(printenv "$REQUIRES_FLAG" 2>/dev/null || echo "")"
  case "$(echo "$FLAG_VAL" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes|on) ;;
    *)
      echo "Preset '$AGENT' requires feature flag $REQUIRES_FLAG=1 (currently '${FLAG_VAL}')" >&2
      mkdir -p "$RUN_ROOT/${TASK_ID:-_pre}" 2>/dev/null || true
      exit 78  # EX_CONFIG
      ;;
  esac
fi

# CFO budget pre-flight gate (remote presets only).
# Posts {preset, max_cost_usd, est_cost_usd:null, agent, sprint_id} to the
# CFO MCP endpoint; expects {"allow": bool, "reason": "..."}.
if [ -n "$CFO_BUDGET_ENDPOINT" ] && [ -n "$MAX_COST_USD" ]; then
  BUDGET_RESPONSE=$(python3 - "$CFO_BUDGET_ENDPOINT" "$AGENT" "$MAX_COST_USD" "${SPRINT_ID:-}" <<'PY' 2>/dev/null || echo '{"allow": true, "reason": "endpoint-unreachable-fail-open"}'
import json, sys, urllib.request
url, agent, max_cost, sprint_id = sys.argv[1:]
body = json.dumps({"preset": agent, "max_cost_usd": float(max_cost),
                   "sprint_id": sprint_id or None}).encode()
req = urllib.request.Request(url, data=body, method="POST",
                             headers={"Content-Type": "application/json"})
try:
    with urllib.request.urlopen(req, timeout=5) as r:
        print(r.read().decode())
except Exception as e:
    print(json.dumps({"allow": False, "reason": f"endpoint-error: {e.__class__.__name__}"}))
PY
)
  ALLOW=$(echo "$BUDGET_RESPONSE" | python3 -c "import json,sys; print(json.load(sys.stdin).get('allow', False))" 2>/dev/null || echo "False")
  if [ "$ALLOW" != "True" ]; then
    REASON=$(echo "$BUDGET_RESPONSE" | python3 -c "import json,sys; print(json.load(sys.stdin).get('reason', 'denied'))" 2>/dev/null || echo "denied")
    echo "CFO budget gate denied preset '$AGENT': $REASON" >&2
    exit 77  # EX_NOPERM
  fi
fi

# Set resilience based on provider
case "$PROVIDER" in
  openrouter|anthropic|openai)
    export CLAW_RESILIENCE="${CLAW_RESILIENCE:-none}"
    ;;
  lmstudio|ollama)
    export CLAW_RESILIENCE="${CLAW_RESILIENCE:-force}"
    ;;
  *)
    # auto — don't override, let claw auto-detect
    ;;
esac

echo "Agent:      $AGENT" >&2
echo "Provider:   $PROVIDER" >&2
echo "Model:      $MODEL" >&2
echo "Plan mode:  $PLAN_MODE" >&2
echo "Timeout:    ${TIMEOUT}s" >&2
echo "" >&2

# ---------------------------------------------------------------------------
# Task ID and status directory
# ---------------------------------------------------------------------------
TASK_ID="${TASK_ID:-$(uuidgen 2>/dev/null || python3 -c 'import uuid; print(uuid.uuid4())')}"
TASK_DIR="$RUN_ROOT/$TASK_ID"
mkdir -p "$TASK_DIR"

write_status() {
  local status="$1"; shift
  python3 -c "
import json, sys
d = {}
for a in sys.argv[1:]:
    k,v = a.split('=',1)
    d[k]=v
with open('$TASK_DIR/status.json','w') as f:
    json.dump(d,f)
" "$@"
}

write_status "running" "task_id=$TASK_ID" "status=running" \
  "started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  "agent=$AGENT" "provider=$PROVIDER" "dir=$WORK_DIR" "pid=$$"

# ---------------------------------------------------------------------------
# Resource lock (atomic: flock + slot check within same locked region)
# ---------------------------------------------------------------------------
if [ -n "$RESOURCE" ]; then
  LOCK_FILE="$LOCK_DIR/${RESOURCE}.lock"
  STATE_FILE="$LOCK_DIR/${RESOURCE}_state.json"
  echo "Waiting for resource '$RESOURCE' (max ${MAX_PARALLEL} parallel)..." >&2

  ACQUIRED=0
  exec 200>"$LOCK_FILE"
  for i in $(seq 1 "$MAX_WAIT_LOCK"); do
    # Block until we get the exclusive lock, then check slots atomically
    if ! flock 200 2>/dev/null; then
      sleep 1
      continue
    fi
    # We hold the lock now — read and update state atomically
    SLOTS_USED=$(python3 -c "
import json
try:
    d = json.load(open('$STATE_FILE'))
    print(d.get('slots_used', 0))
except:
    print(0)
" 2>/dev/null || echo 0)

    if [ "$SLOTS_USED" -lt "$MAX_PARALLEL" ]; then
      TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)
      python3 -c "
import json
f = '$STATE_FILE'
try:
    d = json.load(open(f))
except:
    d = {'slots_used': 0, 'max_parallel': $MAX_PARALLEL, 'queue': []}
d['slots_used'] = d.get('slots_used', 0) + 1
d['last_acquired'] = '$TIMESTAMP'
json.dump(d, open(f, 'w'))
" 2>/dev/null || true
      ACQUIRED=1
      break
    fi
    # Release lock and retry
    flock -u 200
    sleep 2
  done

  if [ "$ACQUIRED" -eq 0 ]; then
    echo "Timeout waiting for resource '$RESOURCE'" >&2
    write_status "failed" "task_id=$TASK_ID" "status=failed" "error=resource_timeout"
    echo "task_id=$TASK_ID"
    echo "$TASK_DIR/status.json"
    echo "$TASK_DIR/diff.patch"
    echo "$TASK_DIR/summary.md"
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Create worktree
# ---------------------------------------------------------------------------
WORKTREE_BRANCH="claw-run/$TASK_ID"
WORKTREE_DIR="$TASK_DIR/worktree"
cleanup_worktree() {
  if [ -d "$WORKTREE_DIR" ] 2>/dev/null; then
    rm -rf "$WORKTREE_DIR" 2>/dev/null || true
  fi
  git -C "$WORK_DIR" branch -D "$WORKTREE_BRANCH" 2>/dev/null || true
}
trap cleanup_worktree EXIT

git -C "$WORK_DIR" stash push -m "claw-run-stash-$TASK_ID" 2>/dev/null || true
git -C "$WORK_DIR" worktree add --detach "$WORKTREE_DIR" HEAD 2>/dev/null || {
  # fallback: use a checkout in TASK_DIR
  mkdir -p "$WORKTREE_DIR"
  git -C "$WORK_DIR" checkout-index --all --prefix="$WORKTREE_DIR/" 2>/dev/null || true
}
cd "$WORKTREE_DIR" || cd "$WORK_DIR"

# ---------------------------------------------------------------------------
# Build the claw command
# ---------------------------------------------------------------------------
SESSION_LOG="$TASK_DIR/session.log"
touch "$SESSION_LOG"

CMD=("$CLI_BIN" "--output-format" "json" "--compact")

case "$PERMISSION_MODE" in
  danger-full-access) CMD+=("--dangerously-skip-permissions") ;;
  read-only) CMD+=("--permission-mode" "read-only") ;;
  *) ;;
esac

# Provider/model flags
if [ -n "$MODEL" ]; then
  CMD+=("--model" "$MODEL")
fi

case "$PROVIDER" in
  openrouter)
    export OPENAI_BASE_URL="${OPENAI_BASE_URL:-https://openrouter.ai/api/v1/chat/completions}"
    if [ -z "${OPENAI_API_KEY:-}" ]; then
      for _env_file in "$HOME/.config/openroutercode/.env" "$HOME/.config/opencode/.env"; do
        if [ -f "$_env_file" ]; then
          # shellcheck disable=SC1090
          source "$_env_file" 2>/dev/null || true
          break
        fi
      done
      unset _env_file
    fi
    ;;
  lmstudio)
    export OPENAI_BASE_URL="${OPENAI_BASE_URL:-http://localhost:1234/v1}"
    export OPENAI_API_KEY="${OPENAI_API_KEY:-lmstudio}"
    ;;
  ollama)
    export OPENAI_BASE_URL="${OPENAI_BASE_URL:-http://localhost:11434/v1}"
    export OPENAI_API_KEY="${OPENAI_API_KEY:-ollama}"
    ;;
  anthropic)
    # Uses ANTHROPIC_API_KEY — no OPENAI vars needed
    ;;
  *)
    # auto — let claw detect from model name
    ;;
esac

# System prompt (prepended to plan text)
FULL_PLAN="$PLAN"
if [ -n "$SYSTEM_PROMPT" ]; then
  FULL_PLAN="$SYSTEM_PROMPT

---
$PLAN"
fi

if [ "$REMOTE" -eq 1 ]; then
  CMD+=("--remote")
fi

# For ultraplan mode, prepend the command
if [ "$PLAN_MODE" = "ultraplan" ]; then
  FULL_PLAN="$FULL_PLAN

(Use /ultraplan to break this into a structured execution plan before coding.)"
fi

CMD+=("$FULL_PLAN")

echo "Running: ${CMD[*]}" >&2
"${CMD[@]}" > "$SESSION_LOG" 2>&1 &
CLAW_PID=$!
echo "$CLAW_PID" > "$TASK_DIR/claw.pid"

# ---------------------------------------------------------------------------
# Wait for completion with timeout
# ---------------------------------------------------------------------------
ELAPSED=0
while kill -0 "$CLAW_PID" 2>/dev/null; do
  if [ "$ELAPSED" -ge "$TIMEOUT" ]; then
    echo "Timeout after ${TIMEOUT}s — terminating" >&2
    kill "$CLAW_PID" 2>/dev/null || true
    sleep 2
    kill -9 "$CLAW_PID" 2>/dev/null || true
    write_status "timeout" "task_id=$TASK_ID" "status=timeout" "error=timeout_after_${TIMEOUT}s"
    # Output contract even on timeout
    echo "task_id=$TASK_ID"
    echo "$TASK_DIR/status.json"
    echo "$TASK_DIR/diff.patch"
    echo "$TASK_DIR/summary.md"
    exit 124
  fi
  sleep 5
  ELAPSED=$((ELAPSED + 5))
done

wait "$CLAW_PID" 2>/dev/null || true
EXIT_CODE=$?

# ---------------------------------------------------------------------------
# Capture diff from the worktree
# ---------------------------------------------------------------------------
DIFF_FILE="$TASK_DIR/diff.patch"
GIT_DIR_PARENT="$WORK_DIR"
if [ -d "$WORKTREE_DIR/.git" ] 2>/dev/null; then
  GIT_DIR_PARENT="$WORKTREE_DIR"
fi

git -C "$GIT_DIR_PARENT" diff --no-pager --stat HEAD > "$DIFF_FILE" 2>/dev/null || true
git -C "$GIT_DIR_PARENT" diff --no-pager HEAD >> "$DIFF_FILE" 2>/dev/null || true

FILES_CHANGED=$(git -C "$GIT_DIR_PARENT" diff --stat HEAD 2>/dev/null | tail -1 | awk '{print $1}' || echo 0)
LINES_ADDED=$(git -C "$GIT_DIR_PARENT" diff --stat HEAD 2>/dev/null | tail -1 | awk '{print $4}' || echo 0)
LINES_REMOVED=$(git -C "$GIT_DIR_PARENT" diff --stat HEAD 2>/dev/null | tail -1 | awk '{print $6}' || echo 0)

# ---------------------------------------------------------------------------
# Extract summary from session log
# ---------------------------------------------------------------------------
SUMMARY_FILE="$TASK_DIR/summary.md"
python3 - "$SESSION_LOG" "$SUMMARY_FILE" <<'PY' 2>/dev/null || echo "Summary not available — see session.log" > "$SUMMARY_FILE"
import json, sys

log_path = sys.argv[1]
out_path = sys.argv[2]
buffer = ""

with open(log_path) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            buffer += line + "\n"
            continue
        if isinstance(obj, dict):
            for key in ('text', 'content', 'response', 'message', 'output'):
                val = obj.get(key)
                if val and isinstance(val, str):
                    buffer += val + "\n"
                    break

# Truncate to ~200 tokens
words = buffer.split()
summary = " ".join(words[:300])
with open(out_path, 'w') as f:
    f.write(summary + "\n")
PY

# ---------------------------------------------------------------------------
# Final status
# ---------------------------------------------------------------------------
if [ "$EXIT_CODE" -eq 0 ]; then
  write_status "done" \
    "task_id=$TASK_ID" "status=done" "exit_code=0" \
    "files_changed=$FILES_CHANGED" "lines_added=$LINES_ADDED" "lines_removed=$LINES_REMOVED" \
    "completed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
else
  write_status "failed" \
    "task_id=$TASK_ID" "status=failed" "exit_code=$EXIT_CODE" \
    "completed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fi

# ---------------------------------------------------------------------------
# Per-task PR request artifact (EM opens the PR — workers have no GH creds)
# ---------------------------------------------------------------------------
PR_REQUEST_FILE=""
if [ -n "$PR_INTO" ] && [ "$EXIT_CODE" -eq 0 ] && [ "${FILES_CHANGED:-0}" != "0" ]; then
  WORKER_BRANCH="${BRANCH_OVERRIDE:-claw-run/$TASK_ID}"
  TITLE_LINE=$(head -1 "$SUMMARY_FILE" 2>/dev/null | head -c 100)
  TITLE_LINE="${TITLE_LINE:-${AGENT}: task $TASK_ID}"
  PR_REQUEST_FILE="$TASK_DIR/pr_request.json"

  # Commit + push the worktree branch (best-effort; non-fatal on push failure
  # so the EM can retry / push manually).
  if [ -d "$GIT_DIR_PARENT/.git" ] || git -C "$GIT_DIR_PARENT" rev-parse --git-dir >/dev/null 2>&1; then
    BASE_COMMIT=$(git -C "$GIT_DIR_PARENT" rev-parse HEAD 2>/dev/null || echo "")
    git -C "$GIT_DIR_PARENT" checkout -B "$WORKER_BRANCH" >/dev/null 2>&1 || true
    git -C "$GIT_DIR_PARENT" add -A >/dev/null 2>&1 || true
    git -C "$GIT_DIR_PARENT" -c user.email="worker@claw-code.local" \
        -c user.name="claw-code worker" \
        commit -m "${AGENT}: ${TITLE_LINE}

Task: ${TASK_ID}
Sprint: ${SPRINT_ID:-unassigned}
Preset: ${AGENT}" >/dev/null 2>&1 || true
    PUSH_OK=1
    git -C "$GIT_DIR_PARENT" push -u origin "$WORKER_BRANCH" >/dev/null 2>&1 || PUSH_OK=0
  fi

  python3 - "$PR_REQUEST_FILE" "$TASK_ID" "${SPRINT_ID:-}" "$WORKER_BRANCH" "$PR_INTO" "$TITLE_LINE" "$SUMMARY_FILE" "$AGENT" "$FILES_CHANGED" "$LINES_ADDED" "$LINES_REMOVED" <<'PY'
import json, sys
path, task_id, sprint_id, head, base, title, body_path, preset, files, added, removed = sys.argv[1:]
out = {
    "task_id": task_id,
    "sprint_id": sprint_id or None,
    "head": head,
    "base": base,
    "title": title.strip() or f"{preset}: task {task_id}",
    "body_path": body_path,
    "preset": preset,
    "diff_stats": {
        "files": int(files or 0),
        "added": int(added or 0),
        "removed": int(removed or 0),
    },
}
json.dump(out, open(path, "w"), indent=2)
PY

  # Append to sprint manifest (atomic via flock)
  if [ -n "$SPRINT_ID" ]; then
    SPRINT_MANIFEST="$RUN_ROOT/_sprints/${SPRINT_ID}.json"
    mkdir -p "$(dirname "$SPRINT_MANIFEST")"
    (
      flock 201
      python3 - "$SPRINT_MANIFEST" "$TASK_ID" "$PR_INTO" "$AGENT" <<'PY'
import json, os, sys
path, task_id, base, preset = sys.argv[1:]
if os.path.exists(path):
    m = json.load(open(path))
else:
    m = {"sprint_id": os.path.basename(path).removesuffix(".json"),
         "sprint_branch": base, "tasks": []}
m.setdefault("tasks", []).append({
    "task_id": task_id, "preset": preset, "status": "pr_requested",
})
json.dump(m, open(path, "w"), indent=2)
PY
    ) 201>"$RUN_ROOT/_sprints/${SPRINT_ID}.lock"
  fi
fi

# ---------------------------------------------------------------------------
# Completion webhook (wake-on-completion for the EM)
# ---------------------------------------------------------------------------
if [ -n "$COMPLETION_WEBHOOK" ]; then
  python3 - "$COMPLETION_WEBHOOK" "$TASK_ID" "$AGENT" "${SPRINT_ID:-}" \
    "$EXIT_CODE" "$TASK_DIR/status.json" "$DIFF_FILE" "$SUMMARY_FILE" \
    "${PR_REQUEST_FILE:-}" <<'PY' >/dev/null 2>&1 || true
import json, sys, urllib.request
url, task_id, agent, sprint_id, exit_code, status, diff, summary, pr_req = sys.argv[1:]
payload = json.dumps({
    "task_id": task_id, "agent": agent, "sprint_id": sprint_id or None,
    "exit_code": int(exit_code), "status_file": status,
    "diff_file": diff, "summary_file": summary,
    "pr_request_file": pr_req or None,
}).encode()
req = urllib.request.Request(url, data=payload, method="POST",
                             headers={"Content-Type": "application/json"})
try:
    urllib.request.urlopen(req, timeout=5)
except Exception:
    pass
PY
fi

# ---------------------------------------------------------------------------
# Release resource lock
# ---------------------------------------------------------------------------
if [ -n "$RESOURCE" ] && [ "$ACQUIRED" = "1" ]; then
  # We still hold the flock from acquisition — decrement atomically
  python3 -c "
import json
f = '$STATE_FILE'
try:
    d = json.load(open(f))
    d['slots_used'] = max(0, d.get('slots_used', 1) - 1)
    json.dump(d, open(f, 'w'))
except:
    pass
" 2>/dev/null || true
  # Release the flock
  flock -u 200 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Output contract (4 lines, +1 optional pr_request line)
# ---------------------------------------------------------------------------
echo "task_id=$TASK_ID"
echo "$TASK_DIR/status.json"
echo "$DIFF_FILE"
echo "$SUMMARY_FILE"
if [ -n "${PR_REQUEST_FILE:-}" ] && [ -f "$PR_REQUEST_FILE" ]; then
  echo "pr_request=$PR_REQUEST_FILE"
fi
