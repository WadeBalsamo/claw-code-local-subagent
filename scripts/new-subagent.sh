#!/usr/bin/env bash
#
# Scaffold a new claw-code subagent preset from the annotated template.
#
# Usage:
#   scripts/new-subagent.sh <name> ["one-line description"]
#
# Creates scripts/presets/<name>.json (a copy of the template with preset_name
# and description filled in, and the _doc-only keys stripped). Then edit the
# system_prompt / model / allowed_tools to taste and invoke it via:
#   - the run_subagent MCP tool:  {"isolated": true, "preset": "<name>", "prompt": "..."}
#   - the isolated launcher:       scripts/launchers/run-claw-code.sh --agent <name> --dir . --plan "..."
#   - list it:                     the list_presets MCP tool
# See docs/subagents.md for the full guide.

set -euo pipefail

NAME="${1:-}"
DESC="${2:-Custom claw-code subagent.}"

if [ -z "$NAME" ]; then
  echo "usage: $0 <name> [\"one-line description\"]" >&2
  exit 2
fi
case "$NAME" in
  *[!a-zA-Z0-9_-]*)
    echo "error: name must contain only [A-Za-z0-9_-] (got: $NAME)" >&2
    exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$SCRIPT_DIR/presets/templates/subagent.template.json"
DEST="$SCRIPT_DIR/presets/$NAME.json"

[ -f "$TEMPLATE" ] || { echo "error: template not found: $TEMPLATE" >&2; exit 1; }
[ -e "$DEST" ] && { echo "error: preset already exists: $DEST" >&2; exit 1; }

PY="$(command -v python3 || command -v python || true)"
[ -n "$PY" ] || { echo "error: python3 is required" >&2; exit 1; }

NAME="$NAME" DESC="$DESC" "$PY" - "$TEMPLATE" "$DEST" <<'PYEOF'
import os, json, sys
template, dest = sys.argv[1], sys.argv[2]
with open(template) as f:
    data = json.load(f)
data["preset_name"] = os.environ["NAME"]
data["description"] = os.environ["DESC"]
# Drop documentation-only keys (those beginning with "_").
for key in [k for k in data if k.startswith("_")]:
    del data[key]
with open(dest, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
PYEOF

echo "Created subagent preset: $DEST"
echo
echo "Next steps:"
echo "  1. Edit $DEST — set system_prompt, model, provider, and allowed_tools."
echo "  2. Run it via the run_subagent MCP tool:"
echo "       {\"isolated\": true, \"preset\": \"$NAME\", \"prompt\": \"<your task>\"}"
echo "     or the launcher:"
echo "       scripts/launchers/run-claw-code.sh --agent $NAME --dir . --plan \"<your task>\""
echo "  See docs/subagents.md for the full reference."
