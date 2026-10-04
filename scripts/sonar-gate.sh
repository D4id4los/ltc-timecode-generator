#!/usr/bin/env bash
# One-shot SonarCloud quality-gate setup (idempotent — safe to re-run).
# Bash + curl + jq port of the former scripts/sonar-gate.mjs.
#
# Creates (or finds) the "LTC gate" quality gate, ensures it carries the
# new-code conditions below, assigns it to the project, and prints the
# final gate definition verbatim for eyeball verification.
#
# Usage:
#   SONAR_TOKEN=<token> scripts/sonar-gate.sh
# Environment:
#   SONAR_TOKEN   required — user token from sonarcloud.io (My Account → Security)
#   SONAR_HOST    optional — defaults to https://sonarcloud.io
#   SONAR_ORG     optional — defaults to d4id4los
#   SONAR_PROJECT optional — defaults to D4id4los_ltc-timecode-generator
#
# If the API shapes have drifted (create_condition/select parameter names,
# rating op/error encodings), the documented fallback is the ~10-minute
# portal path: SonarCloud → Quality Gates → copy "Sonar way" → edit
# conditions → set as the project's gate.

set -euo pipefail

HOST="${SONAR_HOST:-https://sonarcloud.io}"
ORG="${SONAR_ORG:-d4id4los}"
PROJECT="${SONAR_PROJECT:-D4id4los_ltc-timecode-generator}"
TOKEN="${SONAR_TOKEN:-}"
# SonarCloud rejects qualitygates/* calls without an organization parameter.
ORGQ="organization=${ORG}"

if [ -z "$TOKEN" ]; then
  echo "error: SONAR_TOKEN is not set (create one at sonarcloud.io → My Account → Security)" >&2
  exit 1
fi

GATE="LTC gate"
# Expressed the way the API wants it (a condition FAILS when op holds):
# coverage fails when below 80% (LT 80); ratings fail when worse than A
# (GT 1) — i.e. coverage > 80% and rating A on new code.
CONDITIONS=("new_coverage:LT:80" "new_reliability_rating:GT:1" "new_security_rating:GT:1" "new_maintainability_rating:GT:1")

api_get() { # path → body on stdout, non-zero exit on HTTP error
  local path="$1"
  curl -fsS -H "Authorization: Bearer $TOKEN" "${HOST}/api/${path}"
}

HTTP_STATUS=0
# api_post <path> → body on stdout; on HTTP error prints "HTTP <code>\n<body>"
# to stderr, sets HTTP_STATUS, and returns 1.
api_post() {
  local path="$1" body code
  set +e
  body="$(curl -sS -w '\n%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" "${HOST}/api/${path}")"
  set -e
  code="${body##*$'\n'}"
  body="${body%$'\n'*}"
  HTTP_STATUS="$code"
  if [ "$code" -lt 200 ] || [ "$code" -ge 300 ]; then
    printf 'HTTP %s\n%s\n' "$code" "$body" >&2
    return 1
  fi
  if [ -n "$body" ]; then printf '%s' "$body"; fi
  return 0
}

# 1. Find the gate by name, create it if absent.
gate_meta="$(api_get "qualitygates/list?${ORGQ}")"
gate_id="$(jq -r --arg n "$GATE" '.qualitygates[] | select(.name == $n) | .id' <<<"$gate_meta")"
if [ -n "$gate_id" ] && [ "$gate_id" != "null" ]; then
  echo "gate \"${GATE}\" already exists (id ${gate_id})"
else
  gate_id="$(api_post "qualitygates/create?${ORGQ}&name=$(jq -rn --arg n "$GATE" '$n|@uri')" | jq -r '.id')"
  echo "created gate \"${GATE}\" (id ${gate_id})"
fi

# 2. Ensure every condition is present (matched by metric+op+error).
shown="$(api_get "qualitygates/show?${ORGQ}&name=$(jq -rn --arg n "$GATE" '$n|@uri')")"
for c in "${CONDITIONS[@]}"; do
  metric="${c%%:*}"; rest="${c#*:}"; op="${rest%%:*}"; error="${rest##*:}"
  present="$(jq -r --arg m "$metric" --arg o "$op" --arg e "$error" \
    '[.conditions[] | select(.metric == $m and .op == $o and ((.error|tostring) == $e))] | length > 0' \
    <<<"$shown")"
  if [ "$present" = "true" ]; then
    echo "condition present: ${metric} ${op} ${error}"
    continue
  fi
  api_post "qualitygates/create_condition?${ORGQ}&gateId=${gate_id}&metric=${metric}&op=${op}&error=${error}" >/dev/null
  echo "condition added: ${metric} ${op} ${error}"
  shown="$(api_get "qualitygates/show?${ORGQ}&name=$(jq -rn --arg n "$GATE" '$n|@uri')")"
done

# 3. Assign the gate to the project. Some SonarCloud plans reject
# custom-gate assignment ("Organization … is not allowed to modify
# Quality gates", HTTP 403). In that case fall back to verifying that
# the already-assigned gate covers all four condition metrics — the
# built-in "Sonar way" does (it is a strict superset: it also enforces
# new-code duplication < 3% and 100% hotspots reviewed).
assigned="$GATE"
if api_post "qualitygates/select?${ORGQ}&projectKey=$(jq -rn --arg p "$PROJECT" '$p|@uri')&gateId=${gate_id}" >/dev/null; then
  echo "gate \"${GATE}\" assigned to ${PROJECT}"
else
  status="$HTTP_STATUS"
  if [ "$status" != "403" ]; then
    exit 1
  fi
  echo "warning: assigning the custom gate failed (HTTP 403 — plan does not allow custom gates)"
  cur="$(api_get "qualitygates/get_by_project?${ORGQ}&project=$(jq -rn --arg p "$PROJECT" '$p|@uri')")"
  assigned="$(jq -r '.qualityGate.name // empty' <<<"$cur")"
  if [ -z "$assigned" ]; then
    echo "error: project has no assigned gate to fall back to" >&2
    exit 1
  fi
  assigned_show="$(api_get "qualitygates/show?${ORGQ}&name=$(jq -rn --arg n "$assigned" '$n|@uri')")"
  missing=""
  for c in "${CONDITIONS[@]}"; do
    metric="${c%%:*}"
    if ! jq -e -r --arg m "$metric" '[.conditions[].metric] | index($m) != null' <<<"$assigned_show" >/dev/null; then
      missing="${missing:+$missing, }${metric}"
    fi
  done
  if [ -n "$missing" ]; then
    echo "error: assigned gate \"${assigned}\" is missing conditions for: ${missing}" >&2
    exit 1
  fi
  echo "warning: proceeding with the assigned built-in gate \"${assigned}\" — it covers all four metrics"
fi

# 4. Print the final assigned gate definition verbatim.
final="$(api_get "qualitygates/show?${ORGQ}&name=$(jq -rn --arg n "$assigned" '$n|@uri')")"
echo ""
echo "Final gate definition (\"${assigned}\"):"
jq -S . <<<"$final"
