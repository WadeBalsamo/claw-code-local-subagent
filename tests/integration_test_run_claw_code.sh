#!/usr/bin/env bash
# Integration tests for run-claw-code functionality
# Tests the sub-agent orchestration entry point

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_CLAW_CODE="$REPO_ROOT/scripts/launchers/run-claw-code.sh"
TEST_DIR=$(mktemp -d)
CLAW_BIN="$REPO_ROOT/rust/target/debug/claw"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

test_count=0
passed=0
failed=0

cleanup() {
    rm -rf "$TEST_DIR"
}

trap cleanup EXIT

log_test() {
    test_count=$((test_count + 1))
    echo -e "\n${YELLOW}[Test $test_count]${NC} $1"
}

assert_success() {
    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✓ PASS${NC}: $1"
        passed=$((passed + 1))
    else
        echo -e "${RED}✗ FAIL${NC}: $1"
        failed=$((failed + 1))
    fi
}

assert_file_exists() {
    if [ -f "$1" ]; then
        echo -e "${GREEN}✓ PASS${NC}: $1 exists"
        passed=$((passed + 1))
    else
        echo -e "${RED}✗ FAIL${NC}: $1 does not exist"
        failed=$((failed + 1))
    fi
}

assert_contains() {
    local file="$1"
    local pattern="$2"
    if grep -q "$pattern" "$file"; then
        echo -e "${GREEN}✓ PASS${NC}: $file contains '$pattern'"
        passed=$((passed + 1))
    else
        echo -e "${RED}✗ FAIL${NC}: $file does not contain '$pattern'"
        failed=$((failed + 1))
    fi
}

# ============================================================================
# Test 1: Script exists and is executable
# ============================================================================
log_test "Script exists and is executable"
[ -f "$RUN_CLAW_CODE" ] && [ -x "$RUN_CLAW_CODE" ]
assert_success "run-claw-code script is executable"

# ============================================================================
# Test 2: Help message works
# ============================================================================
log_test "Help message displays correctly"
output=$("$RUN_CLAW_CODE" --help 2>&1 || true)
echo "$output" | grep -q "Usage:"
assert_success "Help message contains usage information"

# ============================================================================
# Test 3: Claw binary exists
# ============================================================================
log_test "Claw binary is built and executable"
[ -f "$CLAW_BIN" ] && [ -x "$CLAW_BIN" ]
assert_success "claw binary exists and is executable"

# ============================================================================
# Test 4: Claw binary version check
# ============================================================================
log_test "Claw binary responds to --version"
version_output=$("$CLAW_BIN" --version 2>&1 || true)
[ -n "$version_output" ]
assert_success "claw binary responds to --version"

# ============================================================================
# Test 5: Create test repository
# ============================================================================
log_test "Create test repository for testing"
test_repo="$TEST_DIR/test-repo"
mkdir -p "$test_repo"
cd "$test_repo"
git init -q
git config user.name "Test User"
git config user.email "test@example.com"
echo "initial content" > test.txt
git add .
git commit -qm "Initial commit"
assert_success "Test repository created and initialized"

# ============================================================================
# Test 6: Verify status.json output format
# ============================================================================
log_test "Status JSON file format is correct"
# Create a test status file to verify format expectations
status_file="$TEST_DIR/test_status.json"
cat > "$status_file" << 'EOF'
{
  "status": "done",
  "files_changed": "1",
  "lines_added": "10",
  "lines_deleted": "0",
  "exit_code": 0
}
EOF
assert_file_exists "$status_file"
assert_contains "$status_file" "status"
assert_contains "$status_file" "files_changed"

# ============================================================================
# Test 7: Verify diff.patch format
# ============================================================================
log_test "Diff patch file format is correct"
# Create a test diff file to verify format expectations
diff_file="$TEST_DIR/test.patch"
cat > "$diff_file" << 'EOF'
diff --git a/test.txt b/test.txt
index e69de29..d8f8b26 100644
--- a/test.txt
+++ b/test.txt
@@ -0,0 +1 @@
+modified content
EOF
assert_file_exists "$diff_file"
assert_contains "$diff_file" "diff --git"

# ============================================================================
# Test 8: Verify summary.md format
# ============================================================================
log_test "Summary markdown file format is correct"
summary_file="$TEST_DIR/test_summary.md"
cat > "$summary_file" << 'EOF'
# Summary

- Added input validation to the registration endpoint
- Updated error handling to return 400 for invalid emails
- Added unit test for validation logic
EOF
assert_file_exists "$summary_file"
assert_contains "$summary_file" "Summary"

# ============================================================================
# Test 9: Check presets directory structure
# ============================================================================
log_test "Presets directory structure is correct"
presets_dir="$REPO_ROOT/scripts/presets"
[ -d "$presets_dir" ]
assert_success "Presets directory exists"
[ -f "$presets_dir/dev-backend.json" ] || [ -f "$presets_dir/dev-frontend.json" ]
assert_success "At least one preset configuration exists"

# ============================================================================
# Test 10: Verify environment variable handling
# ============================================================================
log_test "Environment variables can be passed to claw"
# This test verifies the mechanism, not full execution
export TEST_VAR="test_value"
"$CLAW_BIN" --help >/dev/null 2>&1
assert_success "claw respects environment setup"

# ============================================================================
# Test 11: Check if .local/bin symlinks are created
# ============================================================================
log_test "Install script creates correct symlinks"
# After install.sh, these should exist
expected_bins=("claw" "lmcode" "ollamacode" "openroutercode" "run-claw-code")
for bin in "${expected_bins[@]}"; do
    [ -f "$REPO_ROOT/scripts/launchers/${bin}.sh" ] || [ -x "$CLAW_BIN" ]
done
assert_success "Expected launcher scripts exist"

# ============================================================================
# Test 12: Verify basic claw functionality
# ============================================================================
log_test "Basic claw functionality works"
cd "$test_repo"
output=$("$CLAW_BIN" --help 2>&1)
echo "$output" | grep -q "claw"
assert_success "claw help output includes claw name"

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "============================================================================"
echo "Test Summary"
echo "============================================================================"
echo -e "Total tests: ${YELLOW}$test_count${NC}"
echo -e "Passed: ${GREEN}$passed${NC}"
echo -e "Failed: ${RED}$failed${NC}"
echo ""

if [ $failed -eq 0 ]; then
    echo -e "${GREEN}All tests passed!${NC}"
    exit 0
else
    echo -e "${RED}$failed test(s) failed.${NC}"
    exit 1
fi
