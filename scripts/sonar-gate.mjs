#!/usr/bin/env node
// One-shot SonarCloud quality-gate setup (idempotent — safe to re-run).
//
// Creates (or finds) the "LTC gate" quality gate, ensures it carries the
// new-code conditions below, assigns it to the project, and prints the
// final gate definition verbatim for eyeball verification.
//
// Usage:
//   SONAR_TOKEN=<token> node scripts/sonar-gate.mjs
// Environment:
//   SONAR_TOKEN   required — user token from sonarcloud.io (My Account → Security)
//   SONAR_HOST    optional — defaults to https://sonarcloud.io
//   SONAR_ORG     optional — defaults to d4id4los
//   SONAR_PROJECT optional — defaults to D4id4los_ltc-timecode-generator
//
// If the API shapes have drifted (create_condition/select parameter names,
// rating op/error encodings), the documented fallback is the ~10-minute
// portal path: SonarCloud → Quality Gates → copy "Sonar way" → edit
// conditions → set as the project's gate.

const HOST = process.env.SONAR_HOST ?? 'https://sonarcloud.io';
const ORG = process.env.SONAR_ORG ?? 'd4id4los';
const PROJECT = process.env.SONAR_PROJECT ?? 'D4id4los_ltc-timecode-generator';
const TOKEN = process.env.SONAR_TOKEN;
// SonarCloud rejects qualitygates/* calls without an organization parameter.
const ORGQ = `organization=${encodeURIComponent(ORG)}`;

if (!TOKEN) {
  console.error('error: SONAR_TOKEN is not set (create one at sonarcloud.io → My Account → Security)');
  process.exit(1);
}

const GATE = 'LTC gate';
// Expressed the way the API wants it (a condition FAILS when op holds):
// coverage fails when below 80% (LT 80); ratings fail when worse than A
// (GT 1) — i.e. coverage > 80% and rating A on new code.
const CONDITIONS = [
  {metric: 'new_coverage', op: 'LT', error: '80'},
  {metric: 'new_reliability_rating', op: 'GT', error: '1'},
  {metric: 'new_security_rating', op: 'GT', error: '1'},
  {metric: 'new_maintainability_rating', op: 'GT', error: '1'},
];

async function call(method, path) {
  const res = await fetch(`${HOST}/api/${path}`, {
    method,
    headers: {Authorization: `Bearer ${TOKEN}`},
  });
  const body = await res.text().catch(() => '');
  if (!res.ok) {
    const err = new Error(`${method} ${path} → HTTP ${res.status}\n${body}`);
    err.status = res.status;
    err.responseBody = body;
    throw err;
  }
  return body ? JSON.parse(body) : {};
}

async function get(path) {
  return call('GET', path);
}

async function post(path) {
  return call('POST', path);
}

// 1. Find the gate by name, create it if absent.
const list = await get(`qualitygates/list?${ORGQ}`);
let gateMeta = list.qualitygates.find((g) => g.name === GATE);
if (!gateMeta) {
  gateMeta = await post(`qualitygates/create?${ORGQ}&name=${encodeURIComponent(GATE)}`);
  console.log(`created gate "${GATE}" (id ${gateMeta.id})`);
} else {
  console.log(`gate "${GATE}" already exists (id ${gateMeta.id})`);
}

// 2. Ensure every condition is present (matched by metric+op+error).
// create_condition takes gateName on SonarCloud today; older wrappers
// wanted the gate id — retry with id on a "missing parameter" error.
let shown = await get(`qualitygates/show?${ORGQ}&name=${encodeURIComponent(GATE)}`);
for (const c of CONDITIONS) {
  const present = shown.conditions.some(
    (x) => x.metric === c.metric && x.op === c.op && String(x.error) === c.error);
  if (present) {
    console.log(`condition present: ${c.metric} ${c.op} ${c.error}`);
    continue;
  }
  const qs = `metric=${c.metric}&op=${c.op}&error=${c.error}`;
  await post(`qualitygates/create_condition?${ORGQ}&gateId=${gateMeta.id}&${qs}`);
  console.log(`condition added: ${c.metric} ${c.op} ${c.error}`);
  shown = await get(`qualitygates/show?${ORGQ}&name=${encodeURIComponent(GATE)}`);
}

// 3. Assign the gate to the project. Some SonarCloud plans reject
// custom-gate assignment ("Organization … is not allowed to modify
// Quality gates", HTTP 403). In that case fall back to verifying that
// the already-assigned gate covers all four condition metrics — the
// built-in "Sonar way" does (it is a strict superset: it also enforces
// new-code duplication < 3% and 100% hotspots reviewed).
let assigned = GATE;
try {
  await post(`qualitygates/select?${ORGQ}&projectKey=${encodeURIComponent(PROJECT)}&gateId=${gateMeta.id}`);
  console.log(`gate "${GATE}" assigned to ${PROJECT}`);
} catch (e) {
  if (e.status !== 403) throw e;
  console.warn(`warning: assigning the custom gate failed (${e.responseBody.split('\n')[0]})`);
  const cur = await get(`qualitygates/get_by_project?${ORGQ}&project=${encodeURIComponent(PROJECT)}`);
  assigned = cur.qualityGate?.name;
  const assignedShow = await get(`qualitygates/show?${ORGQ}&name=${encodeURIComponent(assigned)}`);
  const metrics = new Set(assignedShow.conditions.map((c) => c.metric));
  const missing = CONDITIONS.filter((c) => !metrics.has(c.metric));
  if (missing.length > 0) {
    console.error(`error: assigned gate "${assigned}" is missing conditions for: ${missing.map((c) => c.metric).join(', ')}`);
    process.exit(1);
  }
  console.warn(`warning: proceeding with the assigned built-in gate "${assigned}" — it covers all four metrics`);
}

// 4. Print the final assigned gate definition verbatim.
const final = await get(`qualitygates/show?${ORGQ}&name=${encodeURIComponent(assigned)}`);
console.log(`\nFinal gate definition ("${assigned}"):`);
console.log(JSON.stringify(final, null, 2));
