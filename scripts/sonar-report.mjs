#!/usr/bin/env node
// Deterministic SonarQube Cloud report exporter.
//
// Pulls quality-gate status, measures, issues and security hotspots from the
// SonarQube Cloud Web API and writes fixed-name JSON + Markdown files to
// reports/sonar/, so a run is fully reproducible: same project state in,
// same files out.  Feeds those files to an AI agent or diffs them over time.
//
// Usage:
//   SONAR_TOKEN=<token> npm run sonar:report
// Environment:
//   SONAR_TOKEN   required — user token from sonarcloud.io (My Account → Security)
//   SONAR_HOST    optional — defaults to https://sonarcloud.io
//   SONAR_ORG     optional — defaults to d4id4los
//   SONAR_PROJECT optional — defaults to D4id4los_ltc-timecode-generator

import {mkdirSync, writeFileSync} from 'node:fs';
import {resolve} from 'node:path';

const HOST = process.env.SONAR_HOST ?? 'https://sonarcloud.io';
const ORG = process.env.SONAR_ORG ?? 'd4id4los';
const PROJECT = process.env.SONAR_PROJECT ?? 'D4id4los_ltc-timecode-generator';
const TOKEN = process.env.SONAR_TOKEN;

if (!TOKEN) {
  console.error('error: SONAR_TOKEN is not set (create one at sonarcloud.io → My Account → Security)');
  process.exit(1);
}

const OUT_DIR = resolve('reports/sonar');
const PAGE_SIZE = 500;

async function api(path) {
  const res = await fetch(`${HOST}/api/${path}`, {
    headers: {Authorization: `Bearer ${TOKEN}`},
  });
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    console.error(`error: GET ${path} → HTTP ${res.status}\n${body}`);
    process.exit(1);
  }
  return res.json();
}

async function paginated(path, listKey) {
  const items = [];
  for (let page = 1; ; page++) {
    const sep = path.includes('?') ? '&' : '?';
    const data = await api(`${path}${sep}ps=${PAGE_SIZE}&p=${page}`);
    items.push(...(data[listKey] ?? []));
    const total = data.paging?.total ?? items.length;
    if (items.length >= total || page > 200) break;
  }
  return items;
}

const RATING = {1: 'A', 2: 'B', 3: 'C', 4: 'D', 5: 'E'};
const SEVERITY_ORDER = ['INFO', 'MINOR', 'MAJOR', 'CRITICAL', 'BLOCKER'];

mkdirSync(OUT_DIR, {recursive: true});

// 1. Quality gate
const qg = await api(`qualitygates/project_status?projectKey=${PROJECT}`);
writeFileSync(resolve(OUT_DIR, 'quality-gate.json'), JSON.stringify(qg, null, 2) + '\n');

