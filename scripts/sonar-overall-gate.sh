#!/usr/bin/env bash
# Overall-code quality-gate enforcement for SonarQube Cloud (CI step).
#
# The project's custom "LTC gate" cannot be *assigned* on this SonarCloud
# plan (HTTP 403 — see scripts/sonar-gate.sh), and the assigned built-in
# "Sonar way" gate only carries NEW-code conditions.  This script closes
# that gap for overall code: after the scan, it pulls the project-level
# measures from the Web API and fails (exit 1) when any overall-code
# condition is violated.  Conditions (decision D2 of the Phase-4 plan):
#
#   reliability_rating      ≤ A (1)
#   security_rating         ≤ A (1)
#   maintainability_rating  ≤ A (1)   (falls back to sqale_rating)
#   duplicated_lines_density  < 3 %
#   security_hotspots_reviewed = 100 %
#
# Overall-code COVERAGE is deliberately NOT gated: overall coverage is
# dominated by untestable GUI drawing code, and new-code coverage at 80 %
# (enforced by the assigned gate) already forces every change to be
# tested.  A coverage threshold can be turned on explicitly with
# --coverage-threshold N.
#
# Usage:
#   SONAR_TOKEN=<token> scripts/sonar-overall-gate.sh [--self-test]
# Options:
#   --self-test                  run the evaluator against the committed
#                                fixtures instead of the live API
#   --coverage-threshold N       additionally fail when coverage < N (default: off)
#   --max-reliability-rating N   override (1..5, default 1)
#   --max-security-rating N      override (1..5, default 1)
#   --max-maintainability-rating N  override (1..5, default 1)
#   --max-duplication N          override percent (default 3)
#   --min-hotspots-reviewed N    override percent (default 100)
# Exit codes:
#   0 pass · 1 condition violated · 2 broken (API/parse failure, or a
#   required measure is missing from the API response)
# Environment:
#   SONAR_TOKEN   required (except --self-test)
#   SONAR_HOST    optional — defaults to https://sonarcloud.io
#   SONAR_PROJECT optional — defaults to D4id4los_ltc-timecode-generator

set -euo pipefail

HOST="${SONAR_HOST:-https://sonarcloud.io}"
PROJECT="${SONAR_PROJECT:-D4id4los_ltc-timecode-generator}"
TOKEN="${SONAR_TOKEN:-}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="${SCRIPT_DIR}/fixtures/sonar-overall-gate-fixtures.json"

