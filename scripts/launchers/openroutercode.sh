#!/usr/bin/env bash
# openroutercode — OpenRouter launcher for claw-code
# Installed by install.sh to ~/.local/bin/openroutercode
# Browse OpenRouter's model catalog, pick a model, and launch claw.
set -euo pipefail

REPO_ROOT="${CLAW_CODE_ROOT:-$(cd "$(dirname "$(readlink -f "$0")")/../.." && pwd)}"
CLI_BIN="$REPO_ROOT/rust/target/debug/claw"
if [ ! -x "$CLI_BIN" ]; then
  CLI_BIN="$REPO_ROOT/rust/target/release/claw"
fi

CONFIG_DIR="$HOME/.config/openroutercode"
if [ ! -d "$CONFIG_DIR" ] && [ -d "$HOME/.config/opencode" ]; then CONFIG_DIR="$HOME/.config/opencode"; fi
ENV_FILE="$CONFIG_DIR/.env"
MODEL_FILE="$CONFIG_DIR/selected_model"
RECENTS_FILE="$CONFIG_DIR/recent_models"
CACHE_FILE="$CONFIG_DIR/openrouter_models_cache.tsv"
CACHE_TTL=$(( 60 * 60 * 6 ))
MAX_RECENTS=5
DEFAULT_MODEL="deepseek/deepseek-v4-pro:nitro"
LEGACY_DEFAULT_MODEL="deepseek/deepseek-v4-pro"
OPENROUTER_API="https://openrouter.ai/api/v1/models?supported_parameters=tools"
PAGE_SIZE=18

mkdir -p "$CONFIG_DIR"

# shellcheck disable=SC1090
[ -f "$ENV_FILE" ] && { set -a; source "$ENV_FILE"; set +a; }

if [ ! -x "$CLI_BIN" ]; then
  echo "Error: claw binary not found at $CLI_BIN" >&2
  echo "Build with: cd $REPO_ROOT/rust && cargo build --workspace" >&2
  exit 1
fi

if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  echo "OpenRouter API key not configured." >&2
  read -rsp "Paste OPENROUTER_API_KEY: " OPENROUTER_API_KEY; echo
  OPENROUTER_API_KEY="$(echo "$OPENROUTER_API_KEY" | tr -d '[:space:]')"
  [ -n "$OPENROUTER_API_KEY" ] || { echo "No key entered." >&2; exit 1; }
  echo "OPENROUTER_API_KEY=$OPENROUTER_API_KEY" > "$ENV_FILE"
  chmod 600 "$ENV_FILE"
  echo "Saved to $ENV_FILE" >&2
fi

# -- helpers ------------------------------------------------------------------
HAS_FZF=0; command -v fzf >/dev/null 2>&1 && HAS_FZF=1
tui_clear() { command -v tput >/dev/null 2>&1 && tput clear 2>/dev/null || printf '\033[2J\033[H'; }
fmt_ctx() { local n="${1:-0}"; if [ "$n" -ge 1000000 ]; then awk -v n="$n" 'BEGIN{printf "%.0fM",n/1000000}'; elif [ "$n" -ge 1000 ]; then printf '%dK' $((n/1000)); else printf '%d' "$n"; fi; }
fmt_cost() { printf '$%.4f/M' "${1:-0}"; }
bold() { command -v tput >/dev/null 2>&1 && printf '%s%s%s' "$(tput bold 2>/dev/null)" "$*" "$(tput sgr0 2>/dev/null)" || printf '%s' "$*"; }

save_recent() {
  local m="$1"; [ -n "$m" ] || return 0
  { printf '%s\n' "$m"; [ -f "$RECENTS_FILE" ] && cat "$RECENTS_FILE" || true; } | awk 'NF && !seen[$0]++' | head -n "$MAX_RECENTS" > "$RECENTS_FILE.tmp"
  mv "$RECENTS_FILE.tmp" "$RECENTS_FILE"
}

cache_fresh() {
  [ -f "$CACHE_FILE" ] || return 1
  local mtime
  mtime="$(stat -c %Y "$CACHE_FILE" 2>/dev/null || stat -f %m "$CACHE_FILE" 2>/dev/null || echo 0)"
  local age=$(( $(date +%s) - mtime ))
  [ "$age" -lt "$CACHE_TTL" ]
}

