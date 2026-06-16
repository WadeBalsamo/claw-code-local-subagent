#!/usr/bin/env bash
# Integration tests for the adversarial-review hook driver
# (scripts/adversarial_review.sh): user-intent context injection, claw-code
# settings precedence, the token integer-guard, fail-open visibility, and the
# settings disable switch. Uses a stub `claw` so no network/model is needed.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DRIVER="$REPO_ROOT/scripts/adversarial_review.sh"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
test_count=0; passed=0; failed=0

log_test() { test_count=$((test_count + 1)); echo -e "\n${YELLOW}[Test $test_count]${NC} $1"; }
pass() { echo -e "${GREEN}✓ PASS${NC}: $1"; passed=$((passed + 1)); }
fail() { echo -e "${RED}✗ FAIL${NC}: $1"; failed=$((failed + 1)); }
assert() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

PY="$(command -v python3 || command -v python || true)"
if [ -z "$PY" ]; then
  echo "python3 not available; skipping adversarial-review driver tests"
  exit 0
fi

# Stage an isolated repo with the driver, rubric, a stub claw, a transcript and
# a plan-mode hook payload. Echoes the work dir. $1 = settings.json contents.
stage() {
  local work; work="$(mktemp -d)"
  mkdir -p "$work/scripts" "$work/.agents/skills/adversarial-review" "$work/.codex" "$work/bin"
  cp "$DRIVER" "$work/scripts/adversarial_review.sh"
  cp "$REPO_ROOT/.agents/skills/adversarial-review/rubric.md" \
     "$work/.agents/skills/adversarial-review/rubric.md"
  git -C "$work" init -q
  printf '%s' "$1" > "$work/.codex/settings.json"
  printf '{"type":"user","message":{"role":"user","content":"ADD_RETRY_LOGIC_MARKER"}}\n' \
    > "$work/transcript.jsonl"
  printf '{"tool_input":{"plan":"PLAN_MARKER"},"transcript_path":"%s/transcript.jsonl"}' \
    "$work" > "$work/payload.json"
  # Stub claw: emulate start/status so the driver exercises the pollable path.
  {
    printf '#!/usr/bin/env bash\n'
    printf 'if [ "$1" = "subagent" ] && [ "$2" = "start" ]; then\n'
    printf '  echo "$*" > "%s/args.txt"\n' "$work"
    printf '  cat > "%s/prompt.txt"\n' "$work"
    printf '  printf "%%s" "{\\"status\\":\\"started\\",\\"run_id\\":\\"stub-run\\",\\"status_file\\":\\"%s/status.json\\",\\"status_command\\":\\"claw subagent status --status-file %s/status.json\\",\\"stop_command\\":\\"claw subagent stop --status-file %s/status.json\\"}"\n' "$work" "$work" "$work"
    printf '  exit 0\n'
    printf 'fi\n'
    printf 'if [ "$1" = "subagent" ] && [ "$2" = "status" ]; then\n'
    printf '  echo "$*" > "%s/status_args.txt"\n' "$work"
    printf '  printf "%%s" "{\\"status\\":\\"completed\\",\\"phase\\":\\"completed\\",\\"stale\\":false,\\"worker_alive\\":false,\\"event_count\\":2,\\"summary\\":\\"VERDICT: PASS\\",\\"model\\":\\"x\\",\\"provider\\":\\"openrouter\\"}"\n'
    printf '  exit 0\n'
    printf 'fi\n'
    printf 'echo "unexpected claw invocation: $*" >&2\n'
    printf 'exit 2\n'
  } > "$work/bin/claw"
  chmod +x "$work/bin/claw"
  echo "$work"
}

run_driver() {  # $1=workdir, rest=extra env assignments
  local work="$1"; shift
  ( cat "$work/payload.json" | env "$@" PATH="$work/bin:$PATH" OPENROUTER_API_KEY=sk-bogus \
      CLAW_BIN=claw CLAW_REVIEW_CONTEXT_TIMEOUT=2 \
      bash "$work/scripts/adversarial_review.sh" --mode plan ) >"$work/out.txt" 2>"$work/err.txt"
  echo $?
}

