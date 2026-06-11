#!/usr/bin/env bash
# ollamacode — Ollama launcher for claw-code
# Installed by install.sh to ~/.local/bin/ollamacode
# Auto-detects whether to start a local server or connect to a remote one.
set -euo pipefail

REPO_ROOT="${CLAW_CODE_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/../.." && pwd)}"
CLI_BIN="$REPO_ROOT/rust/target/debug/claw"
if [ ! -x "$CLI_BIN" ]; then
  CLI_BIN="$REPO_ROOT/rust/target/release/claw"
fi

CONFIG_FILE="$HOME/.config/ollamacode.conf"

# -- defaults -----------------------------------------------------------------
OLLAMA_HOST="${OLLAMA_HOST:-127.0.0.1}"
OLLAMA_PORT="${OLLAMA_PORT:-11434}"
OLLAMA_KEEP_ALIVE="${OLLAMA_KEEP_ALIVE:-30m}"
OLLAMACODE_REASONING="${OLLAMACODE_REASONING:-on}"
OLLAMACODE_TEMPERATURE="${OLLAMACODE_TEMPERATURE:-0.2}"
OLLAMACODE_PERMISSION_MODE="${OLLAMACODE_PERMISSION_MODE:-workspace-write}"
OLLAMACODE_DEFAULT_MODEL="${OLLAMACODE_DEFAULT_MODEL:-}"
SERVER_PID=""

[[ -f "$CONFIG_FILE" ]] && source "$CONFIG_FILE"

OLLAMA_BASE_URL="http://${OLLAMA_HOST}:${OLLAMA_PORT}"

# -- server lifecycle ---------------------------------------------------------
check_server() {
  python3 - "$OLLAMA_BASE_URL" <<'PY' >/dev/null 2>&1
import sys, json, urllib.request
url = sys.argv[1].rstrip('/') + '/v1/models'
try:
    with urllib.request.urlopen(
        urllib.request.Request(url, headers={'Content-Type': 'application/json'}), timeout=5
    ) as r: json.load(r)
except Exception: sys.exit(1)
PY
}

get_models() {
  python3 - "$OLLAMA_BASE_URL" <<'PY'
import sys, json, urllib.request
url = sys.argv[1].rstrip('/') + '/v1/models'
with urllib.request.urlopen(
    urllib.request.Request(url, headers={'Content-Type': 'application/json'}), timeout=10
) as r:
    data = json.load(r)
for item in data.get('data', []):
    if isinstance(item, dict) and item.get('id'): print(item['id'])
PY
}

get_model_ctx() {
  local model="$1"
  python3 - "$OLLAMA_BASE_URL" "$model" <<'PY' 2>/dev/null || true
import sys, json, urllib.request
url  = sys.argv[1].rstrip('/') + '/api/show'
body = json.dumps({"name": sys.argv[2]}).encode()
req  = urllib.request.Request(url, data=body, headers={'Content-Type': 'application/json'})
try:
    with urllib.request.urlopen(req, timeout=15) as r:
        data = json.load(r)
    mi = data.get('model_info') or {}
    for k, v in mi.items():
        if k.endswith('.context_length') and isinstance(v, (int, float)):
            print(int(v)); return
    ctx = mi.get('context_length')
    if ctx: print(int(ctx))
except Exception: pass
PY
}

start_server() {
  local ctx="${1:-131072}"
  check_server && { echo "Ollama already running at $OLLAMA_BASE_URL" >&2; return 0; }
  echo "Starting Ollama (ctx=$ctx)..." >&2
  OLLAMA_HOST="${OLLAMA_HOST}:${OLLAMA_PORT}" \
  OLLAMA_CONTEXT_LENGTH="$ctx" OLLAMA_KEEP_ALIVE="$OLLAMA_KEEP_ALIVE" \
  nohup ollama serve >/tmp/ollama-server.log 2>&1 &
  SERVER_PID=$!
  for i in {1..15}; do
    check_server && { echo "Server ready." >&2; return 0; }
    sleep 1
  done
  echo "Server did not start." >&2; kill "$SERVER_PID" 2>/dev/null || true; return 1
}

