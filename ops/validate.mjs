#!/usr/bin/env node

import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const repository = resolve(here, "..");
const dashboardPath = join(here, "grafana", "perch-overview.json");
const rulesPath = join(here, "prometheus", "perch-rules.yml");
const runbookPath = join(here, "RUNBOOK.md");
const labelPolicyPath = join(here, "metric-label-policy.json");
const metricsPath = join(repository, "crates", "perch-daemon", "src", "metrics.rs");

const [dashboardSource, rulesSource, runbookSource, labelPolicySource, metricsSource] = await Promise.all([
  readFile(dashboardPath, "utf8"),
  readFile(rulesPath, "utf8"),
  readFile(runbookPath, "utf8"),
  readFile(labelPolicyPath, "utf8"),
  readFile(metricsPath, "utf8"),
]);

let dashboard;
try {
  dashboard = JSON.parse(dashboardSource);
} catch (error) {
  throw new Error(`invalid Grafana dashboard JSON: ${error.message}`);
}

if (!dashboard.title || !Array.isArray(dashboard.panels) || dashboard.panels.length === 0) {
  throw new Error("Grafana dashboard must have a title and at least one panel");
}

let labelPolicy;
try {
  labelPolicy = JSON.parse(labelPolicySource);
} catch (error) {
  throw new Error(`invalid metric label policy JSON: ${error.message}`);
}

const panelIds = dashboard.panels.map((panel) => panel.id);
if (panelIds.some((id) => !Number.isInteger(id)) || new Set(panelIds).size !== panelIds.length) {
  throw new Error("Grafana panel IDs must be unique integers");
}

const sourceMetrics = new Set(metricsSource.match(/perch_[a-z0-9_]+/g) ?? []);
const operationSources = `${dashboardSource}\n${rulesSource}`;
const referencedMetrics = new Set(operationSources.match(/perch_[a-z0-9_]+/g) ?? []);
const unknownMetrics = [];

for (const metric of referencedMetrics) {
  if (sourceMetrics.has(metric)) continue;
  const histogramBase = metric.replace(/_(bucket|sum|count)$/, "");
  if (histogramBase !== metric && sourceMetrics.has(histogramBase)) continue;
  unknownMetrics.push(metric);
}

if (unknownMetrics.length > 0) {
  throw new Error(`operations files reference metrics not emitted by perch-daemon: ${unknownMetrics.sort().join(", ")}`);
}

const queryCount = dashboard.panels.reduce(
  (count, panel) => count + (Array.isArray(panel.targets) ? panel.targets.length : 0),
  0,
);
if (queryCount === 0) {
  throw new Error("Grafana dashboard does not contain any metric queries");
}

const alerts = [...rulesSource.matchAll(/^\s*- alert: ([A-Za-z][A-Za-z0-9]+)\s*$/gm)].map(
  (match) => match[1],
);
if (alerts.length === 0 || new Set(alerts).size !== alerts.length) {
  throw new Error("Prometheus alerts must exist and have unique names");
}
for (const alert of alerts) {
  const anchor = alert.toLowerCase();
  if (!rulesSource.includes(`runbook: ops/RUNBOOK.md#${anchor}`)) {
    throw new Error(`alert ${alert} is missing its repository runbook annotation`);
  }
  if (!runbookSource.includes(`## ${alert}\n`)) {
    throw new Error(`runbook is missing the ${alert} procedure`);
  }
}


const labelMatches = [
  ...metricsSource.matchAll(/"([a-z][a-z0-9_]*)"\s*=>/g),
  ...metricsSource.matchAll(/\(\s*"([a-z][a-z0-9_]*)"\s*,/g),
];
const emittedLabelKeys = new Set(
  labelMatches.map((match) => match[1]).filter((label) => !label.startsWith("perch_")),
);
const documentedLabelKeys = new Set(Object.keys(labelPolicy.labels ?? {}));
const undocumentedLabels = [...emittedLabelKeys].filter((label) => !documentedLabelKeys.has(label));
const stalePolicyLabels = [...documentedLabelKeys].filter((label) => !emittedLabelKeys.has(label));
if (undocumentedLabels.length > 0 || stalePolicyLabels.length > 0) {
  throw new Error(
    `metric label policy mismatch; undocumented=${undocumentedLabels.sort().join(",") || "none"}; stale=${stalePolicyLabels.sort().join(",") || "none"}`,
  );
}
for (const forbidden of labelPolicy.forbidden_label_keys ?? []) {
  if (emittedLabelKeys.has(forbidden)) {
    throw new Error(`forbidden high-cardinality metric label emitted: ${forbidden}`);
  }
}

console.log(
  `validated ${dashboard.panels.length} Grafana panels, ${queryCount} panel queries, ${alerts.length} alert runbooks, ${emittedLabelKeys.size} bounded label keys, and ${referencedMetrics.size} emitted Perch metrics`,
);