refresh_cache() {
  echo "Fetching OpenRouter tool-capable models..." >&2
  local tj tt; tj="$(mktemp)" tt="$(mktemp)"
  if ! curl -fsSL --max-time 20 "$OPENROUTER_API" -o "$tj" 2>/dev/null; then rm -f "$tj" "$tt"; echo "Network error." >&2; return 1; fi
  python3 - "$tj" "$tt" <<'PY'
import json, sys
def flt(v):
    try: return float(v)
    except: return 0.0

def derive_cats(mid, name, desc, ctx, in_c):
    t = ' '.join([mid, name, desc]).lower()
    c = set()
    prov = mid.split('/')[0].lower() if '/' in mid else 'other'
    c.add(prov)
    FAMILY_ALIASES = {
        'anthropic': ['claude'], 'openai': ['gpt-', 'o1-', 'o3-', 'o4-'],
        'google': ['gemini', 'palm'], 'meta-llama': ['llama'],
        'mistralai': ['mistral', 'mixtral', 'codestral'], 'deepseek': ['deepseek'],
        'qwen': ['qwen'], 'x-ai': ['grok'], 'cohere': ['command'],
        'nvidia': ['nvidia', 'nemotron'], 'microsoft': ['phi-', 'wizardlm'],
        'amazon': ['nova', 'titan'], 'perplexity': ['r1-', 'sonar'],
        'nousresearch': ['hermes', 'nous'],
    }
    for fam, keywords in FAMILY_ALIASES.items():
        if prov == fam or any(k in t for k in keywords): c.add(fam)
    if ctx >= 1_000_000: c.add('long-context')
    elif ctx >= 128_000: c.add('large-context')
    if in_c == 0: c.add('free')
    elif in_c <= 0.30: c.add('budget')
    elif in_c <= 2.00: c.add('value')
    elif in_c <= 8.00: c.add('balanced')
    else: c.add('premium')
    CAPS = [
        ('cod','coding'),('program','coding'),('software','coding'),('engineer','coding'),
        ('debug','coding'),('swe-','coding'),('develo','coding'),('script','coding'),
        ('reason','reasoning'),('math','reasoning'),('logic','reasoning'),('think','reasoning'),
        ('agent','agents'),('agentic','agents'),('tool','agents'),('function call','agents'),
        ('vision','multimodal'),('image','multimodal'),('multimodal','multimodal'),
        ('writ','writing'),('creat','writing'),('content','writing'),
        ('instruct','instruct'),('chat','chat'),('search','search'),
        ('flash','fast'),('turbo','fast'),('mini','fast'),('haiku','fast'),
        ('opus','flagship'),('ultra','flagship'),('pro','flagship'),
    ]
    for kw, cat in CAPS:
        if kw in t: c.add(cat)
    return ','.join(sorted(c))

src, dst = sys.argv[1], sys.argv[2]
with open(src, encoding='utf-8') as f:
    models = json.load(f).get('data', [])
rows = []
for m in models:
    if 'tools' not in set(m.get('supported_parameters') or []): continue
    mid = m.get('id', ''); name = m.get('name', mid); ctx = int(m.get('context_length') or 0)
    p = m.get('pricing') or {}; in_c = flt(p.get('prompt')) * 1_000_000; out_c = flt(p.get('completion')) * 1_000_000
    desc = (m.get('description') or '').replace('\n', ' ').strip()[:200]
    prov = mid.split('/')[0] if '/' in mid else 'other'
    cats = derive_cats(mid, name, desc, ctx, in_c)
    rows.append((mid, name, prov, ctx, in_c, out_c, cats, 'text->text', desc))
rows.sort(key=lambda r: (r[4], -r[3]))
with open(dst, 'w', encoding='utf-8') as out:
    for r in rows:
        out.write('\t'.join([r[0], r[1], r[2], str(r[3]), f'{r[4]:.4f}', f'{r[5]:.4f}', r[6], r[7], r[8]]) + '\n')
PY
  mv "$tt" "$CACHE_FILE"; rm -f "$tj"
  printf 'Cached: %d models -> %s\n' "$(wc -l < "$CACHE_FILE")" "$CACHE_FILE" >&2
}

filter_catalog() {
  local prov="$1" cat="$2" max_in="$3" min_ctx="$4" term="$5"
  [ -f "$CACHE_FILE" ] || return 0
  awk -F'\t' -v p="$prov" -v c="$cat" -v mi="$max_in" -v mc="$min_ctx" -v s="$term" '
  BEGIN { IGNORECASE=1 }
  { ok = 1
    if (p != "" && $3 !~ p) ok=0
    if (c != "" && $7 !~ c) ok=0
    if (s != "" && ($1 " " $2 " " $9) !~ s) ok=0
    if (mi != "" && ($5 + 0) > (mi + 0)) ok=0
    if (mc != "" && ($4 + 0) < (mc + 0)) ok=0
    if (ok) print
  }' "$CACHE_FILE"
}

