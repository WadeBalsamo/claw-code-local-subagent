#!/usr/bin/env bash
# sprint_aggregate.sh — produce a combined diff + stats for all tasks in a sprint.
#
# Reads /tmp/claw-runs/_sprints/<sprint_id>.json (manifest written incrementally
# by run-claw-code as tasks complete), and emits:
#   /tmp/claw-runs/_sprints/<sprint_id>/combined.patch
#   /tmp/claw-runs/_sprints/<sprint_id>/combined_stats.json
#
# Usage:  sprint_aggregate.sh <sprint_id> <repo_dir>
# Exit:   0 on success, 1 on usage error, 2 on missing manifest, 3 on git error

set -euo pipefail

SPRINT_ID="${1:-}"
REPO_DIR="${2:-}"

if [[ -z "$SPRINT_ID" || -z "$REPO_DIR" ]]; then
    echo "usage: sprint_aggregate.sh <sprint_id> <repo_dir>" >&2
    exit 1
fi

SPRINT_DIR="/tmp/claw-runs/_sprints/${SPRINT_ID}"
MANIFEST="${SPRINT_DIR}.json"

if [[ ! -f "$MANIFEST" ]]; then
    echo "manifest not found: $MANIFEST" >&2
    exit 2
fi

mkdir -p "$SPRINT_DIR"
COMBINED_PATCH="${SPRINT_DIR}/combined.patch"
COMBINED_STATS="${SPRINT_DIR}/combined_stats.json"

SPRINT_BRANCH=$(python3 -c "import json,sys; print(json.load(open('$MANIFEST'))['sprint_branch'])")

# Combined diff: sprint branch tip vs main
git -C "$REPO_DIR" fetch origin main >/dev/null 2>&1 || true
MERGE_BASE=$(git -C "$REPO_DIR" merge-base "origin/main" "$SPRINT_BRANCH" 2>/dev/null || git -C "$REPO_DIR" merge-base "main" "$SPRINT_BRANCH")

git -C "$REPO_DIR" diff --no-pager "${MERGE_BASE}..${SPRINT_BRANCH}" > "$COMBINED_PATCH" || exit 3

# Stats
FILES=$(git -C "$REPO_DIR" diff --name-only "${MERGE_BASE}..${SPRINT_BRANCH}" | wc -l | tr -d ' ')
SHORTSTAT=$(git -C "$REPO_DIR" diff --shortstat "${MERGE_BASE}..${SPRINT_BRANCH}")
ADDED=$(echo "$SHORTSTAT" | grep -oE '[0-9]+ insertion' | grep -oE '[0-9]+' || echo 0)
REMOVED=$(echo "$SHORTSTAT" | grep -oE '[0-9]+ deletion' | grep -oE '[0-9]+' || echo 0)
TASKS=$(python3 -c "import json; print(len(json.load(open('$MANIFEST'))['tasks']))")

python3 - "$MANIFEST" "$COMBINED_STATS" "$FILES" "$ADDED" "$REMOVED" "$TASKS" "$MERGE_BASE" "$SPRINT_BRANCH" <<'PY'
import json, sys
manifest_path, out_path, files, added, removed, tasks, base, branch = sys.argv[1:]
with open(manifest_path) as f:
    m = json.load(f)
out = {
    "sprint_id": m["sprint_id"],
    "sprint_branch": branch,
    "merge_base": base,
    "tasks_total": int(tasks),
    "tasks_completed": sum(1 for t in m["tasks"] if t.get("status") == "done"),
    "files_changed": int(files),
    "lines_added": int(added),
    "lines_removed": int(removed),
    "prs_merged": [t.get("pr_url") for t in m["tasks"] if t.get("pr_url")],
    "task_ids": [t["task_id"] for t in m["tasks"]],
}
with open(out_path, "w") as f:
    json.dump(out, f, indent=2)
PY

echo "combined_patch=$COMBINED_PATCH"
echo "combined_stats=$COMBINED_STATS"