stop_server() { [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""; }
cleanup() { [[ "${OLLAMACODE_SERVER_MODE:-session}" != "off" ]] && stop_server; }
trap cleanup EXIT

# -- model selection TUI ------------------------------------------------------
select_model() {
  local models
  mapfile -t models < <(get_models 2>/dev/null || true)
  if [ ${#models[@]} -eq 0 ]; then
    echo "No models found. Run: ollama pull <model>" >&2; exit 1
  fi
  declare -A CTX_CACHE
  for m in "${models[@]}"; do CTX_CACHE["$m"]="$(get_model_ctx "$m" 2>/dev/null || echo "?";)"; done

  local selected=0 num="${#models[@]}"
  _hide() { printf "\033[?25l"; }; _show() { printf "\033[?25h"; }; _home() { printf "\033[2J\033[H"; }

  _draw() {
    _home
    echo "Ollama server: $OLLAMA_BASE_URL" >&2
    echo "↑↓ enter q" >&2; echo >&2
    local i; for i in "${!models[@]}"; do
      if (( i == selected )); then printf "\033[7m  ▶ %-50s ctx: %s\033[0m\n" "${models[$i]}" "${CTX_CACHE[${models[$i]}]:-?}" >&2
      else printf "    %-50s ctx: %s\n" "${models[$i]}" "${CTX_CACHE[${models[$i]}]:-?}" >&2; fi
    done
  }

  _read_key() {
    local key rest; IFS= read -rsn1 key
    if [[ "$key" == $'\033' ]]; then IFS= read -rsn2 -t 0.1 rest || true; key+="$rest"; fi
    case "$key" in $'\033[A') echo "up" ;; $'\033[B') echo "down" ;; $'') echo "enter" ;; q|Q) echo "quit" ;; *) echo "other" ;; esac
  }

  _hide; trap '_show' RETURN
  while true; do
    _draw
    case "$(_read_key)" in
      up) (( selected > 0 )) && (( selected-- )) ;;
      down) (( selected < num - 1 )) && (( selected++ )) ;;
      enter) _show; echo "${models[$selected]}"; return 0 ;;
      quit) _show; exit 0 ;;
    esac
  done
}

# -- setup menu ---------------------------------------------------------------
setup_menu() {
  local choice
  while true; do
    echo "===== Ollamacode Settings =====" >&2
    echo " 1) Host     : $OLLAMA_HOST" >&2
    echo " 2) Port     : $OLLAMA_PORT" >&2
    echo " 3) Default  : ${OLLAMACODE_DEFAULT_MODEL:-<none>}" >&2
    echo " 4) Permission: $OLLAMACODE_PERMISSION_MODE" >&2
    echo " 5) Keep alive: $OLLAMA_KEEP_ALIVE" >&2
    echo " 6) Save & exit" >&2
    echo " 7) Exit" >&2
    read -rp "Option: " choice
    case "${choice:-}" in
      1) read -rp "Host [$OLLAMA_HOST]: " v; [[ -n "${v:-}" ]] && OLLAMA_HOST="$v" ;;
      2) read -rp "Port [$OLLAMA_PORT]: " v; [[ "${v:-}" =~ ^[0-9]+$ ]] && OLLAMA_PORT="$v" ;;
      3) read -rp "Default model tag: " v; OLLAMACODE_DEFAULT_MODEL="${v:-}" ;;
      4) read -rp "Permission mode [$OLLAMACODE_PERMISSION_MODE]: " v; [[ -n "${v:-}" ]] && OLLAMACODE_PERMISSION_MODE="$v" ;;
      5) read -rp "Keep alive [$OLLAMA_KEEP_ALIVE]: " v; [[ -n "${v:-}" ]] && OLLAMA_KEEP_ALIVE="$v" ;;
      6) cat > "$CONFIG_FILE" <<EOF