# -- fzf browser --------------------------------------------------------------
browse_fzf() {
  maybe_refresh; [ -f "$CACHE_FILE" ] || { echo "No catalog." >&2; return 1; }
  local selected
  selected=$(awk -F'\t' '{printf "%-44s  %5s  in:%-12s  out:%-12s  %s\n", $1, $4, "$"$5"/M", "$"$6"/M", $7}' "$CACHE_FILE" | \
    fzf --height=90% --layout=reverse --info=inline --prompt="model> " \
      --header="Enter=select  Ctrl-C=cancel" \
      --preview="awk -F'\t' -v id={1} '\$1==id{printf \"%s\n\nCtx: %s\nIn: \$%s/M\nOut: \$%s/M\nCats: %s\n\n%s\",\$2,\$4,\$5,\$6,\$7,\$9}' \"$CACHE_FILE\"" \
      --preview-window=right:45%:wrap 2>/dev/null) || return 1
  MODEL="$(awk '{print $1}' <<<"$selected")"; [ -n "$MODEL" ]
}

# -- TUI browser --------------------------------------------------------------
browse_tui() {
  maybe_refresh
  if [ ! -f "$CACHE_FILE" ]; then echo "No catalog. Use 'c' to enter model manually." >&2; return 1; fi
  local f_prov="" f_cat="" f_in="" f_ctx="" f_term="" page=0
  local -a rows=(); local total=0 total_pages=0 filters_dirty=1
  local ACTION="" ACTION_ARG=""

  while true; do
    if [ "$filters_dirty" -eq 1 ]; then
      rows=()
      while IFS= read -r row; do
        rows+=("$row")
      done < <(filter_catalog "$f_prov" "$f_cat" "$f_in" "$f_ctx" "$f_term")
      total="${#rows[@]}"; total_pages=$(( (total + PAGE_SIZE - 1) / PAGE_SIZE )); [ "$total_pages" -lt 1 ] && total_pages=1
      page=0; filters_dirty=0
    fi
    [ "$page" -ge "$total_pages" ] && page=$(( total_pages - 1 ))
    tui_clear
    echo "Models: $(wc -l < "$CACHE_FILE") cached, $total match" >&2
    echo "p:$f_prov k:$f_cat \$:$f_in x:$f_ctx s:$f_term  Page $((page+1))/$total_pages" >&2
    echo "---" >&2
    local start=$(( page * PAGE_SIZE ))
    local end=$(( start + PAGE_SIZE ))
    [ "$end" -gt "$total" ] && end="$total"
    if [ "$total" -eq 0 ]; then echo "(no models — type 'a' to clear)" >&2
    else
      local i="$start"
      while [ "$i" -lt "$end" ]; do
        local row="${rows[$i]}"; local id name prov ctx in_c out_c cats mod desc
        IFS=$'\t' read -r id name prov ctx in_c out_c cats mod desc <<< "$row"
        printf ' %3d)  %-40s  %5s ctx\n' "$((i+1))" "$id" "$(fmt_ctx "$ctx")" >&2
        printf '       in: %s  out: %s  %s\n' "$(fmt_cost "$in_c")" "$(fmt_cost "$out_c")" "$cats" >&2
        i=$(( i + 1 ))
      done
    fi
    echo "---" >&2
    echo "p <prov> k <cat> \$ <max$/M> x <minCtx> s <text> a(clear) n/b m <n> r(efresh) q" >&2
    read -rp "> " input || return 1
    local trimmed; trimmed="$(printf '%s' "$input" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
    case "$trimmed" in q|Q) return 1 ;; r|R) refresh_cache || true; filters_dirty=1 ;; n|N) page=$((page+1)) ;; b|B) page=$((page-1)); [ "$page" -lt 0 ] && page=0 ;; "") ;; *)
      local -a toks; read -ra toks <<< "$trimmed"; local i=0 ntoks="${#toks[@]}"
      while [ "$i" -lt "$ntoks" ]; do
        local t="${toks[$i]}"; i=$((i+1)); local nxt=""; [ "$i" -lt "$ntoks" ] && nxt="${toks[$i]}"
        case "$t" in
          p) if [ -n "$nxt" ]; then f_prov="$nxt"; i=$((i+1)); else f_prov=""; fi ;;
          k) if [ -n "$nxt" ]; then f_cat="$nxt"; i=$((i+1)); else f_cat=""; fi ;;
          '$'|'$'*) local val="${t#'$'}"; [ -z "$val" ] && [ -n "$nxt" ] && val="$nxt" && i=$((i+1)); f_in="$val" ;;
          x) if [ -n "$nxt" ]; then f_ctx="$nxt"; i=$((i+1)); else f_ctx=""; fi ;;
          s) local rest_arr=("${toks[@]:$i}"); f_term="${rest_arr[*]:-}"; i="$ntoks" ;;
          m) ACTION_ARG="$nxt"
            local sel_n="$ACTION_ARG"; if [[ ! "$sel_n" =~ ^[0-9]+$ ]]; then read -rp "Number: " sel_n; fi
            if [[ "$sel_n" =~ ^[0-9]+$ ]]; then
              local sel_idx=$(( sel_n - 1 ))
              if [ "$sel_idx" -ge 0 ] && [ "$sel_idx" -lt "$total" ]; then
                IFS=$'\t' read -r MODEL _ <<< "${rows[$sel_idx]}"; echo "Selected: $MODEL" >&2; return 0
              fi; echo "Out of range." >&2
            fi ;;
          a) f_prov=""; f_cat=""; f_in=""; f_ctx=""; f_term="" ;;
          *) ;;
        esac
      done
      filters_dirty=1 ;;
    esac
  done
}

