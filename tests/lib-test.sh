#!/usr/bin/env bash
# lib-test.sh — shared test harness for shell test files.
# Source this at the top of each test file (after set -euo pipefail).
# Provides: pass(), fail(), assert_eq(), print_results()
#
# Expects $VERBOSE to be set before sourcing (defaults to false).

VERBOSE="${VERBOSE:-false}"

passed=0
failed=0
total=0

# Record a passing test.
pass() {
  total=$((total + 1))
  passed=$((passed + 1))
  if $VERBOSE; then
    printf "  \033[32mPASS\033[0m %s\n" "$1"
  fi
}

# Record a failing test with an optional detail message.
fail() {
  total=$((total + 1))
  failed=$((failed + 1))
  printf "  \033[31mFAIL\033[0m %s\n" "$1"
  if [ -n "${2:-}" ]; then
    printf "       %s\n" "$2"
  fi
}

# assert_eq ACTUAL EXPECTED LABEL — convenience wrapper.
assert_eq() {
  if [ "$1" = "$2" ]; then
    pass "$3"
  else
    fail "$3" "expected '$2', got '$1'"
  fi
}

# Print final results and exit with appropriate code.
print_results() {
  echo ""
  echo "=== Results: $passed/$total passed, $failed failed ==="
  if [ "$failed" -gt 0 ]; then
    echo "REGRESSION DETECTED"
    exit 1
  else
    echo "ALL PASS"
    exit 0
  fi
}