OLLAMA_HOST=${OLLAMA_HOST}
OLLAMA_PORT=${OLLAMA_PORT}
OLLAMA_KEEP_ALIVE=${OLLAMA_KEEP_ALIVE}
OLLAMACODE_REASONING=${OLLAMACODE_REASONING}
OLLAMACODE_TEMPERATURE=${OLLAMACODE_TEMPERATURE}
OLLAMACODE_PERMISSION_MODE=${OLLAMACODE_PERMISSION_MODE}
OLLAMACODE_DEFAULT_MODEL=${OLLAMACODE_DEFAULT_MODEL}
EOF
        echo "Saved to $CONFIG_FILE" >&2; return 0 ;;
      7) return 1 ;;
      *) echo "Invalid." >&2 ;;
    esac
  done
}

# -- arg parsing --------------------------------------------------------------
MODEL_ARG=""
CTX_OVERRIDE=""
DO_SETUP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --setup) DO_SETUP=1; shift ;;
    --model) MODEL_ARG="$2"; shift 2 ;;
    --ctx) CTX_OVERRIDE="$2"; shift 2 ;;
    --host) OLLAMA_HOST="$2"; OLLAMA_BASE_URL="http://${2}:${OLLAMA_PORT}"; shift 2 ;;
    --port) OLLAMA_PORT="$2"; OLLAMA_BASE_URL="http://${OLLAMA_HOST}:${2}"; shift 2 ;;
    --help|-h)
      echo "Usage: ollamacode [--model TAG] [--ctx N] [--host URL] [--port PORT] [--setup]"
      exit 0 ;;
    *) echo "Unknown: $1" >&2; exit 1 ;;
  esac
done

if [ ! -x "$CLI_BIN" ]; then
  echo "Error: claw binary not found. Build with: cd $REPO_ROOT/rust && cargo build --workspace" >&2
  exit 1
fi

(( DO_SETUP )) && { setup_menu; exit 0; }

# -- ensure server is running -------------------------------------------------
if ! check_server; then
  if command -v ollama >/dev/null 2>&1; then
    OLLAMACODE_SERVER_MODE="session"
    start_server "131072" || exit 1
  else
    echo "Ollama not reachable at $OLLAMA_BASE_URL and ollama CLI not found." >&2
    exit 1
  fi
fi

# -- model selection ----------------------------------------------------------
if [[ -z "${MODEL_ARG:-}" ]]; then
  if [[ -n "${OLLAMACODE_DEFAULT_MODEL:-}" ]]; then
    read -rp "Use default model [${OLLAMACODE_DEFAULT_MODEL}]? (Y/n/setup): " ans
    case "${ans:-Y}" in
      [Nn]*) MODEL_ARG="$(select_model)" ;;
      setup|SETUP) setup_menu || true; MODEL_ARG="$(select_model)" ;;
      *) MODEL_ARG="$OLLAMACODE_DEFAULT_MODEL" ;;
    esac
  else
    MODEL_ARG="$(select_model)"
  fi
fi

# -- context ------------------------------------------------------------------
MODEL_MAX_CTX="$(get_model_ctx "$MODEL_ARG" 2>/dev/null || true)"
DEFAULT_CTX="${CTX_OVERRIDE:-${MODEL_MAX_CTX:-131072}}"
echo "" >&2
[[ -n "${MODEL_MAX_CTX:-}" ]] && echo "  Max context: ${MODEL_MAX_CTX}" >&2
read -rp "  Context window [${DEFAULT_CTX}]: " ctx_in
NUM_CTX="${ctx_in:-$DEFAULT_CTX}"

# -- launch -------------------------------------------------------------------
export OPENAI_BASE_URL="${OLLAMA_BASE_URL}/v1"
export OPENAI_API_KEY="${OPENAI_API_KEY:-ollama}"
export CLAW_RESILIENCE=force

echo "Launching claw with $MODEL_ARG (ctx=$NUM_CTX)" >&2
exec "$CLI_BIN" --model "$MODEL_ARG" --permission-mode "$OLLAMACODE_PERMISSION_MODE"
