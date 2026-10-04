#!/usr/bin/env bash
# WP-T7 test-lint guardrail entry point — see scripts/test_lint.py for the
# rule definitions and AGENTS.md (Testing section) for the policy.
set -euo pipefail
exec python3 "$(dirname "$0")/test_lint.py" "$@"
