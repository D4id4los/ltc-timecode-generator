#!/usr/bin/env bash
# Deterministic SonarQube Cloud report exporter.
# Bash + curl + jq port of the former scripts/sonar-report.mjs.
#
# Pulls quality-gate status, measures, issues and security hotspots from the
# SonarQube Cloud Web API and writes fixed-name JSON + Markdown files to
# reports/sonar/, so a run is fully reproducible: same project state in,
# same files out.  Feeds those files to an AI agent or diffs them over time.
#
# Usage:
#   SONAR_TOKEN=<token> scripts/sonar-report.sh
# Environment:
#   SONAR_TOKEN   required — user token from sonarcloud.io (My Account → Security)
#   SONAR_HOST    optional — defaults to https://sonarcloud.io
#   SONAR_PROJECT optional — defaults to D4id4los_ltc-timecode-generator
#
# Requires jq on the dev machine (CI is unaffected — it runs the scanner action).

set -euo pipefail

HOST="${SONAR_HOST:-https://sonarcloud.io}"
PROJECT="${SONAR_PROJECT:-D4id4los_ltc-timecode-generator}"
TOKEN="${SONAR_TOKEN:-}"

if [ -z "$TOKEN" ]; then
  echo "error: SONAR_TOKEN is not set (create one at sonarcloud.io → My Account → Security)" >&2
  exit 1
fi

OUT_DIR="reports/sonar"
PAGE_SIZE=500
mkdir -p "$OUT_DIR"

api() { # path → body on stdout, exits on HTTP error
  local path="$1" body code
  set +e
  body="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $TOKEN" "${HOST}/api/${path}")"
  set -e
  code="${body##*$'\n'}"
  body="${body%$'\n'*}"
  if [ "$code" -lt 200 ] || [ "$code" -ge 300 ]; then
    echo "error: GET ${path%%\?*} → HTTP ${code}" >&2
    printf '%s\n' "$body" >&2
    exit 1
  fi
  printf '%s' "$body"
}

# paginated <path> <listKey>: follows paging until total is reached,
# prints the concatenated items as one JSON array.
paginated() {
  local path="$1" list_key="$2" sep="?" page=1 all="" seen=0
  [[ "$path" == *"?"* ]] && sep="&"
  while :; do
    local data chunk total
    data="$(api "${path}${sep}ps=${PAGE_SIZE}&p=${page}")"
    chunk="$(jq -c --arg k "$list_key" '.[$k][]' <<<"$data")"
    [ -n "$chunk" ] && all="${all}${all:+,}$(printf '%s' "$chunk" | paste -sd, -)"
    seen=$((seen + $(printf '%s' "$chunk" | grep -c . || true)))
    total="$(jq -r '.paging.total // 0' <<<"$data")"
    if [ "$seen" -ge "$total" ] || [ "$page" -gt 200 ]; then
      break
    fi
    page=$((page + 1))
  done
  echo "[${all}]"
}

# 1. Quality gate
qg="$(api "qualitygates/project_status?projectKey=${PROJECT}")"
jq -S . <<<"$qg" >"$OUT_DIR/quality-gate.json"

# 2. Key measures — the server rejects unknown metric keys, so strip any
# it names and retry instead of failing the whole report.
METRICS=(ncloc coverage duplicated_lines_density complexity cognitive_complexity \
  bugs code_smells vulnerabilities security_hotspots reliability_rating \
  security_rating security_review_rating maintainability_rating sqale_rating)
metric_keys=("${METRICS[@]}")
while :; do
  set +e
  raw="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer $TOKEN" \
    "${HOST}/api/measures/component?component=${PROJECT}&metricKeys=$(IFS=,; echo "${metric_keys[*]}")")"
  set -e
  code="${raw##*$'\n'}"
  raw="${raw%$'\n'*}"
  if [ "$code" -ge 200 ] && [ "$code" -lt 300 ]; then
    measures="$raw"
    break
  fi
  not_found="$(printf '%s' "$raw" | grep -o 'not found: [^"}]*' | sed 's/not found: //' | tr ',' '\n' | tr -d ' ' || true)"
  if [ -n "$not_found" ]; then
    echo "warning: dropping unknown metrics: $(printf '%s' "$not_found" | paste -sd, -)" >&2
    keep=()
    for k in "${metric_keys[@]}"; do
      grep -qx "$k" <<<"$not_found" || keep+=("$k")
    done
    if [ "${#keep[@]}" = 0 ]; then
      echo "error: no valid metrics left to request" >&2
      exit 1
    fi
    metric_keys=("${keep[@]}")
    continue
  fi
  echo "error: GET measures/component → HTTP ${code}" >&2
  printf '%s\n' "$raw" >&2
  exit 1
done
jq -S . <<<"$measures" >"$OUT_DIR/measures.json"

