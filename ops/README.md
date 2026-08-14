# Perch operations pack

This directory contains a versioned, deployable starting point for operating a
Perch server fleet. It intentionally uses only metrics emitted by
`perch-daemon`; application-specific SLOs can be layered on top.

## Prometheus

Load [`prometheus/perch-rules.yml`](prometheus/perch-rules.yml) with the
Prometheus `rule_files` setting. The file contains a small set of recording
rules plus alerts for:

- loss of the Perch metric surface;
- HTTP errors, admission overload, sustained worker-transport backlog, active
  transport cancellation, and deadline violations;
- unhealthy activation, worker crash loops, shard failures, and cgroup OOMs;
- durable-queue age, connection-pool saturation, dead letters, and storage
  errors;
- failed rollback and artifact collection.

The queue-age warning defaults to 60 seconds and the HTTP-error warning to 5%
over five minutes. Those are safe initial signals, not universal product SLOs;
override them in an environment-owned rule file when a workload has a different
latency or backlog budget.

Every alert links to the matching procedure in
[`RUNBOOK.md`](RUNBOOK.md). Configure Alertmanager in the deployment
environment to page the primary on-call for `severity="critical"` and notify
the service owner for `severity="warning"`; repository configuration cannot
know the fleet's receiver names or credentials. Preserve alert labels through
routing and never add request bodies, queue payloads, or credentials.

If `promtool` is installed, validate the rules with:

```sh
promtool check rules ops/prometheus/perch-rules.yml
promtool test rules ops/prometheus/perch-rules.test.yml
```

The synthetic suite checks representative ratio/volume gates, `for` hold
durations, counter increases, label retention, and expected annotations. Add a
case whenever an alert expression or threshold changes.

## Grafana

Import [`grafana/perch-overview.json`](grafana/perch-overview.json) or place it
in a Grafana dashboard provisioning directory. Select the Prometheus data
source, then filter by deployment and queue. The dashboard queries raw Perch
metrics, so it works even when the optional recording rules are not installed.
It includes framed worker-protocol byte throughput and bounded poison/cancel
reason breakdowns; partial frames are represented as transport failures rather
than estimated byte counts.

## Repository validation

Run:

```sh
node ops/validate.mjs
```

The validator parses the dashboard, checks panel IDs and PromQL-bearing fields,
requires a unique repository runbook target and matching procedure for every
alert, enforces the complete bounded label-key inventory in
[`metric-label-policy.json`](metric-label-policy.json), and fails when either
operations file references a `perch_*` metric that the daemon does not emit.
Public extension HTTP methods are collapsed into `method="OTHER"`; request IDs,
message IDs, paths, hosts, error strings, runtime/package identities, and PIDs
are forbidden as metric labels. Prometheus remains the authority for full
PromQL and rule syntax/behavior validation.

Measure the two direct request-path metric updates in an optimized build with:

```sh
cargo test --release -p perch-daemon --bin perch \
  metrics::tests::request_metric_hot_path_cost -- \
  --ignored --nocapture --test-threads=1
```

The probe performs 10,000 warmups and 500,000 counter-plus-histogram updates.
It fails above one microsecond per request by default; set
`PERCH_METRICS_MAX_NS_PER_REQUEST` only when enforcing a stricter
environment-specific regression budget.

## Live provisioning smoke test

[`smoke-live.sh`](smoke-live.sh) starts a finite local metric fixture, a real
Prometheus server, and a real Grafana server in a temporary directory. It
proves all 22 rules load, the fixture is scraped/queryable, the dashboard is
provisioned under UID `perch-server-overview`, and Grafana reports the
`perch-prometheus` datasource healthy:

```sh
PROMETHEUS_BIN=/path/to/prometheus \
GRAFANA_BIN=/path/to/grafana \
GRAFANA_HOME=/path/to/share/grafana \
  ops/smoke-live.sh
```

The three ports default to fixture `19101`, Prometheus `19090`, and Grafana
`13000`; override them with `PERCH_SMOKE_METRICS_PORT`,
`PERCH_SMOKE_PROMETHEUS_PORT`, and `PERCH_SMOKE_GRAFANA_PORT`. The harness
binds only to loopback, enables anonymous Grafana viewing only in the temporary
process, prints component log tails on failure, and stops/removes all temporary
state on exit. Alertmanager receivers remain an environment-owned deployment
concern.