// 2. Key measures — the server rejects unknown metric keys, so strip any
// it names and retry instead of failing the whole report.
const METRICS = [
  'ncloc', 'coverage', 'duplicated_lines_density', 'complexity',
  'cognitive_complexity', 'bugs', 'code_smells', 'vulnerabilities',
  'security_hotspots', 'reliability_rating', 'security_rating',
  'security_review_rating', 'maintainability_rating', 'sqale_rating',
];
let metricKeys = [...METRICS];
let measures;
for (;;) {
  const res = await fetch(`${HOST}/api/measures/component?component=${PROJECT}&metricKeys=${metricKeys.join(',')}`, {
    headers: {Authorization: `Bearer ${TOKEN}`},
  });
  if (res.ok) {
    measures = await res.json();
    break;
  }
  const body = await res.text().catch(() => '');
  const notFound = body.match(/not found: ([^"}]+)/)?.[1]?.split(',').map((s) => s.trim());
  if (notFound?.length) {
    console.warn(`warning: dropping unknown metrics: ${notFound.join(', ')}`);
    metricKeys = metricKeys.filter((k) => !notFound.includes(k));
    if (metricKeys.length === 0) {
      console.error('error: no valid metrics left to request');
      process.exit(1);
    }
    continue;
  }
  console.error(`error: GET measures/component → HTTP ${res.status}\n${body}`);
  process.exit(1);
}
writeFileSync(resolve(OUT_DIR, 'measures.json'), JSON.stringify(measures, null, 2) + '\n');

// 3. Open issues (all severities, main branch)
const issues = await paginated(`issues/search?componentKeys=${PROJECT}&resolved=false`, 'issues');
issues.sort((a, b) =>
  (SEVERITY_ORDER.indexOf(a.severity) - SEVERITY_ORDER.indexOf(b.severity))
  || a.component.localeCompare(b.component)
  || (a.line ?? 0) - (b.line ?? 0));
writeFileSync(resolve(OUT_DIR, 'issues.json'), JSON.stringify({total: issues.length, issues}, null, 2) + '\n');

// 4. Security hotspots
const hotspots = await paginated(`hotspots/search?projectKey=${PROJECT}`, 'hotspots');
writeFileSync(resolve(OUT_DIR, 'hotspots.json'), JSON.stringify({total: hotspots.length, hotspots}, null, 2) + '\n');

// 5. Human-readable summary
const m = Object.fromEntries(measures.component.measures.map((x) => [x.metric, x.value]));
// NONE = no gate assigned (cannot fail); OK = passed; else failed, listing
// the error conditions.
const gate = qg.projectStatus.status === 'OK'
  ? 'PASSED'
  : qg.projectStatus.status === 'NONE'
    ? 'NO GATE DEFINED'
    : `FAILED (${qg.projectStatus.conditions?.filter((c) => c.status === 'ERROR').map((c) => c.metricKey).join(', ')})`;
const rating = (v) => RATING[Number(v)] ?? '?';
const bySeverity = {};
for (const i of issues) bySeverity[i.severity] = (bySeverity[i.severity] ?? 0) + 1;

const rel = (key) => key.replace(`${PROJECT}:`, '');
const lines = [];
lines.push(`# SonarQube Cloud report — ${PROJECT}`);
lines.push(`\nGenerated: ${new Date().toISOString()}  |  Branch: main`);
lines.push(`\n## Quality gate: ${gate}`);
lines.push('\n## Measures');
lines.push('| Metric | Value |');
lines.push('|---|---|');
lines.push(`| Lines of code | ${m.ncloc ?? '?'} |`);
lines.push(`| Coverage | ${m.coverage ?? 'n/a'}% |`);
lines.push(`| Duplication | ${m.duplicated_lines_density ?? '?'}% |`);
lines.push(`| Cyclomatic complexity | ${m.complexity ?? '?'} |`);
lines.push(`| Cognitive complexity | ${m.cognitive_complexity ?? '?'} |`);
lines.push(`| Bugs / Code smells / Vulns | ${m.bugs ?? 0} / ${m.code_smells ?? 0} / ${m.vulnerabilities ?? 0} |`);
lines.push(`| Ratings (Reliability/Security/Maintainability) | ${rating(m.reliability_rating)} / ${rating(m.security_rating)} / ${rating(m.maintainability_rating ?? m.sqale_rating)} |`);
lines.push('\n### New-code conditions');
const conditions = qg.projectStatus.conditions ?? [];
if (conditions.length === 0) {
  lines.push('\n(no gate assigned — the portal cannot fail on anything)');
} else {
  lines.push('\n| Metric | Comparator | Error threshold | Status | Actual |');
  lines.push('|---|---|---|---|---|');
  for (const c of conditions) {
    const actual = c.actualValue ?? '-';
    lines.push(`| ${c.metricKey} | ${c.comparator} | ${c.errorThreshold} | ${c.status} | ${actual} |`);
  }
}
lines.push(`\n## Open issues: ${issues.length}`);
for (const s of [...SEVERITY_ORDER].reverse()) {
  if (bySeverity[s]) lines.push(`- ${s}: ${bySeverity[s]}`);
}
lines.push('\n| Severity | Rule | Location | Message |');
lines.push('|---|---|---|---|');
for (const i of issues.slice(0, 200)) {
  const msg = (i.message ?? '').replaceAll('|', '\\|');
  lines.push(`| ${i.severity} | ${i.rule} | \`${rel(i.component)}:${i.line ?? '-'}\` | ${msg} |`);
}
if (issues.length > 200) lines.push(`\n… ${issues.length - 200} more in issues.json`);
lines.push(`\n## Security hotspots: ${hotspots.length} (details in hotspots.json)`);
writeFileSync(resolve(OUT_DIR, 'summary.md'), lines.join('\n') + '\n');

console.log(`Sonar report written to ${OUT_DIR}/`);
console.log(`  quality-gate: ${gate}  issues: ${issues.length}  hotspots: ${hotspots.length}`);