# 3. Open issues (all severities, main branch), sorted severity → component → line.
issues_json="$(jq -c '
  def sevrank: .severity as $s | (["INFO","MINOR","MAJOR","CRITICAL","BLOCKER"] | index($s));
  sort_by(sevrank // 99, .component, (.line // 0))
' <<<"$(paginated "issues/search?componentKeys=${PROJECT}&resolved=false" issues)")"
issues_total="$(jq 'length' <<<"$issues_json")"
printf '%s' "$issues_json" >"$OUT_DIR/.issues.tmp.json"
jq -S -n --argjson total "$issues_total" --slurpfile issues "$OUT_DIR/.issues.tmp.json" \
  '{total: $total, issues: $issues[0]}' >"$OUT_DIR/issues.json"
rm -f "$OUT_DIR/.issues.tmp.json"

# 4. Security hotspots
hotspots_json="$(paginated "hotspots/search?projectKey=${PROJECT}" hotspots)"
hotspots_total="$(jq 'length' <<<"$hotspots_json")"
printf '%s' "$hotspots_json" >"$OUT_DIR/.hotspots.tmp.json"
jq -S -n --argjson total "$hotspots_total" --slurpfile hotspots "$OUT_DIR/.hotspots.tmp.json" \
  '{total: $total, hotspots: $hotspots[0]}' >"$OUT_DIR/hotspots.json"
rm -f "$OUT_DIR/.hotspots.tmp.json"

# 5. Human-readable summary (same section order as the .mjs original).
SUMMARY="$OUT_DIR/summary.md"
{
  echo "# SonarQube Cloud report — ${PROJECT}"
  echo ""
  echo "Generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)  |  Branch: main"
  echo ""
  # NONE = no gate assigned (cannot fail); OK = passed; else failed, listing
  # the error conditions.
  jq -r --argjson qg "$qg" '
    .projectStatus.status as $st
    | if $st == "OK" then "## Quality gate: PASSED"
      elif $st == "NONE" then "## Quality gate: NO GATE DEFINED"
      else "## Quality gate: FAILED (" + (($qg.projectStatus.conditions // [])
             | map(select(.status == "ERROR") | .metricKey) | join(", ")) + ")"
      end' <<<"$qg"
  echo ""
  echo "## Measures"
  echo "| Metric | Value |"
  echo "|---|---|"
  jq -r --argjson measures "$measures" '
    ([$measures.component.measures[] | {(.metric): .value}] | add) as $m
    | def rating(v): {"1":"A","2":"B","3":"C","4":"D","5":"E"}[(v|tonumber|tostring)] // "?";
    "| Lines of code | \($m.ncloc // "?") |",
    "| Coverage | \($m.coverage // "n/a")% |",
    "| Duplication | \($m.duplicated_lines_density // "?")% |",
    "| Cyclomatic complexity | \($m.complexity // "?") |",
    "| Cognitive complexity | \($m.cognitive_complexity // "?") |",
    "| Bugs / Code smells / Vulns | \($m.bugs // 0) / \($m.code_smells // 0) / \($m.vulnerabilities // 0) |",
    "| Ratings (Reliability/Security/Maintainability) | \(rating($m.reliability_rating // 0)) / \(rating($m.security_rating // 0)) / \(rating($m.maintainability_rating // $m.sqale_rating // 0)) |"' <<<"$measures"
  echo ""
  echo "### New-code conditions"
  if [ "$(jq '(.projectStatus.conditions // []) | length' <<<"$qg")" = 0 ]; then
    echo ""
    echo "(no gate assigned — the portal cannot fail on anything)"
  else
    echo ""
    echo "| Metric | Comparator | Error threshold | Status | Actual |"
    echo "|---|---|---|---|---|"
    jq -r --argjson qg "$qg" '
      .projectStatus.conditions[]
      | "| \(.metricKey) | \(.comparator) | \(.errorThreshold) | \(.status) | \(.actualValue // "-") |"' <<<"$qg"
  fi
  echo ""
  echo "## Open issues: ${issues_total}"
  jq -r --slurpfile issues "$OUT_DIR/issues.json" '
    ($issues[0].issues | group_by(.severity) | map({key: .[0].severity, value: length}) | from_entries) as $b
    | ["BLOCKER","CRITICAL","MAJOR","MINOR","INFO"]
    | map(select($b[.] != null) | "- \(.): \($b[.])")[]
    ' -n </dev/null
  echo ""
  echo "| Severity | Rule | Location | Message |"
  echo "|---|---|---|---|"
  jq -r --slurpfile issues "$OUT_DIR/issues.json" --arg project "$PROJECT" '
    def esc: (gsub("\\|"; "\\|"));
    $issues[0].issues[:200][]
    | "| \(.severity) | \(.rule) | `\(.component | ltrimstr($project + ":")):\(.line // "-")` | \((.message // "") | esc) |"' -n </dev/null
  if [ "$issues_total" -gt 200 ]; then
    echo ""
    echo "… $((issues_total - 200)) more in issues.json"
  fi
  echo ""
  echo "## Security hotspots: ${hotspots_total} (details in hotspots.json)"
} >"$SUMMARY"

echo "Sonar report written to ${OUT_DIR}/"
echo "  quality-gate: $(jq -r --argjson qg "$qg" '.projectStatus.status' <<<"$qg")  issues: ${issues_total}  hotspots: ${hotspots_total}"
