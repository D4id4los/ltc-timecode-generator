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
const PROJECT = process.env.SONAR_PROJECT ?? 'D4id4los_ltc-timecode-generator';
const TOKEN = process.env.SONAR_TOKEN;

if (!TOKEN) {
  console.error('error: SONAR_TOKEN is not set (create one at sonarcloud.io → My Account → Security)');
  process.exit(1);
}

const GATE = 'LTC gate';
const CONDITIONS = [
  {metric: 'new_coverage', op: 'GT', error: '80'},
  {metric: 'new_reliability_rating', op: 'LT', error: '2'},
  {metric: 'new_security_rating', op: 'LT', error: '2'},
  {metric: 'new_maintainability_rating', op: 'LT', error: '2'},
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
const list = await get('qualitygates/list');
let gateMeta = list.qualitygates.find((g) => g.name === GATE);
if (!gateMeta) {
  gateMeta = await post(`qualitygates/create?name=${encodeURIComponent(GATE)}`);
  console.log(`created gate "${GATE}" (id ${gateMeta.id})`);
} else {
  console.log(`gate "${GATE}" already exists (id ${gateMeta.id})`);
}

// 2. Ensure every condition is present (matched by metric+op+error).
// create_condition takes gateName on SonarCloud today; older wrappers
// wanted the gate id — retry with id on a "missing parameter" error.
let shown = await get(`qualitygates/show?name=${encodeURIComponent(GATE)}`);
for (const c of CONDITIONS) {
  const present = shown.conditions.some(
    (x) => x.metric === c.metric && x.op === c.op && String(x.error) === c.error);
  if (present) {
    console.log(`condition present: ${c.metric} ${c.op} ${c.error}`);
    continue;
  }
  const qs = `metric=${c.metric}&op=${c.op}&error=${c.error}`;
  try {
    await post(`qualitygates/create_condition?gateName=${encodeURIComponent(GATE)}&${qs}`);
  } catch (e) {
    if (/missing parameter/i.test(e.responseBody ?? '')) {
      await post(`qualitygates/create_condition?gateId=${gateMeta.id}&${qs}`);
    } else {
      throw e;
    }
  }
  console.log(`condition added: ${c.metric} ${c.op} ${c.error}`);
  shown = await get(`qualitygates/show?name=${encodeURIComponent(GATE)}`);
}

// 3. Assign the gate to the project.
await post(`qualitygates/select?projectKey=${encodeURIComponent(PROJECT)}&gateName=${encodeURIComponent(GATE)}`);
console.log(`gate "${GATE}" assigned to ${PROJECT}`);

// 4. Print the final gate definition verbatim.
const final = await get(`qualitygates/show?name=${encodeURIComponent(GATE)}`);
console.log('\nFinal gate definition:');
console.log(JSON.stringify(final, null, 2));