# ---------------------------------------------------------------------------
log_test "Injects user's original request + conversation context into the prompt"
W=$(stage '{}'); run_driver "$W" >/dev/null
grep -q "USER'S ORIGINAL REQUEST" "$W/prompt.txt"; r1=$?
grep -q "ADD_RETRY_LOGIC_MARKER" "$W/prompt.txt"; r2=$?
grep -q "CONVERSATION CONTEXT" "$W/prompt.txt"; r3=$?
grep -q "PLAN_MARKER" "$W/prompt.txt"; r4=$?
[ $r1 -eq 0 ] && [ $r2 -eq 0 ] && [ $r3 -eq 0 ] && [ $r4 -eq 0 ]; assert $? "prompt carries request + context + artifact"
rm -rf "$W"

log_test "Settings drive reviewer model and timeout"
W=$(stage '{"adversarialReview":{"model":"deepseek/from-settings","timeoutSecs":99}}'); run_driver "$W" >/dev/null
grep -q -- "--model deepseek/from-settings" "$W/args.txt"; m=$?
grep -q -- "--timeout-secs 99" "$W/args.txt"; t=$?
[ $m -eq 0 ] && [ $t -eq 0 ]; assert $? "model + timeoutSecs read from settings"
rm -rf "$W"

log_test "Default reviewer path uses start/status and no timeout"
W=$(stage '{}'); run_driver "$W" >/dev/null
grep -q -- "subagent start" "$W/args.txt"; start=$?
test -f "$W/status_args.txt"; status=$?
grep -q -- "--timeout-secs" "$W/args.txt"; timeout=$?
[ $start -eq 0 ] && [ $status -eq 0 ] && [ $timeout -ne 0 ]; assert $? "pollable path used without default timeout"
rm -rf "$W"

log_test "Env var overrides settings (precedence)"
W=$(stage '{"adversarialReview":{"model":"deepseek/from-settings"}}')
run_driver "$W" CLAW_REVIEW_MODEL=env/wins >/dev/null
grep -q -- "--model env/wins" "$W/args.txt"; assert $? "CLAW_REVIEW_MODEL overrides settings model"
rm -rf "$W"

log_test "Non-integer contextMaxTokens does NOT crash the driver (integer guard)"
W=$(stage '{"adversarialReview":{"contextMaxTokens":"lots"}}'); run_driver "$W" >/dev/null
[ -f "$W/args.txt" ]; assert $? "reviewer still invoked despite malformed token setting (no silent abort)"
rm -rf "$W"

log_test "Float contextMaxTokens is coerced, not rejected"
W=$(stage '{"adversarialReview":{"contextMaxTokens":25000.0}}'); run_driver "$W" >/dev/null
[ -f "$W/args.txt" ]; assert $? "reviewer invoked with integer-valued float setting"
rm -rf "$W"

log_test "Missing OPENROUTER_API_KEY fails open LOUDLY (banner + systemMessage)"
W=$(stage '{}')
rc=$( cat "$W/payload.json" | env -u OPENROUTER_API_KEY PATH="$W/bin:$PATH" CLAW_BIN=claw \
        bash "$W/scripts/adversarial_review.sh" --mode plan >"$W/o" 2>"$W/e"; echo $? )
grep -q "SKIPPED" "$W/e"; banner=$?
grep -q "systemMessage" "$W/o"; sysmsg=$?
[ "$rc" = "0" ] && [ $banner -eq 0 ] && [ $sysmsg -eq 0 ]; assert $? "exit 0 with visible skip banner + systemMessage"
rm -rf "$W"

log_test "adversarialReview.enabled=false skips the review"
W=$(stage '{"adversarialReview":{"enabled":false}}')
rc=$( cat "$W/payload.json" | env PATH="$W/bin:$PATH" OPENROUTER_API_KEY=sk-bogus CLAW_BIN=claw \
        bash "$W/scripts/adversarial_review.sh" --mode plan >"$W/o" 2>"$W/e"; echo $? )
notcalled=1; [ -f "$W/args.txt" ] || notcalled=0
grep -q "disabled via" "$W/e"; disabled=$?
[ "$rc" = "0" ] && [ $disabled -eq 0 ] && [ $notcalled -eq 0 ]; assert $? "review skipped, reviewer not invoked"
rm -rf "$W"

# ---------------------------------------------------------------------------
echo ""
echo "============================================================================"
echo -e "Total: ${YELLOW}$test_count${NC}  Passed: ${GREEN}$passed${NC}  Failed: ${RED}$failed${NC}"
if [ "$failed" -eq 0 ]; then echo -e "${GREEN}All tests passed!${NC}"; exit 0; fi
echo -e "${RED}$failed test(s) failed.${NC}"; exit 1
