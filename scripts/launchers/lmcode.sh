#!/usr/bin/env bash
# lmcode — LM Studio launcher for claw-code
# Installed by install.sh to ~/.local/bin/lmcode
# Wraps the claw binary with LM Studio auto-discovery and model selection.
set -euo pipefail

REPO_ROOT="${CLAW_CODE_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/../.." && pwd)}"
CLI_BIN="$REPO_ROOT/rust/target/debug/claw"
if [ ! -x "$CLI_BIN" ]; then
  CLI_BIN="$REPO_ROOT/rust/target/release/claw"
fi

LM_STUDIO_HOST="${LM_STUDIO_HOST:-127.0.0.1}"
LM_STUDIO_PORT="${LM_STUDIO_PORT:-1234}"
RECENT_FILE="${LM_STUDIO_RECENT_FILE:-$HOME/.lmstudio_recent_ips}"

export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-local-model}"

# -- helpers -----------------------------------------------------------------
fetch_models() {
  python3 - "$1" <<'PY' 2>/dev/null
import sys, json, urllib.request
proxy_handler = urllib.request.ProxyHandler({})
opener = urllib.request.build_opener(proxy_handler)
url = sys.argv[1].rstrip('/') + '/v1/models'
req = urllib.request.Request(url, headers={'Content-Type': 'application/json'})
try:
    with opener.open(req, timeout=10) as resp:
        data = json.load(resp)
except Exception:
    sys.exit(1)
for item in data.get('data', []):
    if isinstance(item, dict) and item.get('id'):
        print(item['id'])
PY
}

test_host_port() {
  local input="$1" host port
  IFS=':' read -r host port <<< "$input"
  port="${port:-1234}"
  fetch_models "http://${host}:${port}" >/dev/null 2>&1
}

normalize_address() {
  local input="$1" host port
  input="${input#http://}"
  input="${input#https://}"
  input="${input%%/*}"
  IFS=':' read -r host port <<< "$input"
  port="${port:-1234}"
  echo "${host}:${port}"
}

add_to_recent() {
  local addr="$1"
  if [ ! -f "$RECENT_FILE" ] || ! grep -qxF "$addr" "$RECENT_FILE" 2>/dev/null; then
    echo "$addr" >> "$RECENT_FILE"
  fi
}

auto_probe() {
  if [ -f "$RECENT_FILE" ] && [ -s "$RECENT_FILE" ]; then
    while IFS= read -r addr; do
      [ -z "$addr" ] && continue
      if test_host_port "$addr"; then echo "$addr"; return 0; fi
    done < <(tac "$RECENT_FILE" | awk '!seen[$0]++')
  fi
  for candidate in "127.0.0.1:${LM_STUDIO_PORT}" "localhost:${LM_STUDIO_PORT}"; do
    if test_host_port "$candidate"; then echo "$candidate"; return 0; fi
  done
  return 1
}