maybe_refresh() { cache_fresh || refresh_cache || true; }

# -- main ---------------------------------------------------------------------
MODEL="${1:-}"
SKIP_SAVE=0

if [ -z "$MODEL" ]; then
  maybe_refresh
  SAVED=""; [ -f "$MODEL_FILE" ] && SAVED="$(cat "$MODEL_FILE")"
  if [ "$SAVED" = "$LEGACY_DEFAULT_MODEL" ]; then
    SAVED="$DEFAULT_MODEL"
    printf '%s\n' "$DEFAULT_MODEL" > "$MODEL_FILE"
  fi
  EFFECTIVE="${SAVED:-$DEFAULT_MODEL}"
  echo "default: $EFFECTIVE" >&2
  RECENTS=()
  if [ -f "$RECENTS_FILE" ]; then
    while IFS= read -r recent; do
      RECENTS+=("$recent")
    done < <(head -n "$MAX_RECENTS" "$RECENTS_FILE")
  fi
  if [ "${#RECENTS[@]}" -gt 0 ]; then
    i=1; for m in "${RECENTS[@]}"; do printf '  %d) %s\n' "$i" "$m" >&2; i=$((i+1)); done
  fi
  echo "Enter/d=default b=browse c=custom q=quit" >&2
  read -rp "> " pick; pick="${pick:-d}"
  if [[ "$pick" =~ ^[1-9][0-9]*$ ]]; then
    idx=$((pick-1)); [ "$idx" -ge "${#RECENTS[@]}" ] && { echo "Invalid." >&2; exit 1; }
    MODEL="${RECENTS[$idx]}"
  else
    case "$pick" in
      d|D|"") MODEL="$EFFECTIVE"; [ "$MODEL" = "$SAVED" ] && SKIP_SAVE=1 ;;
      b|B) if [ "$HAS_FZF" -eq 1 ]; then browse_fzf || browse_tui || { echo "No model." >&2; exit 0; }; else browse_tui || { echo "No model." >&2; exit 0; }; fi ;;
      c|C) read -rp "Model id: " MODEL; MODEL="$(echo "$MODEL" | tr -d '[:space:]')" ;;
      q|Q) exit 0 ;;
      *) echo "Invalid." >&2; exit 1 ;;
    esac
  fi
fi

[ -n "$MODEL" ] || { echo "No model." >&2; exit 1; }

if [ "$SKIP_SAVE" -eq 0 ]; then
  SAVED=""; [ -f "$MODEL_FILE" ] && SAVED="$(cat "$MODEL_FILE")"
  if [ "$MODEL" != "${SAVED:-}" ]; then
    read -rp "Save '$MODEL' as default? [Y/n]: " sv; sv="${sv:-Y}"
    [[ "$sv" =~ ^[Yy]$ ]] && { echo "$MODEL" > "$MODEL_FILE"; echo "Saved." >&2; }
  fi
fi

save_recent "$MODEL"

export OPENAI_BASE_URL="https://openrouter.ai/api/v1/chat/completions"
export OPENAI_API_KEY="$OPENROUTER_API_KEY"
export HTTP_REFERER="https://localhost"
export X_TITLE="claw-code"
export CLAW_RESILIENCE=none

echo "Launching claw with $MODEL via OpenRouter" >&2
exec "$CLI_BIN" --model "$MODEL" --permission-mode workspace-write