# ── Pure core: evaluate_measures ─────────────────────────────────────────
# Input JSON: {measures: {metric: value}, thresholds: {overrides...}}
# Output: JSON array of violation strings (empty = pass).  Required
# measures that are absent produce "missing:X" entries — the caller turns
# those into exit 2 (broken), the others into exit 1 (violated).
read -r -d '' EVALUATE_JQ <<'JQ' || true
def num: tonumber;
def get($m): .measures[$m];
. as $in
| ($in.thresholds // {}) as $t
| ([$t.max_reliability_rating // 1, $t.max_security_rating // 1, $t.max_maintainability_rating // 1] | map(. | num)) as $maxes
| (($t.max_duplication // 3) | num) as $dup_max
| (($t.min_hotspots_reviewed // 100) | num) as $hs_min
| [
  (if (get("reliability_rating") | not) then "missing:reliability_rating"
   elif get("reliability_rating") | num > $maxes[0]
   then "reliability_rating \(get("reliability_rating")) is worse than A (max \($maxes[0]))"
   else empty end),
  (if (get("security_rating") | not) then "missing:security_rating"
   elif get("security_rating") | num > $maxes[1]
   then "security_rating \(get("security_rating")) is worse than A (max \($maxes[1]))"
   else empty end),
  ((get("maintainability_rating") // get("sqale_rating")) as $m
   | if ($m | not) then "missing:maintainability_rating"
     elif ($m | num) > $maxes[2]
     then "maintainability_rating \($m) is worse than A (max \($maxes[2]))"
     else empty end),
  (if (get("duplicated_lines_density") | not) then "missing:duplicated_lines_density"
   elif get("duplicated_lines_density") | num > $dup_max
   then "duplicated_lines_density \(get("duplicated_lines_density"))% exceeds \($dup_max)%"
   else empty end),
  (if (get("security_hotspots_reviewed") | not) then "missing:security_hotspots_reviewed"
   elif get("security_hotspots_reviewed") | num < $hs_min
   then "security_hotspots_reviewed \(get("security_hotspots_reviewed"))% below \($hs_min)%"
   else empty end),
  (if (($t | has("coverage_lt")) | not) then empty
     elif (get("coverage") | not) then "missing:coverage"
     elif (get("coverage") | num) < ($t.coverage_lt | num)
     then "coverage \(get("coverage"))% below \($t.coverage_lt)%"
     else empty end)
] | map(select(. != ""))
JQ

evaluate_measures() { # measures_json thresholds_json → violations JSON array
  jq -c -e "$EVALUATE_JQ" <<<"{\"measures\": $1, \"thresholds\": $2}"
}

# ── Self-test ────────────────────────────────────────────────────────────
self_test() {
  local failures=0 n=0
  while IFS=$'\t' read -r name measures thresholds expected; do
    n=$((n + 1))
    local got
    got="$(evaluate_measures "$measures" "$thresholds")"
    if [ "$got" != "$expected" ]; then
      echo "FAIL: ${name}" >&2
      echo "  expected: ${expected}" >&2
      echo "  got:      ${got}" >&2
      failures=$((failures + 1))
    else
      echo "ok: ${name}"
    fi
  done < <(jq -r '.cases[]
      | [.name,
         (.measures | tostring),
         (.thresholds | tostring),
         (.expected_violations | tostring)]
      | @tsv' "$FIXTURES")
  if [ "$failures" -ne 0 ]; then
    echo "self-test FAILED: ${failures}/${n} cases" >&2
    exit 2
  fi
  echo "self-test passed: ${n} cases"
}

# ── Argument parsing ─────────────────────────────────────────────────────
THRESHOLDS='{}'
SELF_TEST=0
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) SELF_TEST=1 ;;
    --coverage-threshold) THRESHOLDS="$(jq -c -n --argjson t "$THRESHOLDS" --argjson v "$2" '$t + {coverage_lt: $v}')" ; shift ;;
    --max-reliability-rating) THRESHOLDS="$(jq -c -n --argjson t "$THRESHOLDS" --argjson v "$2" '$t + {max_reliability_rating: $v}')" ; shift ;;
    --max-security-rating) THRESHOLDS="$(jq -c -n --argjson t "$THRESHOLDS" --argjson v "$2" '$t + {max_security_rating: $v}')" ; shift ;;
    --max-maintainability-rating) THRESHOLDS="$(jq -c -n --argjson t "$THRESHOLDS" --argjson v "$2" '$t + {max_maintainability_rating: $v}')" ; shift ;;
    --max-duplication) THRESHOLDS="$(jq -c -n --argjson t "$THRESHOLDS" --argjson v "$2" '$t + {max_duplication: $v}')" ; shift ;;
    --min-hotspots-reviewed) THRESHOLDS="$(jq -c -n --argjson t "$THRESHOLDS" --argjson v "$2" '$t + {min_hotspots_reviewed: $v}')" ; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

if [ "$SELF_TEST" = "1" ]; then
  self_test
  exit 0
fi

if [ -z "$TOKEN" ]; then
  echo "error: SONAR_TOKEN is not set (create one at sonarcloud.io → My Account → Security)" >&2
  exit 2
fi

# ── Fetch overall measures from the Web API ──────────────────────────────
# The server rejects metric keys it does not know (the plan dropped
# `maintainability_rating` at some point) — drop named keys and retry,
# mirroring scripts/sonar-report.sh.
METRICS=(reliability_rating security_rating maintainability_rating sqale_rating \
  duplicated_lines_density security_hotspots_reviewed)
if [ -n "${THRESHOLDS##*coverage_lt*}" ] || ! jq -e 'has("coverage_lt")' <<<"$THRESHOLDS" >/dev/null; then
  :
else
  METRICS+=(coverage)
fi
while :; do
  set +e
  raw="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $TOKEN" \
    "${HOST}/api/measures/component?component=${PROJECT}&metricKeys=$(IFS=,; echo "${METRICS[*]}")")"
  set -e
  code="${raw##*$'\n'}"
  raw="${raw%$'\n'*}"
  if [ "$code" -ge 200 ] && [ "$code" -lt 300 ]; then
    measures_response="$raw"
    break
  fi
  not_found="$(printf '%s' "$raw" | grep -o 'not found: [^"}]*' | sed 's/not found: //' | tr ',' '\n' | tr -d ' ' || true)"
  if [ -n "$not_found" ]; then
    echo "warning: dropping unknown metrics: $(printf '%s' "$not_found" | paste -sd, -)" >&2
    keep=()
    for k in "${METRICS[@]}"; do
      grep -qx "$k" <<<"$not_found" || keep+=("$k")
    done
    if [ "${#keep[@]}" = 0 ]; then
      echo "error: no valid metrics left to request" >&2
      exit 2
    fi
    METRICS=("${keep[@]}")
    continue
  fi
  echo "error: GET measures/component → HTTP ${code}" >&2
  printf '%s\n' "$raw" >&2
  exit 2
done

measures="$(jq -c '[.component.measures[] | {(.metric): .value}] | add // {}' <<<"$measures_response")"

violations="$(evaluate_measures "$measures" "$THRESHOLDS")" || { echo "error: evaluator failed" >&2; exit 2; }

missing="$(jq -r '[.[] | select(startswith("missing:"))] | join(", ")' <<<"$violations")"
failed="$(jq -r '[.[] | select(startswith("missing:") | not)] | length' <<<"$violations")"

echo "Overall-code measures: $(jq -r 'to_entries | map("\(.key)=\(.value)") | join(", ")' <<<"$measures")"
if [ "$failed" -gt 0 ]; then
  echo "OVERALL-CODE GATE FAILED (${failed} condition(s)):" >&2
  jq -r '.[] | select(startswith("missing:") | not)' <<<"$violations" >&2
  exit 1
fi
echo "Overall-code gate: PASSED (ratings ≤ A, duplication < 3%, hotspots 100% reviewed)"