interactive_select() {
  local options=()
  if [ -f "$RECENT_FILE" ] && [ -s "$RECENT_FILE" ]; then
    mapfile -t options < <(tac "$RECENT_FILE" | awk '!seen[$0]++')
  fi
  while true; do
    if [ ${#options[@]} -gt 0 ]; then
      echo "Recent addresses:" >&2
      for idx in "${!options[@]}"; do printf "  %3d) %s\n" "$((idx+1))" "${options[$idx]}" >&2; done
      echo "  n) Enter a new address" >&2
      echo "  q) Quit" >&2
    else
      echo "No recent addresses." >&2
      read -rp "Enter host:port (default port 1234) or 'q': " user_input
      [ "$user_input" = "q" ] && return 1
      addr=$(normalize_address "$user_input")
      test_host_port "$addr" && { add_to_recent "$addr"; echo "$addr"; return 0; }
      echo "Connection failed." >&2; continue
    fi
    read -rp "Choice: " choice
    case "$choice" in
      q|Q) return 1 ;;
      n|N)
        read -rp "Enter host:port: " user_input
        addr=$(normalize_address "$user_input")
        test_host_port "$addr" && { add_to_recent "$addr"; echo "$addr"; return 0; }
        echo "Connection failed." >&2; continue ;;
      *)
        if [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] && [ "$choice" -le ${#options[@]} ]; then
          selected="${options[$((choice-1))]}"
          test_host_port "$selected" && { echo "$selected"; return 0; }
          echo "Connection failed for $selected. Removing." >&2
          tmpfile=$(mktemp); grep -vxF "$selected" "$RECENT_FILE" > "$tmpfile" || true; mv "$tmpfile" "$RECENT_FILE"
          mapfile -t options < <(tac "$RECENT_FILE" 2>/dev/null | awk '!seen[$0]++' || true)
          continue
        fi
        echo "Invalid." >&2 ;;
    esac
  done
}

# -- arg parsing -------------------------------------------------------------
MODEL_ARG=""
while [ $# -gt 0 ]; do
  case "$1" in
    --model) MODEL_ARG="$2"; shift 2 ;;
    --host) LM_STUDIO_HOST="$2"; shift 2 ;;
    --port) LM_STUDIO_PORT="$2"; shift 2 ;;
    --help|-h)
      echo "Usage: lmcode [--host HOST] [--port PORT] [--model MODEL]" >&2
      exit 0 ;;
    *) echo "Error: unknown option $1" >&2; exit 1 ;;
  esac
done

if [ ! -x "$CLI_BIN" ]; then
  echo "Error: claw binary not found. Build with: cd $REPO_ROOT/rust && cargo build --workspace" >&2
  exit 1
fi

# -- auto-discover LM Studio address ------------------------------------------
CURRENT_ADDR="${LM_STUDIO_HOST}:${LM_STUDIO_PORT}"
if ! test_host_port "$CURRENT_ADDR"; then
  echo "Default $CURRENT_ADDR unreachable — probing..." >&2
  if found_addr=$(auto_probe); then
    echo "Connected to $found_addr" >&2
    IFS=':' read -r LM_STUDIO_HOST LM_STUDIO_PORT <<< "$found_addr"
    add_to_recent "$found_addr"
  else
    echo "No auto-connect succeeded." >&2
    new_addr=$(interactive_select) || { echo "No address. Exiting." >&2; exit 1; }
    IFS=':' read -r LM_STUDIO_HOST LM_STUDIO_PORT <<< "$new_addr"
  fi
else
  add_to_recent "$CURRENT_ADDR"
fi

export OPENAI_BASE_URL="http://${LM_STUDIO_HOST}:${LM_STUDIO_PORT}/v1"
export OPENAI_API_KEY="${OPENAI_API_KEY:-local-model}"
export CLAW_RESILIENCE=force

# -- model selection ---------------------------------------------------------
MODEL_LIST_JSON=""
MODEL_LIST_JSON=$(fetch_models "$(echo $OPENAI_BASE_URL | sed 's|/v1$||')") || {
  echo "Error: cannot fetch model list." >&2; exit 1
}
IFS=$'\n' read -r -d '' -a MODELS < <(printf '%s\0' "$MODEL_LIST_JSON")
if [ ${#MODELS[@]} -eq 0 ]; then echo "Error: no models returned." >&2; exit 1; fi

if [ -z "$MODEL_ARG" ]; then
  if [ ${#MODELS[@]} -eq 1 ]; then
    MODEL_ARG="${MODELS[0]}"
    echo "Using model: $MODEL_ARG" >&2
  else
    echo "Available models:" >&2
    for idx in "${!MODELS[@]}"; do printf '  %3d) %s\n' "$((idx+1))" "${MODELS[$idx]}" >&2; done
    while true; do
      read -rp "Choose model number (1-${#MODELS[@]}): " choice
      if [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] && [ "$choice" -le ${#MODELS[@]} ]; then
        MODEL_ARG="${MODELS[$((choice-1))]}"; break
      fi
    done
  fi
fi

echo "Launching claw with model $MODEL_ARG" >&2
exec "$CLI_BIN" --model "$MODEL_ARG" --permission-mode danger-full-access
